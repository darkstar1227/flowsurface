//! The single gate in front of every send (condition 72).
//!
//! Every order, cancel or close goes through [`approve`]'s two public wrappers on
//! `TradeClient` (`submit`, `submit_auto`). They run, in order: the F2 gate ([`SendGate`]),
//! the local risk hook ([`RiskHook`]), the confirmation (a spent [`ConfirmToken`], or one of
//! the three pre-authorised [`AutoPath`]s), and only then mint an [`ApprovedOrder`], the one
//! thing the send functions accept.
//!
//! The F2 gate lives in the `data` crate, which this crate cannot depend on, so it is injected
//! as a trait. The weakness that remains: any implementation of [`SendGate`] can approve, so
//! the app must wire exactly one (data's `SessionGate`). `ApprovedOrder` itself cannot be
//! forged or copied:
//!
//! ```compile_fail,E0451
//! use flowsurface_exchange::trade::{ApprovedOrder, OrderIntent};
//! // The fields are crate-private: there is no way to build one outside this crate.
//! fn forge(intent: OrderIntent) -> ApprovedOrder { ApprovedOrder { intent } }
//! ```
//!
//! ```compile_fail,E0599
//! use flowsurface_exchange::trade::ApprovedOrder;
//! // No `Clone`: an approval is used once.
//! fn twice(a: &ApprovedOrder) -> ApprovedOrder { a.clone() }
//! ```
//!
//! ```compile_fail,E0624
//! // The send function is crate-private: it cannot be called without going through `submit`.
//! use flowsurface_exchange::trade::{ApprovedOrder, TradeClient};
//! async fn go(c: &TradeClient, a: ApprovedOrder) { let _ = c.send_approved(a).await; }
//! ```

use super::client::{OpenKind, OpenOrder};
use super::decimal::Decimal;
use super::preview::{OrderPreview, PreviewHash};
use super::protect::ProtectRole;
use super::request::{BinanceRequest, Exposure, Method, OkxRequest};
use super::rules::{FreshRules, SymbolRules, TradeProduct};
use super::stream::{StreamState, TradeAction, allows, stream_state_for};
use super::types::{Dir, Side, TradeError};
use crate::account::{AccountEnv, AccountKey, AccountProduct, AccountVenue};
use crate::rate_limit::Priority;
use std::fmt;

/// A request built by one of the builders. Not `Clone`: a request carries its client id, and
/// the same id must not go out twice (condition 61).
pub(crate) enum Prepared {
    Binance(BinanceRequest),
    Okx(Box<OkxRequest>),
}

impl Prepared {
    fn exposure(&self) -> Exposure {
        match self {
            Prepared::Binance(r) => r.exposure(),
            Prepared::Okx(r) => r.exposure(),
        }
    }

    fn symbol(&self) -> Option<&str> {
        match self {
            Prepared::Binance(r) => r.symbol(),
            Prepared::Okx(r) => r.symbol(),
        }
    }

    fn method(&self) -> Method {
        match self {
            Prepared::Binance(r) => r.method(),
            Prepared::Okx(r) => r.method(),
        }
    }

    fn is_symbolless_algo_cancel(&self) -> bool {
        matches!(self, Prepared::Binance(r)
            if r.method() == Method::Delete && r.path() == "/fapi/v1/algoOrder")
    }
}

/// A built request together with the fresh rules it was built from. The class of the order
/// (adds, reduces, cancel) comes from the builder, not from the caller.
pub struct OrderIntent {
    account: AccountKey,
    pub(crate) prepared: Prepared,
    pub(crate) rules: SymbolRules,
    valid_until_ms: i64,
    pub(crate) symbol: String,
}

impl fmt::Debug for OrderIntent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OrderIntent(<redacted>)")
    }
}

impl OrderIntent {
    pub fn binance(
        account: AccountKey,
        request: BinanceRequest,
        fresh: &FreshRules<'_>,
    ) -> Result<Self, TradeError> {
        let rules = fresh.rules();
        let path_ok = match rules.product() {
            TradeProduct::BinanceUsdM => request.path().starts_with("/fapi/"),
            TradeProduct::BinanceSpot => request.path().starts_with("/api/"),
            _ => false,
        };
        if !path_ok {
            return Err(TradeError::ProductMismatch);
        }
        Self::new(account, Prepared::Binance(request), fresh)
    }

    pub fn okx(
        account: AccountKey,
        request: OkxRequest,
        fresh: &FreshRules<'_>,
    ) -> Result<Self, TradeError> {
        if !matches!(
            fresh.rules().product(),
            TradeProduct::OkxSpot | TradeProduct::OkxSwap
        ) {
            return Err(TradeError::ProductMismatch);
        }
        Self::new(account, Prepared::Okx(Box::new(request)), fresh)
    }

    fn new(
        account: AccountKey,
        prepared: Prepared,
        fresh: &FreshRules<'_>,
    ) -> Result<Self, TradeError> {
        // Reads do not need approval and never go through the gate.
        if prepared.method() == Method::Get {
            return Err(TradeError::Unsupported);
        }
        let rules = fresh.rules();
        // The request must be about the symbol whose rules it was checked with. The one
        // exception is cancelling a USDⓈ-M algo order by id, whose endpoint takes no symbol:
        // it is bound to the rules' symbol here, and it can only cancel.
        let symbol_ok = match prepared.symbol() {
            Some(s) => s == rules.symbol().as_str(),
            None => prepared.is_symbolless_algo_cancel(),
        };
        if !symbol_ok {
            return Err(TradeError::UnknownSymbol);
        }
        Ok(OrderIntent {
            account,
            symbol: rules.symbol().as_str().to_string(),
            rules: rules.clone(),
            valid_until_ms: fresh.valid_until_ms(),
            prepared,
        })
    }

    pub fn exposure(&self) -> Exposure {
        self.prepared.exposure()
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub fn account(&self) -> AccountKey {
        self.account
    }

    /// A contract (futures / swap) order: needs the exchange-reported position mode. Derived
    /// from the product the rules belong to (11c), never passed in by the caller.
    pub fn contract(&self) -> bool {
        matches!(
            self.rules.product(),
            TradeProduct::BinanceUsdM | TradeProduct::OkxSwap
        )
    }

    pub fn client_ids(&self) -> Vec<&str> {
        match &self.prepared {
            Prepared::Binance(r) => r.client_ids(),
            Prepared::Okx(r) => r.client_ids(),
        }
    }

    /// The request as it will be sent, for the confirmation window (condition 69): the values
    /// are the builder's, after rounding. Read-only; give it to
    /// [`ConfirmToken::for_preview`] when the user confirms.
    pub fn preview(&self) -> OrderPreview {
        let (method, path, fields) = match &self.prepared {
            Prepared::Binance(r) => (r.method(), r.path(), r.preview_fields()),
            Prepared::Okx(r) => (r.method(), r.path(), r.preview_fields()),
        };
        OrderPreview::build(
            self.account,
            self.rules.product(),
            method,
            path,
            &self.symbol,
            self.exposure(),
            fields,
        )
    }
}

// ── Gate, risk, confirmation ────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateClass {
    /// Adds exposure.
    Entry,
    /// Closes, reduces or cancels.
    Reduce,
}

/// What the gate is asked. `account`, `venue`, `env` and `product` are filled in by the
/// `TradeClient` that will send the order, from its own fields, never by the caller (B2): the
/// gate must judge the account and environment the request will really be signed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GateRequest {
    pub account: AccountKey,
    pub venue: AccountVenue,
    pub env: AccountEnv,
    pub product: AccountProduct,
    pub class: GateClass,
    pub contract: bool,
}

/// The account, venue, environment and product a `TradeClient` signs for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) account: AccountKey,
    pub(crate) venue: AccountVenue,
    pub(crate) env: AccountEnv,
    pub(crate) product: AccountProduct,
}

impl Target {
    /// The intent must be for this account and for rules of this venue and product.
    pub(crate) fn check<R>(&self, intent: &OrderIntent) -> Result<(), SubmitError<R>> {
        if intent.account != self.account {
            return Err(SubmitError::AccountMismatch);
        }
        let expected = match (self.venue, self.product) {
            (AccountVenue::Binance, AccountProduct::UsdM) => TradeProduct::BinanceUsdM,
            (AccountVenue::Binance, AccountProduct::Spot) => TradeProduct::BinanceSpot,
            (AccountVenue::Okx, AccountProduct::Spot) => TradeProduct::OkxSpot,
            (AccountVenue::Okx, AccountProduct::UsdM) => TradeProduct::OkxSwap,
        };
        if intent.rules.product() != expected {
            return Err(SubmitError::Malformed(TradeError::ProductMismatch));
        }
        Ok(())
    }
}

/// The F2 gate (`data::config::trade_gate::can_send`, wrapped by the app).
pub trait SendGate {
    type Refusal: fmt::Debug;
    fn check(&self, request: GateRequest) -> Result<(), Self::Refusal>;
}

/// What the risk hook sees of an order.
#[derive(Debug, Clone, Copy)]
pub struct RiskView<'a> {
    pub account: AccountKey,
    pub symbol: &'a str,
    pub exposure: Exposure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskRefusal {
    /// A cap is set but the order value could not be computed (no price known).
    NotionalUnknown,
    SingleOrderCapExceeded,
    /// The position after this order would exceed the per-symbol cap.
    SymbolCapExceeded,
    /// A per-symbol cap is set but the position is unknown or stale.
    PositionUnknown,
    /// Equity is unknown or stale while an equity limit is set: no new entries.
    EquityUnavailable,
    /// Locked by the local risk state (a limit tripped); new entries only.
    Locked,
    /// The saved risk settings of the account are invalid: no entries until fixed.
    SettingsInvalid,
    /// The hook belongs to another account than the order.
    WrongAccount,
    /// The account's private stream sent a new snapshot that the risk tracker has not applied
    /// yet: its positions and equity are from before the reconnect.
    SnapshotNotApplied,
    /// The shared risk state is unusable (its lock was poisoned): no entries.
    StateUnavailable,
}

/// Local risk controls (`risk::RiskSnapshot` is the implementation the app wires). Only
/// entries are checked: reducing, closing and cancelling are always allowed (condition 72).
///
/// Sealed: only this crate can implement it, and the one production implementation is
/// [`RiskSnapshot`](super::risk::RiskSnapshot), which reads the account's `EquityTracker`. A
/// hook that always says yes cannot be written outside the crate (review item 7).
///
/// ```compile_fail,E0603
/// use flowsurface_exchange::trade::{RiskHook, RiskRefusal, RiskView};
/// struct Yes;
/// // `Sealed` is not nameable here, so the bound cannot be met.
/// impl flowsurface_exchange::trade::gate::sealed::Sealed for Yes {}
/// impl RiskHook for Yes {
///     fn check(&self, _: &RiskView<'_>) -> Result<(), RiskRefusal> { Ok(()) }
/// }
/// ```
pub trait RiskHook: sealed::Sealed + Sync {
    fn check(&self, view: &RiskView<'_>) -> Result<(), RiskRefusal>;
}

pub(crate) mod sealed {
    pub trait Sealed {}
}

/// The old order's row as the user saw it when confirming a replace: the exchange id, and the
/// fields the grant checks the new order against. A grant made from a row that differs in any
/// of them is refused, so the confirmation covers the whole row and not only its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OldRow {
    id: String,
    kind: OpenKind,
    role: Option<ProtectRole>,
    side: Option<Side>,
    position_side: Option<Dir>,
    qty: Option<Decimal>,
    close_position: bool,
}

impl OldRow {
    /// `old` is the row this fingerprint was made from.
    pub(crate) fn matches(&self, old: &OpenOrder) -> bool {
        *self == OldRow::of(old)
    }

    fn of(old: &OpenOrder) -> Self {
        OldRow {
            id: old.id.clone(),
            kind: old.kind.clone(),
            role: old.role,
            side: old.side,
            position_side: old.position_side,
            qty: old.qty,
            close_position: old.close_position,
        }
    }
}

/// What a confirmation covers.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Binding {
    /// One request, by the hash of its preview.
    Order(PreviewHash),
    /// A replace of a protective order: the new request by the hash of its preview, and the
    /// old order's row (id, kind, side, position side, size) the user saw being cancelled. Not an
    /// order confirmation and not accepted by `submit`, and an order confirmation is not
    /// accepted for a replace.
    Replace { new: PreviewHash, old: OldRow },
    /// A cancel-all or a close-all of one account: the requests are built from the account's
    /// open orders and positions when it runs, so there is no single request to preview.
    Bulk(AccountKey, BulkAction),
}

/// Which bulk action a [`ConfirmToken::for_bulk`] confirmation is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkAction {
    CancelAll,
    Flatten,
}

/// The user's confirmation. Single use: `submit` takes it by value, so a second confirmation
/// of the same order needs a second token (condition 61). It is bound to what the user saw: a
/// token from [`ConfirmToken::for_preview`] is accepted only for the request that preview came
/// from, and not for a cancel-all or close-all (and the reverse). The confirmation window
/// (segment c) is the only place that should make one.
///
/// ```compile_fail,E0599
/// use flowsurface_exchange::trade::ConfirmToken;
/// // There is no unbound token.
/// let _ = ConfirmToken::user_confirmed();
/// ```
#[derive(Debug)]
pub struct ConfirmToken(Binding);

impl ConfirmToken {
    /// The user confirmed the order shown in `preview`.
    pub fn for_preview(preview: &OrderPreview) -> Self {
        ConfirmToken(Binding::Order(preview.hash))
    }

    /// The user confirmed replacing `old` (cancel it) by the order shown in `new`. The token
    /// keeps the new request's hash and a fingerprint of the old order's row (id, kind, side,
    /// position side, size, whole-position flag); it is accepted only by
    /// [`ReplaceGrant`](super::ReplaceGrant) for that old order, and not by `submit`.
    pub fn for_replace(new: &OrderPreview, old: &OpenOrder) -> Self {
        ConfirmToken(Binding::Replace {
            new: new.hash,
            old: OldRow::of(old),
        })
    }

    /// The user confirmed `action` for `account`. The token is refused for any other account
    /// or the other bulk action.
    pub fn for_bulk(account: AccountKey, action: BulkAction) -> Self {
        ConfirmToken(Binding::Bulk(account, action))
    }

    /// For `submit`: only an order confirmation counts.
    pub(crate) fn spend_order(self) -> Result<Spent, ConfirmationMismatch> {
        match self.0 {
            Binding::Order(h) => Ok(Spent(Kind::Order(h))),
            Binding::Bulk(..) | Binding::Replace { .. } => Err(ConfirmationMismatch),
        }
    }

    /// For a [`ReplaceGrant`](super::ReplaceGrant): only a replace confirmation counts, and
    /// what it keeps is the hash of the new request and the old order's row the user saw.
    pub(crate) fn spend_for_replace(self) -> Result<(PreviewHash, OldRow), ConfirmationMismatch> {
        match self.0 {
            Binding::Replace { new, old } => Ok((new, old)),
            Binding::Order(_) | Binding::Bulk(..) => Err(ConfirmationMismatch),
        }
    }

    /// For `cancel_all` and `flatten`: only a bulk confirmation for this account and this
    /// action counts.
    pub(crate) fn spend_bulk(
        self,
        account: AccountKey,
        action: BulkAction,
    ) -> Result<Spent, ConfirmationMismatch> {
        match self.0 {
            Binding::Bulk(a, act) if a == account && act == action => Ok(Spent(Kind::Bulk)),
            Binding::Bulk(..) | Binding::Order(_) | Binding::Replace { .. } => {
                Err(ConfirmationMismatch)
            }
        }
    }
}

/// The confirmation is not for this kind of action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConfirmationMismatch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Order(PreviewHash),
    Bulk,
    /// A request the grant authorised (`AttachProtective`, `CancelSibling`): only that path's
    /// class of request, and only the request whose hash the grant saw.
    Granted(AutoPath, PreviewHash),
    /// The risk-control close, which has no grant: anything that does not add exposure.
    Unbound,
    /// One leg of a protective-order replace (f3a): the cancel of the old order or the new
    /// order, each bound to the hash the grant holds for it.
    Replace(ReplaceLeg, PreviewHash),
}

/// The two legs of a protective-order replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplaceLeg {
    /// Cancels the old order (neutral).
    Cancel,
    /// Places the new order (reducing).
    Place,
}

/// A confirmation that has been used up. Only [`ConfirmToken::spend_order`] /
/// [`ConfirmToken::spend_bulk`] (a user confirmation), [`Spent::granted`] (a grant), the
/// replace grant or [`Spent::unbound`] (the risk close) makes one. `Clone` so one confirmation
/// can cover the several requests of a cancel-all or close-all.
#[derive(Debug, Clone)]
pub(crate) struct Spent(Kind);

impl Spent {
    /// For a request an [`AutoGrant`](super::AutoGrant) authorised; `bound` is the hash of that
    /// request, taken when the grant authorised it.
    pub(crate) fn granted(path: AutoPath, bound: PreviewHash) -> Self {
        Spent(Kind::Granted(path, bound))
    }

    /// For the risk-control close, which `AutoIntent` already restricts to not adding.
    pub(crate) fn unbound() -> Self {
        Spent(Kind::Unbound)
    }

    /// For one leg of a replace, bound to the hash the replace grant holds for that leg.
    pub(crate) fn replace(leg: ReplaceLeg, bound: PreviewHash) -> Self {
        Spent(Kind::Replace(leg, bound))
    }

    /// Does this confirmation cover `intent`? An order confirmation covers only the request it
    /// was made for; an automatic one covers only the class of request of its path (and, with a
    /// grant, only the request the grant saw), never one that adds exposure.
    fn covers(&self, intent: &OrderIntent) -> bool {
        let exposure = intent.exposure();
        match self.0 {
            Kind::Order(h) => h == intent.preview().hash,
            // A bulk confirmation never covers a request that adds exposure.
            Kind::Bulk | Kind::Unbound => !matches!(exposure, Exposure::Adds { .. }),
            Kind::Granted(path, h) => {
                let shape = match path {
                    AutoPath::AttachProtective => matches!(exposure, Exposure::Reduces),
                    AutoPath::CancelSibling => matches!(exposure, Exposure::Neutral),
                    // Not a path a grant authorises.
                    AutoPath::RiskClose => false,
                };
                shape && h == intent.preview().hash
            }
            Kind::Replace(leg, h) => {
                let shape = match leg {
                    ReplaceLeg::Cancel => matches!(exposure, Exposure::Neutral),
                    ReplaceLeg::Place => matches!(exposure, Exposure::Reduces),
                };
                shape && h == intent.preview().hash
            }
        }
    }

    /// A replace drags a protective order: condition 51 refuses it unless the stream is
    /// synchronised, for the cancel as well as for the new order.
    fn is_replace(&self) -> bool {
        matches!(self.0, Kind::Replace(..))
    }
}

/// The three non-click send paths (condition 71), pre-authorised by the entry confirmation
/// (user decision 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoPath {
    /// Risk control closes or cancels.
    RiskClose,
    /// A protective order attached after a fill.
    AttachProtective,
    /// One protective order filled, so the other is cancelled.
    CancelSibling,
}

/// An intent on one of the automatic paths. Constructing it fails when the order would add
/// exposure, so these paths can only reduce, close or cancel.
#[derive(Debug)]
pub struct AutoIntent {
    pub(crate) path: AutoPath,
    pub(crate) intent: OrderIntent,
    /// The entry and settings a placement is based on (the grant compares them).
    pub(crate) basis: Option<super::grant::Basis>,
}

impl AutoIntent {
    /// `AutoPath::RiskClose` is refused here: only the risk-close routine of this crate builds
    /// it (`AutoIntent::risk_close`), so no caller can claim the risk-control path.
    pub fn new(path: AutoPath, intent: OrderIntent) -> Result<Self, TradeError> {
        if path == AutoPath::RiskClose {
            return Err(TradeError::Unsupported);
        }
        Self::build(path, intent)
    }

    pub(crate) fn risk_close(intent: OrderIntent) -> Result<Self, TradeError> {
        Self::build(AutoPath::RiskClose, intent)
    }

    fn build(path: AutoPath, intent: OrderIntent) -> Result<Self, TradeError> {
        match intent.exposure() {
            Exposure::Adds { .. } => Err(TradeError::AutoPathMustReduce),
            Exposure::Reduces | Exposure::Neutral => Ok(AutoIntent {
                path,
                intent,
                basis: None,
            }),
        }
    }

    /// Say which entry and settings a placement is based on. The grant refuses a placement that
    /// does not match its own.
    pub fn with_basis(mut self, basis: super::grant::Basis) -> Self {
        self.basis = Some(basis);
        self
    }

    pub fn path(&self) -> AutoPath {
        self.path
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError<R> {
    /// Refused by the F2 gate.
    Gate(R),
    Risk(RiskRefusal),
    /// The rules expired between building the request and approving it.
    RulesExpired,
    /// An earlier send for this account and symbol has an unresolved outcome (condition 60).
    Locked,
    /// The record of this send could not be written ahead of it, so nothing was sent
    /// (condition 60: an unresolved send must survive a restart).
    JournalUnavailable,
    /// The private stream is not `Connected` and synchronised (or does not exist, as for
    /// Binance spot), so new entries and drags are refused (condition 51).
    StreamNotReady(StreamState),
    /// The request does not carry the fields the send path needs.
    Malformed(TradeError),
    /// The intent was built for another account than the client that was asked to send it.
    AccountMismatch,
    /// The confirmation was made for another request than this one (its preview hash differs,
    /// or it is a cancel-all / close-all confirmation). Nothing was sent.
    ConfirmationMismatch,
}

/// An order that passed the gate. No `Clone`, no public constructor: only [`approve`] makes
/// one, and the send function takes it by value.
pub struct ApprovedOrder {
    pub(crate) intent: OrderIntent,
    pub(crate) priority: Priority,
}

impl fmt::Debug for ApprovedOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApprovedOrder(<redacted>)")
    }
}

/// Run the gate: F2, risk, confirmation, rules age. The confirmation arrives as a spent
/// token, so the caller cannot reuse it.
pub(crate) fn approve<G: SendGate>(
    gate: &G,
    risk: &dyn RiskHook,
    confirmation: Spent,
    intent: OrderIntent,
    now_ms: i64,
    target: Target,
) -> Result<ApprovedOrder, SubmitError<G::Refusal>> {
    if !confirmation.covers(&intent) {
        return Err(SubmitError::ConfirmationMismatch);
    }
    target.check(&intent)?;
    let exposure = intent.exposure();
    let class = match exposure {
        Exposure::Adds { .. } => GateClass::Entry,
        Exposure::Reduces | Exposure::Neutral => GateClass::Reduce,
    };
    // Condition 51 in the gate itself, not only in the UI: an entry (a drag's new leg is an
    // entry too) needs a synchronised stream; reducing and cancelling always pass. The state is
    // read from the account's entry in the stream table, which only the stream writes.
    let stream = stream_state_for(target.account, target.venue, target.product);
    if class == GateClass::Entry && !allows(stream, TradeAction::NewEntry) {
        return Err(SubmitError::StreamNotReady(stream));
    }
    // A replace of a protective order is a drag (cancel and place again): both legs need a
    // synchronised stream, which `DragModify` says, though neither leg is an entry.
    if confirmation.is_replace() && !allows(stream, TradeAction::DragModify) {
        return Err(SubmitError::StreamNotReady(stream));
    }
    // Condition 57: a refused login or key voids the account's trade session, and it does so
    // for every order, closes included (the credentials are not usable, and a stale `Usable`
    // verdict must not outlive the failure). This reads the stream's own entry, so it holds
    // even if the consumer never handles `StreamOutput::InvalidateSession`.
    if stream == StreamState::AuthFailed {
        return Err(SubmitError::StreamNotReady(stream));
    }
    gate.check(GateRequest {
        account: target.account,
        venue: target.venue,
        env: target.env,
        product: target.product,
        class,
        contract: intent.contract(),
    })
    .map_err(SubmitError::Gate)?;
    risk.check(&RiskView {
        account: intent.account,
        symbol: &intent.symbol,
        exposure,
    })
    .map_err(SubmitError::Risk)?;
    if now_ms > intent.valid_until_ms {
        return Err(SubmitError::RulesExpired);
    }
    // Cancels and reduces ride the protective share of the budget, entries the entry share.
    let priority = match exposure {
        Exposure::Adds { .. } => Priority::Entry,
        Exposure::Reduces | Exposure::Neutral => Priority::Protective,
    };
    Ok(ApprovedOrder { intent, priority })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::PositionMode;
    use crate::trade::binance_futures as bf;
    use crate::trade::client_id::{ClientId, Role};
    use crate::trade::rules::fixtures::{d, futures};
    use crate::trade::types::{
        Action, Dir, Exec, OrderRef, Protective, ProtectiveKind, Size, TimeInForce, WorkingType,
    };

    const NOW: i64 = 2_000;

    fn key() -> AccountKey {
        AccountKey::from_bytes([7; 16])
    }

    /// An entry (adds), a protective order (reduces) and a cancel (neutral).
    fn intents() -> (OrderIntent, OrderIntent, OrderIntent) {
        let t = futures();
        let r = t.lookup("BTCUSDT").unwrap();
        let fresh = t.fresh_lookup("BTCUSDT", NOW, 60_000).unwrap();
        let entry = bf::new_order(bf::FuturesEntry {
            rules: r,
            mode: Some(PositionMode::OneWay),
            action: Action::Open(Dir::Long),
            qty: d("0.002"),
            exec: Exec::Limit {
                price: d("60000"),
                tif: TimeInForce::Gtc,
            },
            ref_price: Some(d("60000")),
            client_id: ClientId::generate(Role::Entry),
        })
        .unwrap();
        let protective = bf::new_algo_order(
            r,
            Some(PositionMode::OneWay),
            Protective {
                kind: ProtectiveKind::StopMarket,
                dir: Dir::Long,
                size: Size::ClosePosition,
                trigger_price: Some(d("59000")),
                limit_price: None,
                tif: TimeInForce::Gtc,
                working_type: WorkingType::MarkPrice,
                trailing: None,
                ref_price: d("60000"),
                client_id: ClientId::generate(Role::StopLoss),
            },
        )
        .unwrap();
        let cancel = bf::cancel_algo_order(&OrderRef::exchange("777").unwrap()).unwrap();
        let make = |r| OrderIntent::binance(key(), r, &fresh).unwrap();
        (make(entry), make(protective), make(cancel))
    }

    fn covers(spent: Spent, intent: &OrderIntent) -> bool {
        spent.covers(intent)
    }

    #[test]
    fn a_granted_confirmation_covers_only_its_path_and_its_request() {
        let (entry, protective, cancel) = intents();
        let (_, other_protective, _) = intents();
        let attach = |h| Spent::granted(AutoPath::AttachProtective, h);
        let sibling = |h| Spent::granted(AutoPath::CancelSibling, h);
        // Its own request on its own path.
        assert!(covers(attach(protective.preview().hash), &protective));
        assert!(covers(sibling(cancel.preview().hash), &cancel));
        // Not another request (another client id, so another hash).
        assert!(!covers(
            attach(protective.preview().hash),
            &other_protective
        ));
        // Not a request of the other path's class, even with a matching hash.
        assert!(!covers(attach(cancel.preview().hash), &cancel));
        assert!(!covers(sibling(protective.preview().hash), &protective));
        // Never an entry.
        assert!(!covers(attach(entry.preview().hash), &entry));
        assert!(!covers(sibling(entry.preview().hash), &entry));
        // The risk-close path is not one a grant authorises.
        assert!(!covers(
            Spent::granted(AutoPath::RiskClose, protective.preview().hash),
            &protective
        ));
    }

    #[test]
    fn the_unbound_and_bulk_confirmations_never_cover_an_entry() {
        let (entry, protective, cancel) = intents();
        for spent in [Spent::unbound(), Spent(Kind::Bulk)] {
            assert!(covers(spent.clone(), &protective));
            assert!(covers(spent.clone(), &cancel));
            assert!(!covers(spent, &entry));
        }
    }

    #[test]
    fn a_replace_leg_covers_only_its_own_request() {
        let (entry, protective, cancel) = intents();
        let place = |h| Spent::replace(ReplaceLeg::Place, h);
        let cancel_leg = |h| Spent::replace(ReplaceLeg::Cancel, h);
        assert!(covers(place(protective.preview().hash), &protective));
        assert!(covers(cancel_leg(cancel.preview().hash), &cancel));
        // The legs are not interchangeable, even with the other's hash.
        assert!(!covers(place(cancel.preview().hash), &cancel));
        assert!(!covers(cancel_leg(protective.preview().hash), &protective));
        // A hash that is not the request's.
        assert!(!covers(place(cancel.preview().hash), &protective));
        assert!(!covers(place(entry.preview().hash), &entry));
        assert!(Spent::replace(ReplaceLeg::Place, entry.preview().hash).is_replace());
        assert!(!Spent::unbound().is_replace());
    }

    #[test]
    fn an_order_confirmation_still_covers_exactly_its_request() {
        let (entry, protective, _) = intents();
        assert!(covers(Spent(Kind::Order(entry.preview().hash)), &entry));
        assert!(!covers(
            Spent(Kind::Order(entry.preview().hash)),
            &protective
        ));
    }
}
