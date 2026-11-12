//! Dragging a stop or a target line to a new price (f3a2): the decisions and the state around
//! the exchange layer's replace path (`exchange::trade::replace`). Nothing here sends anything:
//! it says whether a line may be grabbed, builds the new order's spec from the latest row, checks
//! it again when the user confirms, and turns the end of a replace into words and follow-ups.
//! The one send point is `order_flow::send_replace` (condition 51 and the f3a2 section of
//! `docs/trading-security.md`).
//!
//! The state is in memory only: a drag, a review and the failures live here and never reach the
//! saved state or the drawings library.

use crate::auto_flow::{FailReason, FailureView};
use exchange::account::{AccountProduct, AccountVenue};
use exchange::adapter::Exchange;
use exchange::trade::okx::OkxAlgoType;
use exchange::trade::stream::StreamState;
use exchange::trade::{Decimal, Dir, ManualSpec, OpenKind, OpenOrder, ProtectRole, Side, Size};
use exchange::trade::{ReplaceKind, Replacement};
use uuid::Uuid;

/// The prefix of the key of a failure made here, so that the panel's buttons find it.
pub const FAILURE_KEY_PREFIX: &str = "replace-";

// ── Why a line cannot be grabbed ───────────────────────────────────────

/// Why a stop or target line is not draggable. Each one has a sentence the chart shows on hover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragBlock {
    /// The private stream is not connected.
    Stream,
    /// The lists may not match the exchange (not synchronised, or an order event is unread).
    Stale,
    /// A Live account while live trading is closed.
    Live,
    /// A percent or log price scale: the chart cannot turn a position back into a price there.
    Scale,
    /// An automatic protection job of the app still works on this order.
    AutoJob,
    /// Binance hedge mode, a part of the position: the order has no reduce-only flag, so a new
    /// one cannot be built that only closes.
    PartialHedge,
    /// OKX: only a `conditional` order (one trigger) can be cancelled and placed again here.
    NotConditional,
    /// The order fills as a limit order once triggered: a new one would be a market order.
    LimitTrigger,
    /// A spot account, or a market this build does not replace orders on.
    Product,
    /// The row lacks a field the new order needs.
    Unreadable,
    /// Another replace of this account is being confirmed or sent.
    Busy,
}

impl DragBlock {
    pub fn text(self) -> &'static str {
        match self {
            Self::Stream => "Not draggable: the private stream is not connected.",
            Self::Stale => "Not draggable: the order list may be out of date.",
            Self::Live => "Not draggable: live trading is not open.",
            Self::Scale => "Not draggable: use the plain price scale (not percent or log).",
            Self::AutoJob => "Not draggable: automatic protection is still working on this order.",
            Self::PartialHedge => {
                "Not draggable: a part of a hedge-mode position cannot be re-placed safely."
            }
            Self::NotConditional => "Not draggable: only a one-trigger OKX order can be moved.",
            Self::LimitTrigger => "Not draggable: this order fills as a limit order.",
            Self::Product => "Not draggable: orders on this market cannot be replaced here.",
            Self::Unreadable => "Not draggable: the order could not be read completely.",
            Self::Busy => "Not draggable: another replace is in progress.",
        }
    }
}

/// What the guard needs to know about the account and the lists, built per account from the
/// trading state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guard {
    pub venue: AccountVenue,
    pub product: AccountProduct,
    /// A Live account.
    pub live: bool,
    /// Live trading is open (`LIVE_SENDING_ENABLED`; false in this build).
    pub live_open: bool,
    pub stream: Option<StreamState>,
    pub lists_stale: bool,
    /// A replace of this account is being confirmed or sent.
    pub busy: bool,
}

/// A conditional order the replace path can handle, by the row's own shape (the exchange layer
/// refuses everything else again).
pub fn candidate(row: &OpenOrder) -> Option<ReplaceKind> {
    if !matches!(row.kind, OpenKind::Algo(_)) {
        return None;
    }
    match row.role? {
        ProtectRole::Stop => Some(ReplaceKind::Stop),
        ProtectRole::Target => Some(ReplaceKind::Target),
        ProtectRole::StopAndTarget | ProtectRole::Trailing => None,
    }
}

/// The first reason the row's line cannot be dragged, or `None`. `auto_order`: an automatic
/// protection job of the app works on this order. The percent / log scale is the chart's to
/// check (it alone knows the scale).
pub fn drag_block(row: &OpenOrder, g: &Guard, auto_order: bool) -> Option<DragBlock> {
    if g.product != AccountProduct::UsdM {
        return Some(DragBlock::Product);
    }
    if g.stream != Some(StreamState::Connected) {
        return Some(DragBlock::Stream);
    }
    if g.lists_stale {
        return Some(DragBlock::Stale);
    }
    if g.live && !g.live_open {
        return Some(DragBlock::Live);
    }
    if g.busy {
        return Some(DragBlock::Busy);
    }
    if auto_order {
        return Some(DragBlock::AutoJob);
    }
    if row.limit {
        return Some(DragBlock::LimitTrigger);
    }
    match g.venue {
        AccountVenue::Binance => {
            if row.position_side.is_some() && !row.close_position {
                return Some(DragBlock::PartialHedge);
            }
        }
        AccountVenue::Okx => {
            if row.kind != OpenKind::Algo(Some(OkxAlgoType::Conditional)) {
                return Some(DragBlock::NotConditional);
            }
        }
    }
    let readable = row.side.is_some()
        && row.trigger_price.as_ref().is_some_and(Decimal::is_positive)
        && (row.close_position || row.qty.is_some());
    (!readable).then_some(DragBlock::Unreadable)
}

// ── From a drag to a spec ──────────────────────────────────────────────

/// The new order, before the exchange layer rounds and builds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub spec: ManualSpec,
    pub kind: ReplaceKind,
    /// The direction of the position the order protects.
    pub dir: Dir,
    pub old_trigger: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanRefusal {
    Blocked(DragBlock),
    /// The dragged price is the order's own trigger: nothing to do.
    Unchanged,
    /// The row is not a stop or a target of the replace path.
    NotReplaceable,
    /// The dragged price is not a usable price.
    BadPrice,
}

impl PlanRefusal {
    pub fn text(self) -> &'static str {
        match self {
            Self::Blocked(b) => b.text(),
            Self::Unchanged => "The price did not change. Nothing was done.",
            Self::NotReplaceable => "That order cannot be replaced.",
            Self::BadPrice => "That is not a usable price.",
        }
    }
}

/// The direction of the position a protective order closes: a sell protects a long.
fn protected_dir(row: &OpenOrder) -> Option<Dir> {
    row.position_side.or(match row.side? {
        Side::Sell => Some(Dir::Long),
        Side::Buy => Some(Dir::Short),
    })
}

/// The same-size protective order of the same kind at `price`, from the **latest read** of the
/// old row. The size is the old one's (the whole position, or the same quantity); the exchange
/// layer rounds the price to the symbol's tick, and checks the result against the old row again.
pub fn plan(
    row: &OpenOrder,
    price: Decimal,
    g: &Guard,
    auto_order: bool,
) -> Result<Plan, PlanRefusal> {
    let kind = candidate(row).ok_or(PlanRefusal::NotReplaceable)?;
    if let Some(b) = drag_block(row, g, auto_order) {
        return Err(PlanRefusal::Blocked(b));
    }
    if !price.is_positive() {
        return Err(PlanRefusal::BadPrice);
    }
    let old_trigger = row
        .trigger_price
        .ok_or(PlanRefusal::Blocked(DragBlock::Unreadable))?;
    if price.checked_cmp(&old_trigger) == Ok(std::cmp::Ordering::Equal) {
        return Err(PlanRefusal::Unchanged);
    }
    let dir = protected_dir(row).ok_or(PlanRefusal::Blocked(DragBlock::Unreadable))?;
    let size = if row.close_position {
        Size::ClosePosition
    } else {
        Size::Qty(row.qty.ok_or(PlanRefusal::Blocked(DragBlock::Unreadable))?)
    };
    let spec = match kind {
        ReplaceKind::Stop => ManualSpec::Stop {
            dir,
            size,
            trigger: price,
        },
        ReplaceKind::Target => ManualSpec::TakeProfit {
            dir,
            size,
            trigger: price,
        },
    };
    Ok(Plan {
        spec,
        kind,
        dir,
        old_trigger,
    })
}

/// The part of a row the confirmation was made for. Everything the exchange layer binds, and the
/// trigger the user saw.
fn same_row(a: &OpenOrder, b: &OpenOrder) -> bool {
    a.id == b.id
        && a.symbol == b.symbol
        && a.kind == b.kind
        && a.side == b.side
        && a.position_side == b.position_side
        && a.qty == b.qty
        && a.close_position == b.close_position
        && a.role == b.role
        && a.trigger_price == b.trigger_price
}

/// Why a confirmation that was reviewed on one row cannot go on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recheck {
    Blocked(DragBlock),
    /// The order is no longer in the list.
    Gone,
    /// The order is still there but differs from the one reviewed (size, side, trigger…).
    Changed,
}

impl Recheck {
    pub fn text(self) -> &'static str {
        match self {
            Self::Blocked(b) => b.text(),
            Self::Gone => "That order is no longer open. Nothing was sent.",
            Self::Changed => "That order changed after the review. Nothing was sent.",
        }
    }
}

/// When the user confirms: the order must still be in the latest list, unchanged, and every
/// condition that made it draggable must still hold. Returns the row **as read now**, which is
/// the one the exchange layer's grant is made from.
pub fn recheck<'a>(
    reviewed: &OpenOrder,
    latest: &'a [OpenOrder],
    g: &Guard,
    auto_order: impl Fn(&OpenOrder) -> bool,
) -> Result<&'a OpenOrder, Recheck> {
    let row = latest
        .iter()
        .find(|o| o.id == reviewed.id)
        .ok_or(Recheck::Gone)?;
    if let Some(b) = drag_block(row, g, auto_order(row)) {
        return Err(Recheck::Blocked(b));
    }
    if same_row(row, reviewed) {
        Ok(row)
    } else {
        Err(Recheck::Changed)
    }
}

/// Would the new trigger fire at once against the ticker price (the last price on some exchanges,
/// the mark price on others)? `Some(true)` is warned about in the confirmation, not forbidden (the
/// exchange refuses an order that cannot be placed). `None`
/// when there is no price to compare with.
pub fn would_trigger_now(
    kind: ReplaceKind,
    dir: Dir,
    trigger: &Decimal,
    mark: Option<&Decimal>,
) -> Option<bool> {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let ord = trigger.checked_cmp(mark?).ok()?;
    // A long's stop and a short's target fire when the price falls to the trigger; a short's stop
    // and a long's target when it rises to it.
    let falls = matches!(
        (kind, dir),
        (ReplaceKind::Stop, Dir::Long) | (ReplaceKind::Target, Dir::Short)
    );
    Some(if falls {
        matches!(ord, Greater | Equal)
    } else {
        matches!(ord, Less | Equal)
    })
}

// ── From a position on the chart to a price ────────────────────────────

/// The price at the height `y` of a plain (linear) price scale whose plot starts at `top`, is
/// `height` tall and shows `min..max`. `None` outside the plot or for a degenerate scale.
pub fn price_at_y(
    y: f32,
    top: f32,
    height: f32,
    min: f32,
    max: f32,
    inverted: bool,
) -> Option<f32> {
    // Written as a positive test so that a NaN anywhere fails it.
    let usable = height > 0.0 && y >= top && y <= top + height && max > min;
    if !usable {
        return None;
    }
    let from_top = (y - top) / height;
    let ratio = if inverted { from_top } else { 1.0 - from_top };
    let price = min + ratio * (max - min);
    (price.is_finite() && price > 0.0).then_some(price)
}

/// The dragged price as an exact decimal (the shortest text that reads back as the same float);
/// the exchange layer rounds it to the tick when it builds the order.
pub fn decimal_of_price(price: f32) -> Option<Decimal> {
    if !price.is_finite() || price <= 0.0 {
        return None;
    }
    Decimal::parse(&format!("{price}"))
        .ok()
        .filter(Decimal::is_positive)
}

/// A drag the chart finished, before an account is chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneReplace {
    pub market: Exchange,
    pub symbol: String,
    pub order_id: String,
    pub price: Decimal,
}

// ── What the exchange layer reported ───────────────────────────────────

/// What came back for the cancel leg when it was not a confirmed cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelEnd {
    /// The exchange refused (also "no such order": the order may have triggered or filled).
    Rejected { code: String, msg: String },
    /// No answer.
    Unknown,
    /// An answer that did not name the old order, or only a part.
    Unconfirmed,
    /// It never left this process.
    NotSent,
}

impl CancelEnd {
    /// The old order may be gone although the cancel was not confirmed.
    pub fn may_be_gone(&self) -> bool {
        matches!(self, Self::Unknown | Self::Unconfirmed)
    }
}

/// What came back for the new order after the old one was cancelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewLeg {
    Rejected {
        code: String,
        msg: String,
    },
    /// No answer at first; the lookup found the order.
    Found,
    /// No answer; the lookup never saw it. Nothing was resent.
    NotPlacedPresumed,
    /// No answer, and the lookups did not settle it.
    Unresolved,
    /// It never left this process.
    NeverSent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceResult {
    /// Both orders accepted.
    Done,
    /// Refused before anything was sent (grant, gate, risk, rules, lock). The old order stays.
    Refused(String),
    CancelNotConfirmed(CancelEnd),
    NewNotPlaced(NewLeg),
    /// Place first only (f3b): the new order was sent and not accepted. The old order was not
    /// cancelled and stands. An unresolved or found one means two orders may stand.
    PlacedFirstFailed(NewLeg),
    /// Place first only (f3b): the new order was accepted and the old one was not confirmed
    /// cancelled. Two protective orders may stand; nothing follows the new one automatically.
    OldNotCancelled(CancelEnd),
    /// The send task itself ended without an answer: nothing is known about either leg.
    Lost,
}

impl ReplaceResult {
    /// A fixed word for the log: never an order id, a client id or the exchange's own text.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Done => "both orders accepted",
            Self::Refused(_) => "refused before sending",
            Self::CancelNotConfirmed(CancelEnd::Rejected { .. }) => "cancel rejected",
            Self::CancelNotConfirmed(CancelEnd::Unknown) => "cancel unknown",
            Self::CancelNotConfirmed(CancelEnd::Unconfirmed) => "cancel unconfirmed",
            Self::CancelNotConfirmed(CancelEnd::NotSent) => "cancel not sent",
            Self::NewNotPlaced(NewLeg::Rejected { .. }) => "cancel accepted, new rejected",
            Self::NewNotPlaced(NewLeg::Found) => "cancel accepted, new found",
            Self::NewNotPlaced(NewLeg::NotPlacedPresumed) => {
                "cancel accepted, new presumed not placed"
            }
            Self::NewNotPlaced(NewLeg::Unresolved) => "cancel accepted, new unresolved",
            Self::NewNotPlaced(NewLeg::NeverSent) => "cancel accepted, new not sent",
            Self::PlacedFirstFailed(NewLeg::Rejected { .. }) => "new rejected, old kept",
            Self::PlacedFirstFailed(NewLeg::Found) => "new found, old kept",
            Self::PlacedFirstFailed(NewLeg::NotPlacedPresumed) => {
                "new presumed not placed, old kept"
            }
            Self::PlacedFirstFailed(NewLeg::Unresolved) => "new unresolved, old kept",
            Self::PlacedFirstFailed(NewLeg::NeverSent) => "new not sent, old kept",
            Self::OldNotCancelled(_) => "new accepted, old cancel not confirmed",
            Self::Lost => "task ended without an answer",
        }
    }
}

/// A finished replace, as the main loop gets it. Holds no credential and no client id. The
/// result may carry exchange text, so `Debug` prints the fixed tag only.
#[derive(Clone, PartialEq, Eq)]
pub struct ReplaceSummary {
    pub account: Uuid,
    pub symbol: String,
    pub old_id: String,
    pub kind: ReplaceKind,
    pub dir: Dir,
    pub old_trigger: Decimal,
    pub new_trigger: Decimal,
    pub result: ReplaceResult,
    /// A send went out without its write-ahead record: the journal is damaged.
    pub journal_failed: bool,
    /// Set only with [`ReplaceResult::Done`]: what the automatic protection needs to follow the
    /// new order (f3c). Names client ids, so it never reaches a log (`Debug` prints the tag).
    pub replacement: Option<Replacement>,
}

impl std::fmt::Debug for ReplaceSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplaceSummary")
            .field("result", &self.result.tag())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// What to do about a finished replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handling {
    pub level: Level,
    /// Both legs, in words (the toast and the ticket line).
    pub text: String,
    /// The position may have lost this protection: a red banner (d4) with this reason.
    pub failure: Option<FailReason>,
    /// A system notification goes with the failure.
    pub notify_os: bool,
}

fn kind_name(kind: ReplaceKind) -> &'static str {
    match kind {
        ReplaceKind::Stop => "stop-loss",
        ReplaceKind::Target => "take-profit",
    }
}

/// The words and follow-ups for the end of a replace. A failure is given only when the new order
/// is known not to stand (or not known to); a found order is only a warning.
pub fn handle(s: &ReplaceSummary) -> Handling {
    let name = kind_name(s.kind);
    let moved = format!(
        "{} → {}",
        s.old_trigger.to_plain_string(),
        s.new_trigger.to_plain_string()
    );
    let unprotected = format!(
        "The position may have no {name} now. Check the open orders and add one by hand if it is \
         missing."
    );
    match &s.result {
        ReplaceResult::Done => Handling {
            level: Level::Info,
            text: format!(
                "{} {name} moved ({moved}). Cancel accepted, new order accepted.",
                s.symbol
            ),
            failure: None,
            notify_os: false,
        },
        ReplaceResult::Refused(why) => Handling {
            level: Level::Warn,
            text: format!(
                "{}: the {name} was not moved. Nothing was sent; the old order stays. {}",
                s.symbol,
                crate::order_flow::clip(why)
            ),
            failure: None,
            notify_os: false,
        },
        ReplaceResult::CancelNotConfirmed(end) => {
            let (level, text) = if end.may_be_gone() {
                (
                    Level::Error,
                    format!(
                        "{}: the cancel of the old {name} got no confirmed answer and no new \
                         order was placed. {unprotected}",
                        s.symbol
                    ),
                )
            } else {
                let why = match end {
                    CancelEnd::Rejected { code, msg } => {
                        format!(
                            "The exchange refused the cancel: {code} {}",
                            crate::order_flow::clip(msg)
                        )
                    }
                    _ => "The cancel was not sent.".to_string(),
                };
                (
                    Level::Warn,
                    format!(
                        "{}: the {name} was not moved. {why} The order may have triggered or \
                         filled; no new order was placed.",
                        s.symbol
                    ),
                )
            };
            Handling {
                level,
                text,
                failure: None,
                notify_os: false,
            }
        }
        ReplaceResult::NewNotPlaced(leg) => {
            let (level, why, failure) = match leg {
                NewLeg::Rejected { code, msg } => (
                    Level::Error,
                    format!(
                        "The exchange refused the new order: {code} {}",
                        crate::order_flow::clip(msg)
                    ),
                    Some(FailReason::Rejected),
                ),
                NewLeg::Found => (
                    Level::Warn,
                    "The new order had no answer at first; the exchange knows it. This is not a \
                     fill: check the open orders."
                        .to_string(),
                    None,
                ),
                NewLeg::NotPlacedPresumed => (
                    Level::Error,
                    "The new order had no answer and the exchange never showed it. Nothing was \
                     resent."
                        .to_string(),
                    Some(FailReason::Unknown),
                ),
                NewLeg::Unresolved => (
                    Level::Error,
                    "The result of the new order is unknown. Nothing was resent.".to_string(),
                    Some(FailReason::Unknown),
                ),
                NewLeg::NeverSent => (
                    Level::Error,
                    "The new order was not sent.".to_string(),
                    Some(FailReason::NotSent),
                ),
            };
            let text = if failure.is_some() {
                format!(
                    "{}: the old {name} was cancelled but the new one is not in place. {why} \
                     {unprotected}",
                    s.symbol
                )
            } else {
                format!(
                    "{}: the old {name} was cancelled ({moved}). {why}",
                    s.symbol
                )
            };
            Handling {
                level,
                text,
                notify_os: failure.is_some(),
                failure,
            }
        }
        ReplaceResult::PlacedFirstFailed(leg) => {
            // The old order stands. Only a new order that may exist (found or unknown) is loud.
            let (level, why, failure) = match leg {
                NewLeg::Rejected { code, msg } => (
                    Level::Warn,
                    format!(
                        "The exchange refused the new order: {code} {}",
                        crate::order_flow::clip(msg)
                    ),
                    None,
                ),
                NewLeg::NotPlacedPresumed => (
                    Level::Warn,
                    "The new order had no answer and the exchange never showed it.".to_string(),
                    None,
                ),
                NewLeg::NeverSent => (Level::Warn, "The new order was not sent.".to_string(), None),
                NewLeg::Found => (
                    Level::Error,
                    "The new order had no answer at first; the exchange knows it, so two \
                     orders now stand. Cancel one of them by hand."
                        .to_string(),
                    None,
                ),
                NewLeg::Unresolved => (
                    Level::Error,
                    "The result of the new order is unknown, so two orders may stand. Nothing \
                     was resent. Check the open orders."
                        .to_string(),
                    Some(FailReason::Unknown),
                ),
            };
            Handling {
                level,
                text: format!(
                    "{}: the {name} was not moved. The old order was not cancelled. {why}",
                    s.symbol
                ),
                notify_os: failure.is_some(),
                failure,
            }
        }
        ReplaceResult::OldNotCancelled(end) => {
            let why = match end {
                CancelEnd::Rejected { code, msg } => format!(
                    "The exchange refused the cancel of the old order: {code} {}",
                    crate::order_flow::clip(msg)
                ),
                CancelEnd::NotSent => "The cancel of the old order was not sent.".to_string(),
                CancelEnd::Unknown | CancelEnd::Unconfirmed => {
                    "The cancel of the old order got no confirmed answer.".to_string()
                }
            };
            // Both stand, so the position is covered; the open orders and the strays list show
            // the extra one. The old order may also have triggered or filled.
            Handling {
                level: Level::Error,
                text: format!(
                    "{}: the new {name} ({moved}) was placed first, but the old one is not \
                     confirmed cancelled. {why} Two protective orders may stand: check the open \
                     orders and cancel one by hand.",
                    s.symbol
                ),
                failure: None,
                notify_os: false,
            }
        }
        ReplaceResult::Lost => Handling {
            level: Level::Error,
            text: format!(
                "{}: the replace ended without an answer; either order may have gone out. \
                 {unprotected}",
                s.symbol
            ),
            failure: Some(FailReason::Unknown),
            notify_os: true,
        },
    }
}

/// Only a result that stayed unknown keeps the symbol blocked and the unknown list is read
/// again: a new order of unknown fate, or a task that ended with no answer (either leg may have
/// gone out). Same as `Unresolved` for a plain send.
pub fn keeps_symbol_blocked(result: &ReplaceResult) -> bool {
    matches!(
        result,
        ReplaceResult::NewNotPlaced(NewLeg::Unresolved)
            | ReplaceResult::PlacedFirstFailed(NewLeg::Unresolved)
            | ReplaceResult::Lost
    )
}

// ── State ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    key: String,
    account: Uuid,
    symbol: String,
    dir: Dir,
    kind: ReplaceKind,
    reason: FailReason,
    notified: bool,
}

/// The replaces in flight and the failures the user has not handled. In memory only.
#[derive(Debug, Default)]
pub struct ReplaceFlow {
    /// (account, old order id) of the sends that were started and have not ended. They are not
    /// owned by a window, so closing the confirmation does not drop them.
    inflight: Vec<(Uuid, String)>,
    failures: Vec<Failure>,
    seq: u64,
}

impl ReplaceFlow {
    pub fn is_busy(&self, account: Uuid) -> bool {
        self.inflight.iter().any(|(a, _)| *a == account)
    }

    /// `false` when a replace of the account is already in flight.
    #[must_use]
    pub fn begin_send(&mut self, account: Uuid, old_id: &str) -> bool {
        if self.is_busy(account) {
            return false;
        }
        self.inflight.push((account, old_id.to_string()));
        true
    }

    pub fn finish_send(&mut self, account: Uuid, old_id: &str) {
        self.inflight
            .retain(|(a, id)| !(*a == account && id == old_id));
    }

    /// A failure the panel shows as a red banner. One per (account, symbol, direction, kind):
    /// a second one replaces the first.
    pub fn record_failure(&mut self, s: &ReplaceSummary, reason: FailReason) {
        self.failures.retain(|f| {
            !(f.account == s.account && f.symbol == s.symbol && f.dir == s.dir && f.kind == s.kind)
        });
        self.seq += 1;
        self.failures.push(Failure {
            key: format!("{FAILURE_KEY_PREFIX}{}", self.seq),
            account: s.account,
            symbol: s.symbol.clone(),
            dir: s.dir,
            kind: s.kind,
            reason,
            notified: false,
        });
    }

    /// The banners of the account, in the panel's own shape. A stop can be offered again (the
    /// panel's "retry" opens a whole-position stop review); a target cannot.
    pub fn failure_views(&self, account: Uuid) -> Vec<FailureView> {
        self.failures
            .iter()
            .filter(|f| f.account == account)
            .map(|f| FailureView {
                key: f.key.clone(),
                account: f.account,
                symbol: f.symbol.clone(),
                reasons: vec![f.reason],
                dir: f.dir,
                may_retry: f.kind == ReplaceKind::Stop,
                active: false,
                entry_open: false,
                entry_unresolved: false,
                standing: 0,
            })
            .collect()
    }

    pub fn owns_key(key: &str) -> bool {
        key.starts_with(FAILURE_KEY_PREFIX)
    }

    /// The user handled it. The symbol, for the risk log line.
    pub fn dismiss(&mut self, account: Uuid, key: &str) -> Option<String> {
        let at = self
            .failures
            .iter()
            .position(|f| f.account == account && f.key == key)?;
        Some(self.failures.remove(at).symbol)
    }

    /// A whole-position stop was accepted for the position: the failures that left it without a
    /// stop are cleared.
    pub fn protection_added(&mut self, account: Uuid, symbol: &str, dir: Dir) {
        self.failures.retain(|f| {
            !(f.account == account
                && f.symbol == symbol
                && f.dir == dir
                && f.kind == ReplaceKind::Stop)
        });
    }

    /// The accounts that have a failure not yet told about by a system notification.
    pub fn take_notices(&mut self) -> Vec<Uuid> {
        let mut out = Vec::new();
        for f in self.failures.iter_mut().filter(|f| !f.notified) {
            f.notified = true;
            out.push(f.account);
        }
        out
    }

    #[cfg(test)]
    pub fn failure_count(&self) -> usize {
        self.failures.len()
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::parse(s).unwrap()
    }

    fn guard() -> Guard {
        Guard {
            venue: AccountVenue::Binance,
            product: AccountProduct::UsdM,
            live: false,
            live_open: false,
            stream: Some(StreamState::Connected),
            lists_stale: false,
            busy: false,
        }
    }

    /// A one-way long's stop-loss for half a bitcoin.
    fn stop() -> OpenOrder {
        OpenOrder {
            symbol: "BTCUSDT".into(),
            id: "777".into(),
            kind: OpenKind::Algo(None),
            side: Some(Side::Sell),
            position_side: None,
            qty: Some(d("0.5")),
            trigger_price: Some(d("58000")),
            close_position: false,
            reduce_only: true,
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
    fn a_stop_and_a_target_can_be_grabbed_and_nothing_else() {
        assert_eq!(candidate(&stop()), Some(ReplaceKind::Stop));
        let mut t = stop();
        t.role = Some(ProtectRole::Target);
        assert_eq!(candidate(&t), Some(ReplaceKind::Target));
        for role in [ProtectRole::Trailing, ProtectRole::StopAndTarget] {
            let mut r = stop();
            r.role = Some(role);
            assert_eq!(candidate(&r), None, "{role:?}");
        }
        let mut plain = stop();
        plain.kind = OpenKind::Plain;
        assert_eq!(candidate(&plain), None);
        let mut entry = stop();
        entry.role = None;
        assert_eq!(candidate(&entry), None);
    }

    #[test]
    fn each_condition_that_blocks_a_drag_is_named() {
        let row = stop();
        assert_eq!(drag_block(&row, &guard(), false), None);

        let mut g = guard();
        g.stream = Some(StreamState::Resyncing);
        assert_eq!(drag_block(&row, &g, false), Some(DragBlock::Stream));
        g.stream = None;
        assert_eq!(drag_block(&row, &g, false), Some(DragBlock::Stream));

        let mut g = guard();
        g.lists_stale = true;
        assert_eq!(drag_block(&row, &g, false), Some(DragBlock::Stale));

        let mut g = guard();
        g.live = true;
        assert_eq!(drag_block(&row, &g, false), Some(DragBlock::Live));
        // Open live trading would lift it; the flag stays false in this build.
        g.live_open = true;
        assert_eq!(drag_block(&row, &g, false), None);

        let mut g = guard();
        g.busy = true;
        assert_eq!(drag_block(&row, &g, false), Some(DragBlock::Busy));

        assert_eq!(drag_block(&row, &guard(), true), Some(DragBlock::AutoJob));

        let mut g = guard();
        g.product = AccountProduct::Spot;
        assert_eq!(drag_block(&row, &g, false), Some(DragBlock::Product));

        let mut limit = stop();
        limit.limit = true;
        assert_eq!(
            drag_block(&limit, &guard(), false),
            Some(DragBlock::LimitTrigger)
        );
    }

    #[test]
    fn a_hedge_mode_part_of_a_position_is_not_draggable_but_the_whole_of_it_is() {
        let mut part = stop();
        part.position_side = Some(Dir::Long);
        part.reduce_only = false;
        assert_eq!(
            drag_block(&part, &guard(), false),
            Some(DragBlock::PartialHedge)
        );
        let mut whole = part.clone();
        whole.close_position = true;
        whole.qty = None;
        assert_eq!(drag_block(&whole, &guard(), false), None);
    }

    #[test]
    fn an_okx_order_must_be_a_conditional_one() {
        let mut g = guard();
        g.venue = AccountVenue::Okx;
        let mut row = stop();
        row.kind = OpenKind::Algo(None);
        assert_eq!(drag_block(&row, &g, false), Some(DragBlock::NotConditional));
        row.kind = OpenKind::Algo(Some(OkxAlgoType::Oco));
        assert_eq!(drag_block(&row, &g, false), Some(DragBlock::NotConditional));
        row.kind = OpenKind::Algo(Some(OkxAlgoType::Conditional));
        assert_eq!(drag_block(&row, &g, false), None);
    }

    #[test]
    fn a_row_missing_what_the_new_order_needs_is_not_draggable() {
        for mutate in [
            (|r: &mut OpenOrder| r.side = None) as fn(&mut OpenOrder),
            |r| r.trigger_price = None,
            |r| r.qty = None,
        ] {
            let mut r = stop();
            mutate(&mut r);
            assert_eq!(drag_block(&r, &guard(), false), Some(DragBlock::Unreadable));
        }
    }

    #[test]
    fn the_plan_keeps_the_size_and_the_kind_of_the_old_order() {
        let p = plan(&stop(), d("57500"), &guard(), false).unwrap();
        assert_eq!(p.kind, ReplaceKind::Stop);
        assert_eq!(p.dir, Dir::Long);
        assert_eq!(p.old_trigger, d("58000"));
        assert_eq!(
            p.spec,
            ManualSpec::Stop {
                dir: Dir::Long,
                size: Size::Qty(d("0.5")),
                trigger: d("57500"),
            }
        );

        // A short's whole-position target.
        let mut t = stop();
        t.role = Some(ProtectRole::Target);
        t.side = Some(Side::Buy);
        t.close_position = true;
        t.qty = None;
        t.trigger_price = Some(d("50000"));
        let p = plan(&t, d("49000"), &guard(), false).unwrap();
        assert_eq!(p.kind, ReplaceKind::Target);
        assert_eq!(
            p.spec,
            ManualSpec::TakeProfit {
                dir: Dir::Short,
                size: Size::ClosePosition,
                trigger: d("49000"),
            }
        );

        // A hedge leg names its direction itself.
        let mut h = stop();
        h.position_side = Some(Dir::Short);
        h.side = Some(Side::Buy);
        h.close_position = true;
        let p = plan(&h, d("59000"), &guard(), false).unwrap();
        assert!(matches!(
            p.spec,
            ManualSpec::Stop {
                dir: Dir::Short,
                ..
            }
        ));
    }

    #[test]
    fn the_same_price_is_no_action_and_a_bad_price_is_refused() {
        assert_eq!(
            plan(&stop(), d("58000"), &guard(), false),
            Err(PlanRefusal::Unchanged)
        );
        // Written another way, still the same number.
        assert_eq!(
            plan(&stop(), d("58000.00"), &guard(), false),
            Err(PlanRefusal::Unchanged)
        );
        assert_eq!(
            plan(&stop(), d("0"), &guard(), false),
            Err(PlanRefusal::BadPrice)
        );
    }

    #[test]
    fn a_blocked_or_unreplaceable_row_makes_no_plan() {
        let mut g = guard();
        g.lists_stale = true;
        assert_eq!(
            plan(&stop(), d("57000"), &g, false),
            Err(PlanRefusal::Blocked(DragBlock::Stale))
        );
        let mut trailing = stop();
        trailing.role = Some(ProtectRole::Trailing);
        assert_eq!(
            plan(&trailing, d("57000"), &guard(), false),
            Err(PlanRefusal::NotReplaceable)
        );
    }

    #[test]
    fn the_confirmation_is_checked_again_on_the_latest_list() {
        let reviewed = stop();
        let list = vec![stop()];
        assert!(recheck(&reviewed, &list, &guard(), |_| false).is_ok());

        // Gone.
        assert_eq!(
            recheck(&reviewed, &[], &guard(), |_| false).unwrap_err(),
            Recheck::Gone
        );
        // Another order now has that id's place: the row differs.
        let mut moved = stop();
        moved.trigger_price = Some(d("57000"));
        assert_eq!(
            recheck(&reviewed, &[moved], &guard(), |_| false).unwrap_err(),
            Recheck::Changed
        );
        let mut resized = stop();
        resized.qty = Some(d("0.3"));
        assert_eq!(
            recheck(&reviewed, &[resized], &guard(), |_| false).unwrap_err(),
            Recheck::Changed
        );
        // Each disable condition is checked again.
        let mut g = guard();
        g.lists_stale = true;
        assert_eq!(
            recheck(&reviewed, &list, &g, |_| false).unwrap_err(),
            Recheck::Blocked(DragBlock::Stale)
        );
        let mut g = guard();
        g.stream = Some(StreamState::Reconnecting);
        assert_eq!(
            recheck(&reviewed, &list, &g, |_| false).unwrap_err(),
            Recheck::Blocked(DragBlock::Stream)
        );
        assert_eq!(
            recheck(&reviewed, &list, &guard(), |_| true).unwrap_err(),
            Recheck::Blocked(DragBlock::AutoJob)
        );
        let mut g = guard();
        g.live = true;
        assert_eq!(
            recheck(&reviewed, &list, &g, |_| false).unwrap_err(),
            Recheck::Blocked(DragBlock::Live)
        );
    }

    #[test]
    fn a_trigger_that_would_fire_at_once_is_found_by_side_and_kind() {
        let mark = d("60000");
        let m = Some(&mark);
        // A long's stop fires when the price falls to it: above the mark is at once.
        assert_eq!(
            would_trigger_now(ReplaceKind::Stop, Dir::Long, &d("61000"), m),
            Some(true)
        );
        assert_eq!(
            would_trigger_now(ReplaceKind::Stop, Dir::Long, &d("59000"), m),
            Some(false)
        );
        // A long's target fires when it rises to it.
        assert_eq!(
            would_trigger_now(ReplaceKind::Target, Dir::Long, &d("59000"), m),
            Some(true)
        );
        assert_eq!(
            would_trigger_now(ReplaceKind::Target, Dir::Long, &d("61000"), m),
            Some(false)
        );
        // A short is the other way round.
        assert_eq!(
            would_trigger_now(ReplaceKind::Stop, Dir::Short, &d("59000"), m),
            Some(true)
        );
        assert_eq!(
            would_trigger_now(ReplaceKind::Target, Dir::Short, &d("61000"), m),
            Some(true)
        );
        assert_eq!(
            would_trigger_now(ReplaceKind::Stop, Dir::Long, &d("61000"), None),
            None
        );
    }

    #[test]
    fn a_height_on_the_chart_becomes_a_price_and_then_a_decimal() {
        // Plot from y=100, 400 tall, showing 50000..60000 with the high at the top.
        assert_eq!(
            price_at_y(100.0, 100.0, 400.0, 50_000.0, 60_000.0, false),
            Some(60_000.0)
        );
        assert_eq!(
            price_at_y(500.0, 100.0, 400.0, 50_000.0, 60_000.0, false),
            Some(50_000.0)
        );
        assert_eq!(
            price_at_y(300.0, 100.0, 400.0, 50_000.0, 60_000.0, false),
            Some(55_000.0)
        );
        // Inverted: the high at the bottom.
        assert_eq!(
            price_at_y(100.0, 100.0, 400.0, 50_000.0, 60_000.0, true),
            Some(50_000.0)
        );
        // Outside the plot, or a degenerate scale.
        assert_eq!(
            price_at_y(99.0, 100.0, 400.0, 50_000.0, 60_000.0, false),
            None
        );
        assert_eq!(
            price_at_y(501.0, 100.0, 400.0, 50_000.0, 60_000.0, false),
            None
        );
        assert_eq!(
            price_at_y(300.0, 100.0, 0.0, 50_000.0, 60_000.0, false),
            None
        );
        assert_eq!(
            price_at_y(300.0, 100.0, 400.0, 60_000.0, 60_000.0, false),
            None
        );
        assert_eq!(
            price_at_y(f32::NAN, 100.0, 400.0, 50_000.0, 60_000.0, false),
            None
        );
        // A scale that includes zero or less gives no price at its bottom edge.
        assert_eq!(price_at_y(500.0, 100.0, 400.0, 0.0, 10.0, false), None);

        assert_eq!(decimal_of_price(57_123.5), Some(d("57123.5")));
        assert_eq!(decimal_of_price(0.5), Some(d("0.5")));
        assert_eq!(decimal_of_price(f32::INFINITY), None);
        assert_eq!(decimal_of_price(-1.0), None);
        assert_eq!(decimal_of_price(0.0), None);
    }

    pub fn summary_of(result: ReplaceResult) -> ReplaceSummary {
        summary(ReplaceKind::Stop, result)
    }

    fn summary(kind: ReplaceKind, result: ReplaceResult) -> ReplaceSummary {
        ReplaceSummary {
            account: Uuid::nil(),
            symbol: "BTCUSDT".into(),
            old_id: "777".into(),
            kind,
            dir: Dir::Long,
            old_trigger: d("58000"),
            new_trigger: d("57500"),
            result,
            journal_failed: false,
            replacement: None,
        }
    }

    #[test]
    fn a_lost_replace_keeps_the_symbol_blocked_like_an_unresolved_one() {
        assert!(keeps_symbol_blocked(&ReplaceResult::Lost));
        assert!(keeps_symbol_blocked(&ReplaceResult::NewNotPlaced(
            NewLeg::Unresolved
        )));
        assert!(keeps_symbol_blocked(&ReplaceResult::PlacedFirstFailed(
            NewLeg::Unresolved
        )));
        for r in [
            ReplaceResult::Done,
            ReplaceResult::NewNotPlaced(NewLeg::Found),
            ReplaceResult::NewNotPlaced(NewLeg::NotPlacedPresumed),
            ReplaceResult::CancelNotConfirmed(CancelEnd::Unknown),
        ] {
            assert!(!keeps_symbol_blocked(&r));
        }
    }

    #[test]
    fn a_place_first_failure_keeps_the_old_order_and_is_loud_only_when_two_may_stand() {
        let quiet = [
            NewLeg::Rejected {
                code: "-2021".into(),
                msg: "x".into(),
            },
            NewLeg::NotPlacedPresumed,
            NewLeg::NeverSent,
        ];
        for leg in quiet {
            let h = handle(&summary(
                ReplaceKind::Stop,
                ReplaceResult::PlacedFirstFailed(leg),
            ));
            assert_eq!(h.level, Level::Warn);
            assert!(h.failure.is_none() && !h.notify_os);
            assert!(h.text.contains("old order was not cancelled"), "{}", h.text);
        }
        let h = handle(&summary(
            ReplaceKind::Stop,
            ReplaceResult::PlacedFirstFailed(NewLeg::Found),
        ));
        assert_eq!(h.level, Level::Error);
        assert!(h.text.contains("two orders now stand"), "{}", h.text);
        let h = handle(&summary(
            ReplaceKind::Stop,
            ReplaceResult::PlacedFirstFailed(NewLeg::Unresolved),
        ));
        assert_eq!(h.level, Level::Error);
        assert!(h.failure.is_some() && h.notify_os);
    }

    #[test]
    fn an_old_order_that_was_not_cancelled_after_a_new_one_says_two_orders_stand() {
        for end in [
            CancelEnd::Unknown,
            CancelEnd::Unconfirmed,
            CancelEnd::NotSent,
            CancelEnd::Rejected {
                code: "-2011".into(),
                msg: "gone".into(),
            },
        ] {
            let result = ReplaceResult::OldNotCancelled(end);
            assert!(!keeps_symbol_blocked(&result));
            let h = handle(&summary(ReplaceKind::Stop, result));
            assert_eq!(h.level, Level::Error);
            assert!(
                h.text.contains("Two protective orders may stand"),
                "{}",
                h.text
            );
            assert!(h.text.contains("57500"), "{}", h.text);
        }
    }

    #[test]
    fn a_done_replace_moves_the_line() {
        let h = handle(&summary(ReplaceKind::Stop, ReplaceResult::Done));
        assert_eq!(h.level, Level::Info);
        assert!(h.failure.is_none() && !h.notify_os);
        assert!(h.text.contains("58000 → 57500"), "{}", h.text);
    }

    #[test]
    fn a_refused_replace_is_a_toast() {
        let h = handle(&summary(
            ReplaceKind::Stop,
            ReplaceResult::Refused("The content changed.".into()),
        ));
        assert_eq!(h.level, Level::Warn);
        assert!(h.failure.is_none() && !h.notify_os);
        assert!(h.text.contains("Nothing was sent"), "{}", h.text);
    }

    #[test]
    fn a_cancel_without_a_confirmed_answer_warns_that_the_position_may_be_bare() {
        for end in [CancelEnd::Unknown, CancelEnd::Unconfirmed] {
            let h = handle(&summary(
                ReplaceKind::Stop,
                ReplaceResult::CancelNotConfirmed(end),
            ));
            assert_eq!(h.level, Level::Error);
            assert!(h.failure.is_none());
            assert!(h.text.contains("may have no stop-loss"), "{}", h.text);
        }
        // A refused cancel: the old order was not cancelled, or already gone.
        for end in [
            CancelEnd::Rejected {
                code: "-2011".into(),
                msg: "Unknown order".into(),
            },
            CancelEnd::NotSent,
        ] {
            let h = handle(&summary(
                ReplaceKind::Target,
                ReplaceResult::CancelNotConfirmed(end),
            ));
            assert_eq!(h.level, Level::Warn);
            assert!(h.failure.is_none());
            assert!(!h.text.contains("may have no"), "{}", h.text);
        }
    }

    #[test]
    fn a_new_order_that_is_not_in_place_is_a_banner_with_a_system_notification() {
        let cases = [
            (
                NewLeg::Rejected {
                    code: "-4130".into(),
                    msg: "x".into(),
                },
                FailReason::Rejected,
            ),
            (NewLeg::NotPlacedPresumed, FailReason::Unknown),
            (NewLeg::Unresolved, FailReason::Unknown),
            (NewLeg::NeverSent, FailReason::NotSent),
        ];
        for (leg, reason) in cases {
            let h = handle(&summary(
                ReplaceKind::Stop,
                ReplaceResult::NewNotPlaced(leg),
            ));
            assert_eq!(h.failure, Some(reason));
            assert!(h.notify_os);
            assert_eq!(h.level, Level::Error);
            assert!(h.text.contains("cancelled"), "{}", h.text);
        }
        // The lookup found the new order: a warning, no banner.
        let h = handle(&summary(
            ReplaceKind::Stop,
            ReplaceResult::NewNotPlaced(NewLeg::Found),
        ));
        assert_eq!(h.level, Level::Warn);
        assert!(h.failure.is_none() && !h.notify_os);
        // A task that ended without an answer knows nothing about either leg.
        let h = handle(&summary(ReplaceKind::Stop, ReplaceResult::Lost));
        assert_eq!(h.failure, Some(FailReason::Unknown));
        assert!(h.notify_os);
    }

    #[test]
    fn a_failure_banner_offers_a_retry_for_a_stop_only_and_goes_when_handled() {
        let mut flow = ReplaceFlow::default();
        let acct = Uuid::from_u128(7);
        let mut s = summary(ReplaceKind::Stop, ReplaceResult::Lost);
        s.account = acct;
        flow.record_failure(&s, FailReason::Unknown);
        let views = flow.failure_views(acct);
        assert_eq!(views.len(), 1);
        assert!(views[0].may_retry && views[0].reasons == [FailReason::Unknown]);
        assert!(ReplaceFlow::owns_key(&views[0].key));
        assert!(flow.failure_views(Uuid::from_u128(8)).is_empty());

        // A second failure of the same position and kind replaces the first.
        flow.record_failure(&s, FailReason::NotSent);
        assert_eq!(flow.failure_count(), 1);

        let mut t = summary(ReplaceKind::Target, ReplaceResult::Lost);
        t.account = acct;
        flow.record_failure(&t, FailReason::Unknown);
        let views = flow.failure_views(acct);
        assert_eq!(views.len(), 2);
        assert_eq!(views.iter().filter(|v| v.may_retry).count(), 1);

        // Notified once.
        assert_eq!(flow.take_notices(), vec![acct, acct]);
        assert!(flow.take_notices().is_empty());

        // A whole-position stop clears the stop's banner, not the target's.
        flow.protection_added(acct, "BTCUSDT", Dir::Long);
        assert_eq!(flow.failure_count(), 1);
        let key = flow.failure_views(acct)[0].key.clone();
        assert_eq!(flow.dismiss(acct, &key).as_deref(), Some("BTCUSDT"));
        assert_eq!(flow.dismiss(acct, &key), None);
    }

    #[test]
    fn one_replace_per_account_is_in_flight_and_it_is_not_owned_by_a_window() {
        let mut flow = ReplaceFlow::default();
        let a = Uuid::from_u128(1);
        assert!(flow.begin_send(a, "777"));
        assert!(flow.is_busy(a));
        assert!(!flow.begin_send(a, "778"), "a second replace waits");
        assert!(flow.begin_send(Uuid::from_u128(2), "9"));
        flow.finish_send(a, "777");
        assert!(!flow.is_busy(a));
        assert!(flow.begin_send(a, "778"));
    }
}
