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

use super::decimal::Decimal;
use super::request::{BinanceRequest, Exposure, Method, OkxRequest};
use super::rules::{FreshRules, SymbolRules, TradeProduct};
use super::types::TradeError;
use crate::account::{AccountEnv, AccountKey, AccountProduct, AccountVenue};
use crate::rate_limit::Priority;
use std::cmp::Ordering;
use std::fmt;

/// A request built by one of the builders.
#[derive(Clone)]
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
    /// Locked by the local risk state (segment e); new entries only.
    Locked,
}

/// Local risk controls (segment e plugs its checks in here). Only entries are checked:
/// reducing, closing and cancelling are always allowed (condition 72).
pub trait RiskHook {
    fn check(&self, view: &RiskView<'_>) -> Result<(), RiskRefusal>;
}

/// The default hook: a cap on the value of one new order (PRD 3.3, condition 63). With no
/// price to value the order, an entry is refused rather than waved through.
#[derive(Debug, Clone, Copy)]
pub struct SingleOrderCap {
    pub max_notional: Decimal,
}

impl RiskHook for SingleOrderCap {
    fn check(&self, view: &RiskView<'_>) -> Result<(), RiskRefusal> {
        let Exposure::Adds { notional } = view.exposure else {
            return Ok(());
        };
        let Some(value) = notional else {
            return Err(RiskRefusal::NotionalUnknown);
        };
        match value.checked_cmp(&self.max_notional) {
            Ok(Ordering::Greater) | Err(_) => Err(RiskRefusal::SingleOrderCapExceeded),
            Ok(_) => Ok(()),
        }
    }
}

/// The user's confirmation of one order. Single use: `submit` takes it by value, so a second
/// confirmation of the same order needs a second token (condition 61). The confirmation
/// window (segment c) is the only place that should call [`ConfirmToken::user_confirmed`].
#[derive(Debug)]
pub struct ConfirmToken(());

impl ConfirmToken {
    pub fn user_confirmed() -> Self {
        ConfirmToken(())
    }

    pub(crate) fn spend(self) -> Spent {
        Spent(())
    }
}

/// A confirmation that has been used up. Only [`ConfirmToken::spend`] (a user confirmation) or
/// [`Spent::pre_authorised`] (an automatic path) makes one. `Clone` so one confirmation can
/// cover the several requests of a cancel-all or close-all.
#[derive(Debug, Clone)]
pub(crate) struct Spent(());

impl Spent {
    /// For the three automatic paths, which `AutoIntent` already restricts to reducing.
    pub(crate) fn pre_authorised() -> Self {
        Spent(())
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
}

impl AutoIntent {
    pub fn new(path: AutoPath, intent: OrderIntent) -> Result<Self, TradeError> {
        match intent.exposure() {
            Exposure::Adds { .. } => Err(TradeError::AutoPathMustReduce),
            Exposure::Reduces | Exposure::Neutral => Ok(AutoIntent { path, intent }),
        }
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
    /// The request does not carry the fields the send path needs.
    Malformed(TradeError),
    /// The intent was built for another account than the client that was asked to send it.
    AccountMismatch,
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
    _confirmation: Spent,
    intent: OrderIntent,
    now_ms: i64,
    target: Target,
) -> Result<ApprovedOrder, SubmitError<G::Refusal>> {
    target.check(&intent)?;
    let exposure = intent.exposure();
    let class = match exposure {
        Exposure::Adds { .. } => GateClass::Entry,
        Exposure::Reduces | Exposure::Neutral => GateClass::Reduce,
    };
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
