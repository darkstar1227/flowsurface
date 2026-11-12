//! Replacing a protective order (f3a): the exchange side of dragging a stop or a take-profit.
//!
//! The exchanges have no in-place amend, so a replace is a cancel of the old order and a place
//! of a new one. One user confirmation, for the new order's preview and the old order's id,
//! covers both legs, through a [`ReplaceGrant`]:
//!
//! - the grant is made from that confirmation ([`ConfirmToken::for_replace`], spent in
//!   [`TradeClient::replace_grant`]) and from the old order's row. The confirmation binds the
//!   hash of the new request's preview and a fingerprint of the old order's row (exchange id,
//!   kind, side, position side, size, whole-position flag); the grant refuses an old row that
//!   differs in any of them. The grant then checks the new request against that row (kind
//!   stop or target, side, position side, size) and is bound to the account and the symbol.
//!   An order confirmation (`for_preview`) is not a replace confirmation, and the reverse;
//! - the cancel is built **by the grant** from the old id, so the grant cannot cancel anything
//!   but the order the user confirmed;
//! - [`TradeClient::submit_replace`] takes the grant by value (single use), approves **both**
//!   legs first (grant, expiry, account, stream, F2 gate, risk, rules age), and only then
//!   sends. It cancels first; the new order goes out only if the exchange confirmed the cancel
//!   (`Outcome::Accepted`). A refused, rejected (including "not found" because the order
//!   already triggered or filled), unknown, partial or unsent cancel voids the grant, and no
//!   new order is sent.
//!
//! Approving both legs before the cancel leaves means that a refusal of the new order that can
//! be predicted (stream state, gate, risk, rules age) never happens after the old order is gone.
//! What cannot be predicted (the exchange refusing the new order) leaves the position without
//! that protection; that is [`ReplaceError::NewNotPlaced`], which the caller must show loudly.
//!
//! The new order must be a protective order of the same kind (stop for stop, target for target),
//! on the same side and position side, closing and not adding, and not larger than the old one.
//! Only a stop or a target on Binance USDⓈ-M or OKX swap can be replaced; trailing stops,
//! orders carrying both legs and entries cannot.
//!
//! ```compile_fail,E0599
//! use flowsurface_exchange::trade::ReplaceGrant;
//! fn twice(g: &ReplaceGrant) -> ReplaceGrant { g.clone() }
//! ```

use super::binance_futures;
use super::client::{NotSent, OpenKind, OpenOrder, Submitted, TradeClient};
use super::decimal::Decimal;
use super::gate::{
    ApprovedOrder, ConfirmToken, OrderIntent, Prepared, ReplaceLeg, RiskHook, SendGate, Spent,
    SubmitError, approve,
};
use super::journal::UnknownJournal;
use super::okx::{self, OkxAlgoType};
use super::outcome::Outcome;
use super::preview::PreviewHash;
use super::protect::ProtectRole;
use super::request::{BinanceRequest, Exposure, Method, OkxRequest};
use super::rules::{FreshRules, TradeProduct};
use super::types::{Dir, OrderRef, Side};
use crate::account::AccountKey;
use std::cmp::Ordering;
use std::fmt;

/// How long a replace grant lives after the confirmation: the confirmation window hands the
/// grant straight to `submit_replace`, so half a minute is for a slow gate or limiter, not for
/// waiting. The new order's rules age (`valid_until_ms`) bounds the send as well.
pub const REPLACE_GRANT_VALID_MS: i64 = 30_000;

const BINANCE_ALGO_PATH: &str = "/fapi/v1/algoOrder";
const OKX_ALGO_PATH: &str = "/api/v5/trade/order-algo";
const OKX_CANCEL_ALGOS_PATH: &str = "/api/v5/trade/cancel-algos";

/// Which protective order is replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceKind {
    Stop,
    Target,
}

/// Why a replace was not made or not sent. Nothing was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceRefusal {
    /// The confirmation is not a replace confirmation.
    NotAReplaceConfirmation,
    /// Not a stop or a take-profit on Binance USDⓈ-M or OKX swap (a trailing stop, an order
    /// with both legs, a plain order or a spot order).
    NotReplaceable,
    /// The old order's row lacks what the check needs (side, size) or its id is not usable.
    OldOrderUnreadable,
    WrongAccount,
    WrongSymbol,
    /// The grant expired before the send started.
    Expired,
    /// The new request is not the one the user confirmed.
    NewOrderMismatch,
    /// The new request is not a protective order of the old order's kind.
    WrongKind,
    WrongSide,
    WrongPositionSide,
    /// The new order does not only close.
    NotReducing,
    /// The new order covers more than the old one, or a whole position where the old one
    /// covered a part (or the reverse).
    SizeChanged,
    /// The new order reuses the old order's client id.
    SameClientId,
    /// The cancel request is not the one the grant built.
    CancelMismatch,
    /// The confirmation was for another old order, or for the same order as its row read
    /// then (id, kind, side, position side, size), than the old order row given.
    OldOrderMismatch,
}

impl fmt::Display for ReplaceRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ReplaceRefusal::NotAReplaceConfirmation => "the confirmation is not for a replace",
            ReplaceRefusal::NotReplaceable => "this order cannot be replaced",
            ReplaceRefusal::OldOrderUnreadable => "the old order could not be read completely",
            ReplaceRefusal::WrongAccount => "the grant belongs to another account",
            ReplaceRefusal::WrongSymbol => "the grant belongs to another symbol",
            ReplaceRefusal::Expired => "the confirmation expired",
            ReplaceRefusal::NewOrderMismatch => "the new order is not the one confirmed",
            ReplaceRefusal::WrongKind => "the new order is not of the old order's kind",
            ReplaceRefusal::WrongSide => "the new order is not on the old order's side",
            ReplaceRefusal::WrongPositionSide => "the position side does not match",
            ReplaceRefusal::NotReducing => "the new order does not only close",
            ReplaceRefusal::SizeChanged => "the new order covers another size",
            ReplaceRefusal::SameClientId => "the new order reuses the old client id",
            ReplaceRefusal::CancelMismatch => "the cancel is not the one the grant built",
            ReplaceRefusal::OldOrderMismatch => "the confirmation was for another old order row",
        })
    }
}

/// Why `submit_replace` did not complete. Which variant it is says what is on the exchange:
/// [`ReplaceError::NewNotPlaced`] means the old order is gone; [`ReplaceError::CancelNotConfirmed`]
/// with an `Accepted` or an `Unknown` outcome means it may be gone, and the open orders must be
/// read again.
#[derive(Debug)]
pub enum ReplaceError<R> {
    /// The grant or the requests did not fit. Nothing was sent; the old order stands.
    Refused(ReplaceRefusal),
    /// The gate, risk, stream or rules check refused a leg before anything was sent. Nothing
    /// was sent; the old order stands.
    NotSent(SubmitError<R>),
    /// The cancel was sent and the exchange did not confirm it for the old order (rejected,
    /// which includes "no such order" after a trigger or fill; unknown; partial; accepted
    /// without the old order's id in the answer; or it never left). The grant is void and no
    /// new order was sent. For `Unknown`, and for `Accepted` (the exchange cancelled some
    /// order, most likely the old one), the old order may be gone: read the open orders again
    /// and treat the position as possibly unprotected.
    CancelNotConfirmed(Box<Submitted>),
    /// The old order **was cancelled** and the new one was not placed (rejected, unknown,
    /// or it never left). The position lacks this protection until the user acts; for an
    /// `Unknown` outcome `new.pending` resolves it.
    NewNotPlaced {
        cancel: Box<Submitted>,
        new: Option<Box<Submitted>>,
    },
}

/// Both legs of a finished replace.
#[derive(Debug)]
pub struct ReplaceDone {
    pub cancel: Submitted,
    pub new: Submitted,
    /// What the automatic protection of the app needs to take the new order over from the old
    /// one (f3c). `None` when the replace is not one an automatic grant can follow (OKX).
    pub replacement: Option<Replacement>,
}

/// The proof that a confirmed replace of a stop or a target **completed**: the exchange
/// accepted the cancel of the old order (naming it) and accepted the new order. Only
/// [`TradeClient::submit_replace`] makes one, and only on that path, so it cannot be built by a
/// caller; its fields are private. [`AutoGrant::adopt_replacement`](super::grant::AutoGrant)
/// takes it to let an automatic grant that issued the old order cancel the new one instead.
///
/// It is a record of a completed fact, not a permission: it is `Clone` (so a summary that
/// carries it can be), and adopting twice is refused because the old id is gone after the
/// first time. It names client ids, so `Debug` prints nothing of them.
#[derive(Clone, PartialEq, Eq)]
pub struct Replacement {
    pub(crate) account: AccountKey,
    pub(crate) symbol: String,
    pub(crate) kind: ReplaceKind,
    pub(crate) old_client_id: String,
    pub(crate) new_client_id: String,
}

impl fmt::Debug for Replacement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Replacement(<redacted>)")
    }
}

impl Replacement {
    /// The client id of the old order, to find the entry that placed it.
    pub fn old_client_id(&self) -> &str {
        &self.old_client_id
    }

    pub fn new_client_id(&self) -> &str {
        &self.new_client_id
    }

    /// From the grant and the new request, before either is spent. Binance only (an automatic
    /// grant exists only there), with one client id on each side.
    fn of(grant: &ReplaceGrant, new: &OrderIntent) -> Option<Self> {
        let Prepared::Binance(r) = &new.prepared else {
            return None;
        };
        let ids = r.client_ids();
        let [new_client_id] = ids.as_slice() else {
            return None;
        };
        Some(Replacement {
            account: grant.account,
            symbol: grant.symbol.clone(),
            kind: grant.kind,
            old_client_id: grant.old_client_id.clone()?,
            new_client_id: (*new_client_id).to_string(),
        })
    }

    /// For the tests of crates that depend on this one.
    #[cfg(feature = "test-support")]
    pub fn for_test(
        account: AccountKey,
        symbol: &str,
        kind: ReplaceKind,
        old_client_id: &str,
        new_client_id: &str,
    ) -> Self {
        Replacement {
            account,
            symbol: symbol.to_string(),
            kind,
            old_client_id: old_client_id.to_string(),
            new_client_id: new_client_id.to_string(),
        }
    }
}

/// The one confirmation of a replace. Not `Clone`, taken by value by `submit_replace`.
pub struct ReplaceGrant {
    account: AccountKey,
    symbol: String,
    product: TradeProduct,
    old_id: String,
    old_client_id: Option<String>,
    kind: ReplaceKind,
    side: Side,
    position_side: Option<Dir>,
    old_qty: Option<Decimal>,
    old_closes_position: bool,
    /// The hash of the new request's preview, from the user's confirmation.
    new_hash: PreviewHash,
    /// The cancel of the old order, built here, and its hash.
    cancel: OrderIntent,
    cancel_hash: PreviewHash,
    expires_ms: i64,
}

impl fmt::Debug for ReplaceGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReplaceGrant(<redacted>)")
    }
}

impl ReplaceGrant {
    /// Made when the user confirms the replace, from [`TradeClient::replace_grant`] (which
    /// supplies the client's clock). `token` is [`ConfirmToken::for_replace`] for the new
    /// order's preview and the old order, `old` the open-order row being replaced (it must be
    /// the order the token names), `fresh` the rules of its symbol. The cancel of the old
    /// order is built here.
    pub(super) fn new(
        token: ConfirmToken,
        account: AccountKey,
        old: &OpenOrder,
        fresh: &FreshRules<'_>,
        now_ms: i64,
    ) -> Result<Self, ReplaceRefusal> {
        let (kind, side, intent) = read_old(account, old, fresh)?;
        let rules = fresh.rules();
        let (new_hash, confirmed_old) = token
            .spend_for_replace()
            .map_err(|_| ReplaceRefusal::NotAReplaceConfirmation)?;
        // The user confirmed cancelling this order, as this row showed it, and no other.
        if !confirmed_old.matches(old) {
            return Err(ReplaceRefusal::OldOrderMismatch);
        }
        let cancel_hash = intent.preview().hash;
        Ok(ReplaceGrant {
            account,
            symbol: rules.symbol().as_str().to_string(),
            product: rules.product(),
            old_id: old.id.clone(),
            old_client_id: old.client_id.clone(),
            kind,
            side,
            position_side: old.position_side,
            old_qty: old.qty,
            old_closes_position: old.close_position,
            new_hash,
            cancel: intent,
            cancel_hash,
            expires_ms: now_ms.saturating_add(REPLACE_GRANT_VALID_MS),
        })
    }

    /// The cancel of `old` as the grant will send it, without a token: for the confirmation
    /// window to list next to the new order **before** the user confirms (and before
    /// [`ConfirmToken::for_replace`] is made). Refuses what a grant would refuse about the old
    /// order.
    pub fn cancel_preview_for(
        account: AccountKey,
        old: &OpenOrder,
        fresh: &FreshRules<'_>,
    ) -> Result<super::preview::OrderPreview, ReplaceRefusal> {
        read_old(account, old, fresh).map(|(_, _, intent)| intent.preview())
    }

    pub fn kind(&self) -> ReplaceKind {
        self.kind
    }

    /// The cancel request as it will be sent, for the confirmation window to list next to the
    /// new order.
    pub fn cancel_preview(&self) -> super::preview::OrderPreview {
        self.cancel.preview()
    }

    /// The grant covers `new` and the cancel it built, at `now_ms`.
    fn check(&self, new: &OrderIntent, now_ms: i64) -> Result<(), ReplaceRefusal> {
        if now_ms > self.expires_ms {
            return Err(ReplaceRefusal::Expired);
        }
        if new.account() != self.account || self.cancel.account() != self.account {
            return Err(ReplaceRefusal::WrongAccount);
        }
        if new.symbol() != self.symbol {
            return Err(ReplaceRefusal::WrongSymbol);
        }
        if new.preview().hash != self.new_hash {
            return Err(ReplaceRefusal::NewOrderMismatch);
        }
        if !matches!(new.exposure(), Exposure::Reduces) {
            return Err(ReplaceRefusal::NotReducing);
        }
        self.check_cancel()?;
        match &new.prepared {
            Prepared::Binance(r) if self.product == TradeProduct::BinanceUsdM => {
                self.check_binance(r)
            }
            Prepared::Okx(r) if self.product == TradeProduct::OkxSwap => self.check_okx(r),
            _ => Err(ReplaceRefusal::NotReplaceable),
        }
    }

    /// The cancel is the one request the grant built: neutral, one order, the old id.
    fn check_cancel(&self) -> Result<(), ReplaceRefusal> {
        let ok = self.cancel.preview().hash == self.cancel_hash
            && matches!(self.cancel.exposure(), Exposure::Neutral)
            && match &self.cancel.prepared {
                Prepared::Binance(r) => {
                    r.method() == Method::Delete
                        && r.path() == BINANCE_ALGO_PATH
                        && r.param("algoId") == Some(self.old_id.as_str())
                }
                Prepared::Okx(r) => {
                    r.method() == Method::Post
                        && r.path() == OKX_CANCEL_ALGOS_PATH
                        && matches!(
                            r.cancel_algo_items(),
                            Some([one]) if one.algo_id.as_deref() == Some(self.old_id.as_str())
                                && one.inst_id == self.symbol
                        )
                }
            };
        if ok {
            Ok(())
        } else {
            Err(ReplaceRefusal::CancelMismatch)
        }
    }

    fn check_binance(&self, r: &BinanceRequest) -> Result<(), ReplaceRefusal> {
        if r.method() != Method::Post || r.path() != BINANCE_ALGO_PATH {
            return Err(ReplaceRefusal::NotReplaceable);
        }
        let kind = match r.param("type") {
            Some("STOP_MARKET" | "STOP") => ReplaceKind::Stop,
            Some("TAKE_PROFIT_MARKET" | "TAKE_PROFIT") => ReplaceKind::Target,
            _ => return Err(ReplaceRefusal::WrongKind),
        };
        if kind != self.kind {
            return Err(ReplaceRefusal::WrongKind);
        }
        if r.param("side") != Some(self.side.binance()) {
            return Err(ReplaceRefusal::WrongSide);
        }
        if !matches!(
            (self.position_side, r.param("positionSide")),
            (None, None | Some("BOTH"))
                | (Some(Dir::Long), Some("LONG"))
                | (Some(Dir::Short), Some("SHORT"))
        ) {
            return Err(ReplaceRefusal::WrongPositionSide);
        }
        if r.client_ids()
            .iter()
            .any(|c| Some(*c) == self.old_client_id.as_deref())
        {
            return Err(ReplaceRefusal::SameClientId);
        }
        let closes = r.param("closePosition") == Some("true");
        // The builder and the exchange both keep a reduce-only order from adding; a request
        // with neither flag is refused here.
        if !closes && r.param("reduceOnly") != Some("true") {
            return Err(ReplaceRefusal::NotReducing);
        }
        let qty = r.param("quantity").and_then(|v| Decimal::parse(v).ok());
        self.check_size(closes, qty)
    }

    fn check_okx(&self, r: &OkxRequest) -> Result<(), ReplaceRefusal> {
        if r.method() != Method::Post || r.path() != OKX_ALGO_PATH {
            return Err(ReplaceRefusal::NotReplaceable);
        }
        let b = r.algo_body().ok_or(ReplaceRefusal::NotReplaceable)?;
        if b.ord_type != "conditional" {
            return Err(ReplaceRefusal::WrongKind);
        }
        let kind = match (b.sl_trigger_px.is_some(), b.tp_trigger_px.is_some()) {
            (true, false) => ReplaceKind::Stop,
            (false, true) => ReplaceKind::Target,
            _ => return Err(ReplaceRefusal::WrongKind),
        };
        if kind != self.kind {
            return Err(ReplaceRefusal::WrongKind);
        }
        if b.side != self.side.okx() {
            return Err(ReplaceRefusal::WrongSide);
        }
        if !matches!(
            (self.position_side, b.pos_side),
            (None, None | Some("net"))
                | (Some(Dir::Long), Some("long"))
                | (Some(Dir::Short), Some("short"))
        ) {
            return Err(ReplaceRefusal::WrongPositionSide);
        }
        if Some(b.algo_cl_ord_id.as_str()) == self.old_client_id.as_deref() {
            return Err(ReplaceRefusal::SameClientId);
        }
        let closes = b.close_fraction.is_some();
        // Net mode closes with reduceOnly, hedge mode with the position side.
        if !closes && b.reduce_only != Some(true) && b.pos_side.is_none_or(|p| p == "net") {
            return Err(ReplaceRefusal::NotReducing);
        }
        let qty = b.sz.as_deref().and_then(|v| Decimal::parse(v).ok());
        self.check_size(closes, qty)
    }

    /// The new order covers what the old one did: a whole position for a whole position, and
    /// otherwise no more than the old quantity.
    fn check_size(&self, closes: bool, qty: Option<Decimal>) -> Result<(), ReplaceRefusal> {
        if closes != self.old_closes_position {
            return Err(ReplaceRefusal::SizeChanged);
        }
        if closes {
            return Ok(());
        }
        let (Some(new), Some(old)) = (qty, self.old_qty) else {
            return Err(ReplaceRefusal::SizeChanged);
        };
        match new.checked_cmp(&old) {
            Ok(Ordering::Less | Ordering::Equal) if new.is_positive() => Ok(()),
            _ => Err(ReplaceRefusal::SizeChanged),
        }
    }
}

/// The old order's row, read for a replace: its kind, side and the cancel request built from
/// its id. Everything that makes the old order unreplaceable is refused here.
fn read_old(
    account: AccountKey,
    old: &OpenOrder,
    fresh: &FreshRules<'_>,
) -> Result<(ReplaceKind, Side, OrderIntent), ReplaceRefusal> {
    let rules = fresh.rules();
    let product = rules.product();
    if !matches!(product, TradeProduct::BinanceUsdM | TradeProduct::OkxSwap) {
        return Err(ReplaceRefusal::NotReplaceable);
    }
    // Only a conditional stop or take-profit; a trailing stop, an order with both legs and
    // anything that is not conditional are refused.
    let kind = match (&old.kind, old.role) {
        (OpenKind::Algo(_), Some(ProtectRole::Stop)) => ReplaceKind::Stop,
        (OpenKind::Algo(_), Some(ProtectRole::Target)) => ReplaceKind::Target,
        _ => return Err(ReplaceRefusal::NotReplaceable),
    };
    if old.symbol != rules.symbol().as_str() {
        return Err(ReplaceRefusal::WrongSymbol);
    }
    let side = old.side.ok_or(ReplaceRefusal::OldOrderUnreadable)?;
    if !old.close_position && old.qty.is_none() {
        return Err(ReplaceRefusal::OldOrderUnreadable);
    }
    let by_id = OrderRef::exchange(&old.id).map_err(|_| ReplaceRefusal::OldOrderUnreadable)?;
    let intent = match product {
        TradeProduct::BinanceUsdM => {
            let r = binance_futures::cancel_algo_order(&by_id)
                .map_err(|_| ReplaceRefusal::OldOrderUnreadable)?;
            OrderIntent::binance(account, r, fresh)
        }
        _ => {
            // The type of the algo order is needed to cancel it; a one-leg order is a
            // `conditional`.
            let OpenKind::Algo(Some(OkxAlgoType::Conditional)) = old.kind else {
                return Err(ReplaceRefusal::NotReplaceable);
            };
            let r = okx::cancel_algos(rules, OkxAlgoType::Conditional, &[by_id])
                .map_err(|_| ReplaceRefusal::OldOrderUnreadable)?;
            OrderIntent::okx(account, r, fresh)
        }
    }
    .map_err(|_| ReplaceRefusal::OldOrderUnreadable)?;
    Ok((kind, side, intent))
}

fn not_sent<R>(e: NotSent) -> ReplaceError<R> {
    ReplaceError::NotSent(match e {
        NotSent::Locked => SubmitError::Locked,
        NotSent::Journal => SubmitError::JournalUnavailable,
    })
}

impl TradeClient {
    /// The grant for replacing `old`, made when the user confirms. The expiry runs on this
    /// client's clock; a caller cannot supply a time. See [`ReplaceGrant::new`] for the rest.
    pub fn replace_grant(
        &self,
        token: ConfirmToken,
        account: AccountKey,
        old: &OpenOrder,
        fresh: &FreshRules<'_>,
    ) -> Result<ReplaceGrant, ReplaceRefusal> {
        ReplaceGrant::new(token, account, old, fresh, self.now_ms())
    }

    /// Replace a protective order: cancel the old one, then place the new one, under one
    /// confirmation (see the module notes). The grant is spent here, so a second call needs a
    /// second confirmation.
    ///
    /// Both legs pass the whole gate before the cancel is sent. The new order is sent only
    /// after the cancel came back `Outcome::Accepted`; any other cancel result returns
    /// [`ReplaceError::CancelNotConfirmed`] and nothing else is sent. Neither leg is ever
    /// repeated.
    pub async fn submit_replace<G: SendGate>(
        &self,
        gate: &G,
        risk: &dyn RiskHook,
        journal: &dyn UnknownJournal,
        grant: ReplaceGrant,
        new: OrderIntent,
    ) -> Result<ReplaceDone, ReplaceError<G::Refusal>> {
        let now = self.now_ms();
        grant.check(&new, now).map_err(ReplaceError::Refused)?;
        let new_hash = new.preview().hash;
        let replacement = Replacement::of(&grant, &new);
        let ReplaceGrant {
            cancel,
            cancel_hash,
            old_id,
            ..
        } = grant;
        let target = self.target();
        // Approve both legs first, so a refusal that can be foreseen cannot come after the
        // old order is gone. The cancel is approved first: a refused cancel is a plain refusal.
        let cancel: ApprovedOrder = approve(
            gate,
            risk,
            Spent::replace(ReplaceLeg::Cancel, cancel_hash),
            cancel,
            now,
            target,
        )
        .map_err(ReplaceError::NotSent)?;
        let place: ApprovedOrder = approve(
            gate,
            risk,
            Spent::replace(ReplaceLeg::Place, new_hash),
            new,
            now,
            target,
        )
        .map_err(ReplaceError::NotSent)?;

        log::info!("replace protective order: cancelling the old order");
        let cancelled = self
            .send_approved(cancel, journal)
            .await
            .map_err(not_sent::<G::Refusal>)?;
        // Accepted counts only when the exchange names the old order: OKX answers `sCode 0`
        // without an id when the reply is malformed, and that is not a confirmed cancel.
        let cancel_confirmed = matches!(
            &cancelled.outcome,
            Outcome::Accepted { ids } if ids.contains(&old_id)
        );
        if !cancel_confirmed {
            // Rejected (also "no such order"), unknown, partial, unsent or an accepted reply
            // that does not name the old order: the grant is void.
            return Err(ReplaceError::CancelNotConfirmed(Box::new(cancelled)));
        }
        log::info!("replace protective order: placing the new order");
        match self.send_approved(place, journal).await {
            Ok(sub) if matches!(sub.outcome, Outcome::Accepted { .. }) => Ok(ReplaceDone {
                cancel: cancelled,
                new: sub,
                replacement,
            }),
            Ok(sub) => Err(ReplaceError::NewNotPlaced {
                cancel: Box::new(cancelled),
                new: Some(Box::new(sub)),
            }),
            Err(_) => Err(ReplaceError::NewNotPlaced {
                cancel: Box::new(cancelled),
                new: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trade::rules::fixtures::futures;

    fn row() -> OpenOrder {
        OpenOrder {
            symbol: "BTCUSDT".to_string(),
            id: "777".to_string(),
            kind: OpenKind::Algo(None),
            side: Some(Side::Sell),
            position_side: None,
            qty: None,
            trigger_price: None,
            close_position: true,
            reduce_only: false,
            role: Some(ProtectRole::Stop),
            attached: None,
            client_id: None,
            limit: false,
            limit_price: None,
            activation_price: None,
            callback_rate: None,
            price: None,
        }
    }

    #[test]
    fn a_cancel_of_another_order_is_not_the_grants_cancel() {
        let t = futures();
        let fresh = t.fresh_lookup("BTCUSDT", 2_000, 60_000).unwrap();
        let account = AccountKey::from_bytes([9; 16]);
        let place = binance_futures::cancel_algo_order(&OrderRef::exchange("1").unwrap()).unwrap();
        let new = OrderIntent::binance(account, place, &fresh).unwrap();
        let mut grant = ReplaceGrant::new(
            ConfirmToken::for_replace(&new.preview(), &row()),
            account,
            &row(),
            &fresh,
            2_000,
        )
        .unwrap();
        assert_eq!(grant.check_cancel(), Ok(()));
        // The same endpoint, another order id.
        let other =
            binance_futures::cancel_algo_order(&OrderRef::exchange("778").unwrap()).unwrap();
        grant.cancel = OrderIntent::binance(account, other, &fresh).unwrap();
        assert_eq!(grant.check_cancel(), Err(ReplaceRefusal::CancelMismatch));
        // A cancel of a plain order is not the algo cancel either.
        let plain = binance_futures::cancel_order(
            t.lookup("BTCUSDT").unwrap(),
            &OrderRef::exchange("777").unwrap(),
        )
        .unwrap();
        grant.cancel = OrderIntent::binance(account, plain, &fresh).unwrap();
        assert_eq!(grant.check_cancel(), Err(ReplaceRefusal::CancelMismatch));
    }

    #[test]
    fn the_debug_of_a_replace_result_hides_the_client_id() {
        let unknown = || Submitted {
            outcome: Outcome::Unknown {
                client_id: Some("eSECRETID1".to_string()),
                http_status: Some(500),
            },
            pending: None,
            journal_failed: false,
        };
        let e: ReplaceError<()> = ReplaceError::CancelNotConfirmed(Box::new(unknown()));
        let n: ReplaceError<()> = ReplaceError::NewNotPlaced {
            cancel: Box::new(unknown()),
            new: Some(Box::new(unknown())),
        };
        let done = ReplaceDone {
            cancel: unknown(),
            new: unknown(),
            replacement: None,
        };
        for text in [format!("{e:?}"), format!("{n:?}"), format!("{done:?}")] {
            assert!(!text.contains("eSECRETID1"), "{text}");
            assert!(text.contains("Unknown"), "{text}");
        }
    }
}
