//! The orders the app sends by itself after an entry fills (M5 d3), on Binance USDⓈ-M.
//!
//! This is the **only** file that calls `submit_auto` or builds an `AutoIntent`. Every request
//! here is checked by the entry's `AutoGrant` (made when the user confirmed the entry), so the
//! app can place only the protective orders that confirmation listed, only for what has filled,
//! and cancel only what it placed itself.
//!
//! The state machine ([`AutoBook`]) is pure: events go in (the entry was sent, a fill, an order
//! update), jobs come out ([`AutoBook::poll`]), the results go back in ([`AutoBook::done`]). The
//! sending ([`run`]) is a thin wrapper: Keychain → client → grant → `submit_auto`.
//!
//! Rules it keeps:
//! - a whole-position stop goes out at the **first fill**; take-profit orders and a trailing
//!   stop when the entry has **finished** (Binance cannot change a conditional order, so there
//!   is no replacement);
//! - nothing is sent while the entry's result is unknown, and nothing is ever retried: a failure
//!   is written down for the panel (d4 adds the buttons);
//! - when a stop-loss fires the take-profit orders and the trailing stop are cancelled once, and
//!   when a take-profit or trailing stop fires the take-profit orders go (trailing) or stay;
//!   the whole-position stop is cancelled only when an order has **finished**, the list of
//!   positions is fresh and it shows no position in the entry's direction any more;
//! - a fill without a client id or a trade id is not guessed at: it is not counted;
//! - every failure that leaves a position without its protection, or an order that could not be
//!   cancelled, is written down for the panel (never silent), and an entry that was given up is
//!   not counted as "still active" for the one-entry-per-symbol rule.

use crate::order_flow::{SendResult, describe_submit_error};
use crate::protect_attach::Holdings;
use crate::trading::account_key;
use data::config::exchange_accounts::{AccountMeta, StoreError, load_credentials_with_identity};
use data::config::risk_log::{RiskLog, RiskLogEntry};
use data::config::trade_gate::{SessionEntry, SessionGate};
use data::config::unknown_store::UnknownStore;
use exchange::account::{PositionMode, ProxyState, check_proxy_snapshot};
use exchange::signed_http::SignedHttp;
use exchange::trade::stream::{OrderCategory, OrderStatus, OrderUpdate};
use exchange::trade::{
    AutoGrant, AutoIntent, AutoPath, ClientId, Decimal, Dir, GrantRoles, IssuedRef, OrderIntent,
    Outcome, ProtectSpec, RejectKind, Replacement, Resolution, Role, SharedRisk, Side, Stage,
    StageInput, TradeClient, build_stage, stage_roles,
};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use uuid::Uuid;

/// How long a grant places orders after the confirmation (a resting limit entry may fill late).
/// Cancelling what it placed stays possible afterwards.
pub const GRANT_VALID_MS: i64 = 24 * 60 * 60 * 1000;
/// Decimals kept in an average fill price (the tick rounds it afterwards).
const AVG_SCALE: u32 = 8;
const MAX_NOTES: usize = 12;
/// Algo events of an account whose client id the book did not know yet, kept per account for a
/// replace that completes after its events (f3c). Held in memory only.
const MAX_UNCLAIMED: usize = 32;
/// Client ids of orders of entries that left the book, to tell a late replace answer for one of
/// them from an order the app never placed.
const MAX_FORGOTTEN: usize = 64;
/// How long the Keychain, clock and rules steps may take before the job is given up.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);

pub const REDUCED_INSTEAD: &str = "The entry filled against a position held the other way, so it \
     reduced that position and no protective orders were sent for it. Check the position.";

pub const FILLS_NOT_SEEN: &str = "Some fills of the entry were not seen; the position may not be \
     protected. Check it.";

pub const FILLED_AFTER_CLOSE: &str = "The entry filled again after its stop-loss closed the \
     position; that part is not protected.";

pub const STOP_KEPT: &str = "The other protective orders filled but a position remains; the \
     stop-loss was kept.";

pub const NOT_FOUND_OPEN: &str = "The entry was not found open after the connection came back; \
     automatic take-profit and trailing orders for it have stopped. A stop-loss is still placed \
     if a fill of it is seen. Check the position.";
pub const LOST_CONNECTION: &str = "The connection was lost while the entry was open; it may have \
     filled unseen. Automatic take-profit and trailing orders for it have stopped; a stop-loss \
     is still placed if a fill of it is seen. Check the position.";

pub const SWITCHED_OFF: &str = "Auto-attach was switched off before the protective orders \
     went out: they were not sent. The position may not be fully protected.";

/// How long the fills of an entry that the exchange calls `Filled` may stay short of its quantity
/// before they are written down as not seen. The order update and the fills come as separate
/// messages (the order first on Binance), so a short wait is normal; ten seconds is far more than
/// the gap between two messages of one stream. Read at the next poll, which runs on every stream
/// event and on the periodic equity tick, so the real delay can be a little longer.
pub const FILLS_WAIT_MS: i64 = 10_000;

/// Why a position may not be protected, or an order of the app may still stand. One entry of the
/// book keeps its list (never silent, never retried) until the user has handled it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    /// The exchange refused a protective order (its message is never shown or kept).
    Rejected,
    /// HTTP 429 or 418: the order was not executed and is not sent again.
    RateLimited,
    /// The app's own checks, the gate or the connection stopped the order before it went out.
    NotSent,
    /// No answer, and the lookup did not find the order either.
    Unknown,
    /// The trigger prices could not be worked out from the fill and the settings.
    PlanError,
    /// The quantity does not fit the symbol's size rules.
    QtyCheck,
    /// The grant made at the confirmation of the entry refused the request.
    GrantRefused,
    /// Credentials, trading rules or the connection could not be set up for the job.
    SetupFailed,
    /// The market price the stop was checked against was older than [`REF_PRICE_MAX_AGE_SECS`].
    StaleReference,
    SwitchedOff,
    ReducedInstead,
    /// A protective order triggered but did not take effect.
    NotTakingEffect,
    /// A cancel was refused: the order may still stand.
    CancelFailed,
    /// A cancel got no answer: the order may still stand.
    CancelUnknown,
    FillsNotSeen,
    FilledAfterClose,
    NotFoundOpen,
    LostConnection,
}

impl FailReason {
    /// Every reason, in the order of [`Self::index`]; the table test walks it.
    #[cfg(test)]
    const ALL: [FailReason; 18] = [
        FailReason::Rejected,
        FailReason::RateLimited,
        FailReason::NotSent,
        FailReason::Unknown,
        FailReason::PlanError,
        FailReason::QtyCheck,
        FailReason::GrantRefused,
        FailReason::SetupFailed,
        FailReason::StaleReference,
        FailReason::SwitchedOff,
        FailReason::ReducedInstead,
        FailReason::NotTakingEffect,
        FailReason::CancelFailed,
        FailReason::CancelUnknown,
        FailReason::FillsNotSeen,
        FailReason::FilledAfterClose,
        FailReason::NotFoundOpen,
        FailReason::LostConnection,
    ];

    /// Position in [`Self::ALL`]. An exhaustive match: a new reason does not compile until it is
    /// given a place, and the test then checks it is in the table.
    #[cfg(test)]
    fn index(self) -> usize {
        match self {
            FailReason::Rejected => 0,
            FailReason::RateLimited => 1,
            FailReason::NotSent => 2,
            FailReason::Unknown => 3,
            FailReason::PlanError => 4,
            FailReason::QtyCheck => 5,
            FailReason::GrantRefused => 6,
            FailReason::SetupFailed => 7,
            FailReason::StaleReference => 8,
            FailReason::SwitchedOff => 9,
            FailReason::ReducedInstead => 10,
            FailReason::NotTakingEffect => 11,
            FailReason::CancelFailed => 12,
            FailReason::CancelUnknown => 13,
            FailReason::FillsNotSeen => 14,
            FailReason::FilledAfterClose => 15,
            FailReason::NotFoundOpen => 16,
            FailReason::LostConnection => 17,
        }
    }

    /// The line for the panel. Fixed text: nothing from the exchange is in it.
    pub fn text(self) -> &'static str {
        match self {
            FailReason::Rejected => {
                "The exchange refused a protective order. The position may not be protected."
            }
            FailReason::RateLimited => {
                "The exchange is limiting requests (HTTP 429 or 418), so a protective order was \
                 not placed. It was not sent again. The position may not be protected."
            }
            FailReason::NotSent => {
                "A protective order was not sent: the app's own checks or the connection stopped \
                 it. The position may not be protected."
            }
            FailReason::Unknown => {
                "The exchange did not confirm a protective order. It was not sent again; check \
                 the open orders."
            }
            FailReason::PlanError => {
                "The protective prices could not be worked out from the fill and the account's \
                 settings. The position may not be protected."
            }
            FailReason::QtyCheck => {
                "A protective order's quantity does not fit the symbol's size rules. The \
                 position may not be protected."
            }
            FailReason::GrantRefused => {
                "The confirmation of the entry did not cover a protective order, so it was \
                 refused. The position may not be protected."
            }
            FailReason::SetupFailed => {
                "A protective order could not be started (credentials, trading rules or the \
                 connection). The position may not be protected."
            }
            FailReason::StaleReference => {
                "The market price a protective order is checked against was too old, so it was \
                 not sent. The position may not be protected."
            }
            FailReason::SwitchedOff => SWITCHED_OFF,
            FailReason::ReducedInstead => REDUCED_INSTEAD,
            FailReason::NotTakingEffect => {
                "A protective order triggered but did not take effect (refused, expired or \
                 cancelled). The position may still be open and not protected."
            }
            FailReason::CancelFailed => {
                "A protective order could not be cancelled and may still stand. It was not tried \
                 again; check the open orders and cancel it by hand."
            }
            FailReason::CancelUnknown => {
                "The exchange did not confirm the cancel of a protective order; it may still \
                 stand. It was not tried again; check the open orders and cancel it by hand."
            }
            FailReason::FillsNotSeen => FILLS_NOT_SEEN,
            FailReason::FilledAfterClose => FILLED_AFTER_CLOSE,
            FailReason::NotFoundOpen => NOT_FOUND_OPEN,
            FailReason::LostConnection => LOST_CONNECTION,
        }
    }

    /// Does this leave a position without its stop-loss (a new stop is worth offering)? The
    /// cancel reasons do not: the order that could not be cancelled still stands.
    pub fn may_leave_bare(self) -> bool {
        !matches!(self, FailReason::CancelFailed | FailReason::CancelUnknown)
    }
}

/// A market price the stop is checked against, and how old it is.
///
/// The app has no per-symbol live price here: the quote is the last price of the 24 h ticker
/// list, fetched by the sidebar every 13 s while the list is shown and every 300 s while it is
/// hidden, and stamped (per venue) when a fetch succeeds. A failed fetch keeps the old value, so
/// the age is what tells a kept value from a fresh one. 330 s is the hidden interval plus slack:
/// a shorter limit would stop every automatic stop while the list is hidden. A quote older than
/// this is not used. Before Live opens, a live price feed per symbol should replace it.
pub const REF_PRICE_MAX_AGE_SECS: u64 = 330;

/// The price a stage's trigger side is checked against.
///
/// - `Ok(Some(p))`: a quote that is fresh enough, for a spec that watches the last price (the
///   quote is the last price; there is no mark price source, so a spec that watches the mark price
///   gets none).
/// - `Ok(None)`: no usable source; the fill price stands in (the stop is then checked against the
///   fill only, see `run`).
/// - `Err`: a quote exists but is older than the limit, so it is not used and the stage is not
///   sent (the failure goes to the panel for a manual retry).
pub fn reference_price(
    basis: exchange::trade::TriggerBasis,
    quote: Option<(Decimal, Duration)>,
) -> Result<Option<Decimal>, FailReason> {
    use exchange::trade::TriggerBasis;
    match (basis, quote) {
        (_, None) | (TriggerBasis::Mark, _) => Ok(None),
        (TriggerBasis::Last, Some((price, age))) => {
            if age > Duration::from_secs(REF_PRICE_MAX_AGE_SECS) {
                Err(FailReason::StaleReference)
            } else {
                Ok(Some(price))
            }
        }
    }
}

/// One thing that went wrong with an entry's protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Failure {
    reason: FailReason,
    /// The order it concerns, when it is about one placed order.
    order: Option<IssuedRef>,
}

/// An entry that needs the user, as the panel shows it. The key is the entry's client id: never
/// printed, used only to name the entry in a click.
#[derive(Clone, PartialEq, Eq)]
pub struct FailureView {
    pub key: String,
    pub account: Uuid,
    pub symbol: String,
    pub reasons: Vec<FailReason>,
    pub dir: Dir,
    /// A stop-loss could be offered again: the position may be bare and no stop of the app
    /// stands for it.
    pub may_retry: bool,
    /// The entry still counts as automatic protection at work (it blocks a new entry).
    pub active: bool,
    /// The entry order may still be open on the exchange: a later fill of it gets no stop-loss
    /// once the entry is removed from the book.
    pub entry_open: bool,
    /// The result of the entry order was never found out: it may still be open.
    pub entry_unresolved: bool,
    /// How many protective orders of this entry may still stand on the exchange (open, being
    /// cancelled, or a cancel that failed). Removing the entry stops the app watching them.
    pub standing: usize,
}

impl fmt::Debug for FailureView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FailureView({}, {} reasons, <redacted>)",
            self.symbol,
            self.reasons.len()
        )
    }
}

/// Why an entry could not be removed by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DismissError {
    NotFound,
    /// The entry is being sent or its orders are going out now.
    Busy,
}

// ── The grant, shared with the task that sends ──────────────────────────

/// The grant of one entry. `AutoGrant` itself cannot be cloned; this handle only lets the task
/// that sends reach the one grant, under a lock held for no longer than one check.
#[derive(Clone)]
pub struct GrantCell(Arc<Mutex<AutoGrant>>);

impl GrantCell {
    pub fn new(grant: AutoGrant) -> Self {
        Self(Arc::new(Mutex::new(grant)))
    }

    fn lock(&self) -> MutexGuard<'_, AutoGrant> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for GrantCell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GrantCell(<redacted>)")
    }
}

/// An entry's client id. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct EntryKey(String);

impl fmt::Debug for EntryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EntryKey(<redacted>)")
    }
}

/// What the confirmation of an entry hands over.
pub struct AutoInit {
    pub account: Uuid,
    pub symbol: String,
    pub spec: ProtectSpec,
    pub mode: PositionMode,
    pub grant: AutoGrant,
}

/// The grant of a confirmed Binance futures entry, made from the intent that is about to be
/// sent (never from strings typed elsewhere). `Err` says why the entry must not be sent with
/// automatic protection: the caller then sends nothing.
pub fn mint(
    intent: &OrderIntent,
    account: Uuid,
    symbol: &str,
    spec: &ProtectSpec,
    mode: PositionMode,
    now_ms: i64,
) -> Result<AutoInit, String> {
    let count = |stage| stage_roles(spec, stage);
    let all: Vec<Role> = count(Stage::Stop)
        .into_iter()
        .chain(count(Stage::Rest))
        .collect();
    let roles = GrantRoles {
        stop: all.contains(&Role::StopLoss),
        targets: u8::try_from(all.iter().filter(|r| **r == Role::TakeProfit).count())
            .unwrap_or(u8::MAX),
        trailing: all.contains(&Role::Trailing),
    };
    let grant = AutoGrant::from_entry(intent, roles, spec, GRANT_VALID_MS, now_ms)
        .map_err(|e| format!("Automatic protection is not available for this order: {e}."))?;
    Ok(AutoInit {
        account,
        symbol: symbol.to_string(),
        spec: spec.clone(),
        mode,
        grant,
    })
}

// ── Jobs and their results ──────────────────────────────────────────────

pub struct Planned {
    role: Role,
    issued: IssuedRef,
    id: ClientId,
}

pub enum Work {
    Place { stage: Stage, orders: Vec<Planned> },
    Cancel { which: Vec<IssuedRef> },
}

pub struct AutoJob {
    pub account: Uuid,
    pub symbol: String,
    key: EntryKey,
    work: Work,
    grant: GrantCell,
    spec: ProtectSpec,
    dir: Dir,
    mode: PositionMode,
    avg_price: Decimal,
    filled: Decimal,
}

impl AutoJob {
    /// Does the job place orders (and so need a price to check the trigger against)?
    pub fn places(&self) -> bool {
        matches!(self.work, Work::Place { .. })
    }

    /// Which price the entry's triggers watch.
    pub fn trigger_basis(&self) -> exchange::trade::TriggerBasis {
        self.spec.trigger_price
    }
}

impl fmt::Debug for AutoJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match &self.work {
            Work::Place { stage, .. } => format!("Place({stage:?})"),
            Work::Cancel { .. } => "Cancel".to_string(),
        };
        write!(f, "AutoJob({kind}, <redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepResult {
    Ok,
    /// A cancel found nothing to cancel (already gone, or refused): not an error.
    Gone,
    /// Refused or not sent; the reason is kept for the panel, the text is for the note only.
    Failed(FailReason, String),
    /// No answer, and the lookup did not find the order either. Nothing was resent.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Which {
    Placed(IssuedRef),
    Cancelled(IssuedRef),
}

#[derive(Debug, Clone)]
pub struct AutoSummary {
    pub key: EntryKey,
    pub account: Uuid,
    pub symbol: String,
    pub steps: Vec<(Which, StepResult)>,
}

impl AutoSummary {
    /// An order whose result is unknown was left (its record is in the unknown list).
    pub fn has_unknown(&self) -> bool {
        self.steps
            .iter()
            .any(|(_, r)| matches!(r, StepResult::Unknown))
    }
}

/// A job that could not even start (no account, no connection): every step failed, nothing sent.
pub fn failed_summary(job: &AutoJob, reason: FailReason, why: &str) -> AutoSummary {
    AutoSummary {
        key: job.key.clone(),
        account: job.account,
        symbol: job.symbol.clone(),
        steps: all_failed(&job.work, reason, why),
    }
}

fn all_failed(work: &Work, reason: FailReason, why: &str) -> Vec<(Which, StepResult)> {
    let failed = || StepResult::Failed(reason, why.to_string());
    match work {
        Work::Place { orders, .. } => orders
            .iter()
            .map(|o| (Which::Placed(o.issued), failed()))
            .collect(),
        Work::Cancel { which } => which
            .iter()
            .map(|r| (Which::Cancelled(*r), failed()))
            .collect(),
    }
}

// ── The state machine ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    /// The entry request is on its way; fills wait.
    Sending,
    /// The exchange has the entry.
    Stands,
    /// The result is unknown: nothing is attached until it is found.
    Unresolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StageState {
    NotWanted,
    Todo,
    Running,
    Done,
    Failed,
}

impl StageState {
    fn terminal(self) -> bool {
        matches!(
            self,
            StageState::NotWanted | StageState::Done | StageState::Failed
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Live {
    Open,
    /// Marked by an event; the next poll makes the cancel.
    ToCancel,
    Cancelling,
    Cancelled,
    /// The cancel was refused or its result is unknown: the order may still stand. Never
    /// retried; the panel says so.
    CancelFailed,
    /// The exchange says the order triggered; it has not finished yet (a stop-market order
    /// can still be refused or expire after it).
    Triggered,
    /// The triggered order finished (it filled).
    Finished,
    Failed,
}

impl Live {
    fn holds_the_position(self) -> bool {
        matches!(self, Live::Open | Live::ToCancel | Live::Cancelling)
    }
}

struct Placed {
    which: IssuedRef,
    id: String,
    role: Role,
    state: Live,
    /// The exchange did not say whether it took the order (no resend, no lookup answer).
    unsure: bool,
}

struct AutoEntry {
    account: Uuid,
    symbol: String,
    dir: Dir,
    spec: ProtectSpec,
    mode: PositionMode,
    grant: GrantCell,
    sent: Sent,
    filled: Decimal,
    /// Sum of price × quantity of the counted fills.
    cost: Decimal,
    /// The entry order has finished (filled, or cancelled/expired with what it filled).
    complete: bool,
    /// The quantity a `Filled` update says the order holds: `complete` waits for the fills.
    expected: Option<Decimal>,
    stop: StageState,
    rest: StageState,
    placed: Vec<Placed>,
    busy: bool,
    /// The whole-position stop-loss fired: the position is closed.
    closed: bool,
    reported: bool,
    /// "The stop-loss was kept" was written once for this entry.
    kept_noted: bool,
    /// "The entry filled again after the stop closed the position" was written once.
    closed_fill_noted: bool,
    /// The entry was given up with a note (fills not seen, connection lost): no more is sent for
    /// it, and it does not count as active work.
    attention: bool,
    /// What went wrong, kept until the user has handled the entry (the notes list evicts).
    failures: Vec<Failure>,
    /// When `expected` was first seen at a poll (ms): a fill that is late is told from one that
    /// was lost by the wait that follows ([`FILLS_WAIT_MS`]).
    expected_at: Option<i64>,
    /// The system notification for this entry went out (once per entry, never repeated).
    notified: bool,
}

/// What [`AutoBook::adopt_replacement`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adoption {
    /// The book follows the new order now.
    Adopted,
    /// The old order is not one the book placed: nothing to do.
    NotOurs,
    /// The old order was one the book placed, but its entry has left the book (it was over as
    /// far as the book could tell): the new order is not followed.
    Dropped,
    /// The book placed the old order but could not follow the new one (a note says so).
    Refused,
}

/// What a fill did to the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillVerdict {
    Counted,
    /// Not an entry the app owes protection for.
    NotOurs,
    /// Not on the side that opens the entry's direction: it does not add to the position.
    WrongSide,
}

#[derive(Default)]
pub struct AutoBook {
    entries: BTreeMap<String, AutoEntry>,
    /// The entry being sent for an (account, symbol).
    sending: HashMap<(Uuid, String), String>,
    /// Client id of a placed order → its entry.
    by_placed: HashMap<String, String>,
    /// (account, text, is it a failure of the protection). A failure's text is also kept on its
    /// entry; the panel shows those from the entry, and only the other notes from here.
    notes: VecDeque<(Uuid, String, bool)>,
    finished: Vec<String>,
    /// Per account, the last algo events (client id, status) of orders the book did not know:
    /// the events of a replacement that came before the answer of the replace. Bounded, never
    /// logged, never saved.
    unclaimed: HashMap<Uuid, VecDeque<(String, OrderStatus)>>,
    /// Client ids of placed orders of entries that were dropped (bounded, oldest first).
    forgotten: VecDeque<(Uuid, String)>,
    /// Old client ids of placed orders whose replace is out (old id → account). Set when the
    /// replace is sent and cleared when it ends, whatever the result: while an id is here its
    /// entry is not dropped and a cancel of it is not sent (the replace cancels it, and the
    /// answer decides what the new order is). One replace runs per account, so it stays small.
    replacing: HashMap<String, Uuid>,
}

impl fmt::Debug for AutoBook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AutoBook({} entries)", self.entries.len())
    }
}

fn dir_of_status_done(status: OrderStatus) -> bool {
    matches!(
        status,
        OrderStatus::Filled | OrderStatus::Canceled | OrderStatus::Expired | OrderStatus::Rejected
    )
}

/// What the book reads from the app each time it looks for work.
pub struct PollEnv<'a> {
    /// Whether the account's auto-attach is on (S3), read at the moment of sending.
    pub switch: &'a dyn Fn(Uuid) -> bool,
    /// What the account holds on a symbol.
    pub held: &'a dyn Fn(Uuid, &str) -> Holdings,
    /// A monotonic clock in milliseconds (from `Instant`, not the wall clock), for waits that a
    /// change of the system time must not shorten or stretch.
    pub mono_ms: i64,
}

/// What the book reads from the first snapshot after a reconnect (M4-4).
pub struct ReconnectView<'a> {
    /// Is the entry still open on the symbol, on its opening side, under this client id? An open
    /// order whose row carries no readable client id still counts when symbol and side match
    /// (nothing is proven gone from a missing field).
    pub resting: &'a dyn Fn(&str, Dir, &str) -> bool,
    /// The directions of the positions held on the symbol.
    pub held: &'a dyn Fn(&str) -> Vec<Dir>,
}

impl AutoBook {
    fn note(&mut self, account: Uuid, text: impl Into<String>) {
        self.push_note(account, text.into(), false);
    }

    fn push_note(&mut self, account: Uuid, text: String, failure: bool) {
        self.notes.push_back((account, text, failure));
        while self.notes.len() > MAX_NOTES {
            self.notes.pop_front();
        }
    }

    fn add_failure(e: &mut AutoEntry, reason: FailReason, order: Option<IssuedRef>) {
        let failure = Failure { reason, order };
        if !e.failures.contains(&failure) {
            e.failures.push(failure);
        }
    }

    /// Every note of the account (failures included).
    #[cfg(test)]
    pub fn notes_for(&self, account: Uuid) -> Vec<String> {
        self.notes
            .iter()
            .filter(|(a, _, _)| *a == account)
            .map(|(_, t, _)| t.clone())
            .collect()
    }

    /// The notes that are not failures of an entry (those are shown from the entry).
    pub fn info_notes_for(&self, account: Uuid) -> Vec<String> {
        self.notes
            .iter()
            .filter(|(a, _, failure)| *a == account && !*failure)
            .map(|(_, t, _)| t.clone())
            .collect()
    }

    pub fn clear_notes(&mut self, account: Uuid) {
        self.notes.retain(|(a, _, _)| *a != account);
    }

    /// Is protection still being worked on for an entry of the account on this symbol (M3)? Only
    /// entries with work left count: a send in flight, protective orders still to place or
    /// still standing. An entry with an unknown result, an order that could not be cancelled, or
    /// one that was given up with a note does not: the user has been told to check, and it must
    /// not block the symbol for the rest of the session.
    pub fn is_active(&self, account: Uuid, symbol: &str) -> bool {
        self.entries.values().any(|e| {
            e.account == account && e.symbol.eq_ignore_ascii_case(symbol) && Self::counts_active(e)
        })
    }

    fn counts_active(e: &AutoEntry) -> bool {
        !e.attention
            && match e.sent {
                Sent::Sending => true,
                Sent::Unresolved => false,
                Sent::Stands => {
                    e.busy
                        || !e.stop.terminal()
                        || !e.rest.terminal()
                        || e.placed
                            .iter()
                            .any(|p| p.state.holds_the_position() && !p.unsure)
                }
            }
    }

    /// A stop-loss of the app that stands, or is about to take effect or be cancelled (or whose
    /// cancel failed): a new whole-position stop would be a second one.
    fn stop_stands(e: &AutoEntry) -> bool {
        e.placed.iter().any(|p| {
            p.role == Role::StopLoss
                && matches!(
                    p.state,
                    Live::Open
                        | Live::ToCancel
                        | Live::Cancelling
                        | Live::CancelFailed
                        | Live::Triggered
                )
        })
    }

    /// The entries of the account the user has to look at (condition 65): every one with a
    /// failure written down, and every one that still counts as protection at work (it blocks
    /// a new entry on its symbol, and a stop left open after the position was closed by hand has
    /// no failure of its own).
    pub fn failures_for(&self, account: Uuid) -> Vec<FailureView> {
        self.entries
            .iter()
            .filter(|(_, e)| e.account == account)
            .filter_map(|(key, e)| {
                let active = Self::counts_active(e);
                if e.failures.is_empty() && !active {
                    return None;
                }
                let reasons: Vec<FailReason> = e.failures.iter().map(|f| f.reason).collect();
                let may_retry = e.sent == Sent::Stands
                    && reasons.iter().any(|r| r.may_leave_bare())
                    && !Self::stop_stands(e);
                Some(FailureView {
                    key: key.clone(),
                    account,
                    symbol: e.symbol.clone(),
                    dir: e.dir,
                    reasons,
                    may_retry,
                    active,
                    entry_open: e.sent == Sent::Stands && !e.complete,
                    entry_unresolved: e.sent == Sent::Unresolved,
                    standing: e
                        .placed
                        .iter()
                        .filter(|p| {
                            matches!(
                                p.state,
                                Live::Open | Live::ToCancel | Live::Cancelling | Live::CancelFailed
                            )
                        })
                        .count(),
                })
            })
            .collect()
    }

    /// Does the book already speak for the account's position of `dir` on `symbol` with a failure
    /// that left it bare (the failure banner shows it)?
    pub fn covers_part(&self, account: Uuid, symbol: &str, dir: Dir) -> bool {
        self.entries.values().any(|e| {
            e.account == account
                && e.dir == dir
                && e.symbol.eq_ignore_ascii_case(symbol)
                && e.failures.iter().any(|f| f.reason.may_leave_bare())
        })
    }

    /// Is this client id one of an entry of the book, or of an order the book placed?
    pub fn knows_client_id(&self, client_id: &str) -> bool {
        self.entries.contains_key(client_id) || self.by_placed.contains_key(client_id)
    }

    /// Is an automatic job working on this order right now, so that it must not be replaced
    /// (f3c)? Only what the job is doing counts, not who placed the order:
    ///
    /// - the entry order itself is the job's (it is never a protective order to replace);
    /// - the entry has a job in flight (`busy`: protective orders going out, or cancels going
    ///   out), which reads and changes the placed orders;
    /// - the order is not standing steadily: it is marked to be cancelled, being cancelled, its
    ///   cancel failed, it triggered or finished, or it was cancelled or refused.
    ///
    /// A protective order the book placed that is `Open` while no job runs is stable, and may be
    /// dragged: [`Self::adopt_replacement`] then hands the new order to the book.
    pub fn is_working_on(&self, client_id: &str) -> bool {
        if self.entries.contains_key(client_id) {
            return true;
        }
        let Some(e) = self
            .by_placed
            .get(client_id)
            .and_then(|key| self.entries.get(key))
        else {
            return false;
        };
        e.busy
            || e.placed
                .iter()
                .find(|p| p.id == client_id)
                .is_none_or(|p| p.state != Live::Open)
    }

    /// A confirmed replace of a protective order the book placed completed (the exchange took
    /// the cancel of the old order and the new order): the entry's record, and the grant that
    /// builds the sibling cancels, now name the new order, in the old one's role. Everything is
    /// checked before anything is changed; a refusal leaves book and grant as they were, writes
    /// a note, and the new order is then an order the app does not follow (the orphan list
    /// shows it).
    ///
    /// The old order is expected `Open` (a stable order), `ToCancel` (a sibling fired during the
    /// replace: the next job cancels the new order), or `Cancelled` (the exchange's cancel event
    /// of the replace itself arrived before the answer, and only the grant, which did not
    /// cancel it, vouches for it). `Cancelling` and the rest are refused: a job is cancelling
    /// the old id, or the order is over.
    pub fn adopt_replacement(&mut self, account: Uuid, r: &Replacement) -> Adoption {
        let Some(entry_id) = self.by_placed.get(r.old_client_id()).cloned() else {
            return if (self.forgotten.iter()).any(|(a, f)| *a == account && f == r.old_client_id())
            {
                Adoption::Dropped
            } else {
                Adoption::NotOurs
            };
        };
        let Some(e) = self
            .entries
            .get_mut(&entry_id)
            .filter(|e| e.account == account)
        else {
            return Adoption::NotOurs;
        };
        let Some(idx) = e.placed.iter().position(|p| p.id == r.old_client_id()) else {
            return Adoption::NotOurs;
        };
        let role = e.placed[idx].role;
        // Decided now, with the state the events left: the same rule the triggers use.
        let swept = Self::swept(e, role);
        let refused = match e.placed[idx].state {
            Live::Open | Live::ToCancel | Live::Cancelled => {
                let mut grant = e.grant.lock();
                // Read first: the grant is changed only when it and the book name the same slot.
                if grant.issued_ref_of(r.old_client_id()) != Some(e.placed[idx].which) {
                    Some("the records do not agree".to_string())
                } else {
                    match grant.adopt_replacement(r) {
                        Ok(_) => None,
                        Err(refusal) => Some(refusal.to_string()),
                    }
                }
            }
            _ => Some("a job is working on the order".to_string()),
        };
        if let Some(why) = refused {
            let text = format!(
                "{}: the replaced {} is not followed by the automatic protection ({why}). Check \
                 the open orders.",
                e.symbol,
                role_word(Some(role))
            );
            self.note(account, text);
            return Adoption::Refused;
        }
        let p = &mut e.placed[idx];
        p.id = r.new_client_id().to_string();
        if swept && matches!(p.state, Live::Open | Live::Cancelled) {
            // An order that fired while the replace was out took this one with it: the next
            // poll cancels the new order (its cancel event may have turned the old one to
            // `Cancelled` before the sweep looked, so the sweep is decided here again).
            p.state = Live::ToCancel;
        } else if p.state == Live::Cancelled {
            // The exchange's cancel of the old order made it `Cancelled`; the new order stands.
            p.state = Live::Open;
        }
        self.by_placed.remove(r.old_client_id());
        self.by_placed
            .insert(r.new_client_id().to_string(), entry_id);
        self.replay_unclaimed(account, r.new_client_id());
        Adoption::Adopted
    }

    /// A replace of `old` is about to go out: if the book placed that order, it is not
    /// cancelled and its entry is not dropped until [`Self::replace_ended`]. The book is keyed
    /// by the client id, never the exchange id, so the id is taken from the row here (a row
    /// without one is not recorded: it cannot be a replace the book can claim). Only book
    /// state changes here; nothing is sent.
    pub fn replace_begun(&mut self, account: Uuid, old: &exchange::trade::OpenOrder) {
        let Some(client_id) = old.client_id.as_deref() else {
            return;
        };
        let ours = self
            .by_placed
            .get(client_id)
            .and_then(|k| self.entries.get(k))
            .is_some_and(|e| e.account == account);
        if ours {
            self.replacing.insert(client_id.to_string(), account);
        }
    }

    /// The replace of this account ended (any result, or a task that gave no answer): its
    /// orders are handled by the usual paths again, unless the answer is adopted next. By
    /// account, so that a result without a client id (or a task that panicked) clears it too;
    /// one account has at most one replace running.
    pub fn replace_ended(&mut self, account: Uuid) {
        self.replacing.retain(|_, a| *a != account);
    }

    /// Keep an algo event of an order the book does not know (see [`MAX_UNCLAIMED`]). Only
    /// events that can change a state, and only for ids shaped like the app's.
    fn hold_unclaimed(&mut self, account: Uuid, id: &str, status: OrderStatus) {
        if matches!(status, OrderStatus::New | OrderStatus::PartiallyFilled)
            || !exchange::trade::looks_like_app_id(id)
        {
            return;
        }
        let held = self.unclaimed.entry(account).or_default();
        held.push_back((id.to_string(), status));
        while held.len() > MAX_UNCLAIMED {
            held.pop_front();
        }
    }

    /// The events kept for a replacement the book now follows, in the order they came.
    fn replay_unclaimed(&mut self, account: Uuid, new_id: &str) {
        let Some(held) = self.unclaimed.get_mut(&account) else {
            return;
        };
        let (mine, rest): (VecDeque<_>, VecDeque<_>) = std::mem::take(held)
            .into_iter()
            .partition(|(id, _)| id == new_id);
        *held = rest;
        for (id, status) in mine {
            self.on_algo_update(account, &id, status);
        }
    }

    /// The accounts of entries that have a failure the user has not been told about by a system
    /// notification yet, once per entry (the entry is marked).
    pub fn take_to_notify(&mut self) -> Vec<Uuid> {
        let mut accounts = Vec::new();
        for e in self.entries.values_mut() {
            if !e.failures.is_empty() && !e.notified {
                e.notified = true;
                accounts.push(e.account);
            }
        }
        accounts
    }

    /// The user says an entry is handled: it leaves the book (a failure, or protection still
    /// "at work" that the user knows to be over). Refused while its entry is being sent or its
    /// orders are going out. It touches nothing else: an unknown result of the entry stays
    /// blocked by its own record, which only the manual release or discard (typed phrase)
    /// can clear.
    pub fn dismiss(&mut self, account: Uuid, key: &str) -> Result<String, DismissError> {
        let e = self
            .entries
            .get(key)
            .filter(|e| e.account == account)
            .ok_or(DismissError::NotFound)?;
        if e.sent == Sent::Sending || e.busy {
            return Err(DismissError::Busy);
        }
        let symbol = e.symbol.clone();
        self.entry_gone(key);
        Ok(symbol)
    }

    /// A whole-position stop was accepted for the account's position of `dir` on `symbol`
    /// (a retry, or one the user placed by hand): the failures that left it bare are cleared, and
    /// the entry's own stop, if it was still to be sent, is given up, so the entry's grant is
    /// never used for a stop again. The entry stays in the book while anything of it stands.
    pub fn protection_added(&mut self, account: Uuid, symbol: &str, dir: Dir) {
        for e in self.entries.values_mut().filter(|e| {
            e.account == account && e.dir == dir && e.symbol.eq_ignore_ascii_case(symbol)
        }) {
            e.failures.retain(|f| !f.reason.may_leave_bare());
            // A second whole-position stop would only be refused; the entry's own stop is not
            // sent. Its take-profit stage is left alone.
            if e.stop == StageState::Todo {
                e.stop = StageState::Failed;
            }
        }
        self.housekeeping();
    }

    /// The first snapshot after a reconnect (M4-4): an entry that was open when the connection
    /// dropped and is neither resting nor complete may have filled while no event reached us.
    /// A note is written and nothing is sent (a resend would need a new confirmation).
    pub fn after_reconnect(&mut self, account: Uuid, view: &ReconnectView<'_>) {
        let mut notes = Vec::new();
        let mut reported = Vec::new();
        for (id, e) in &mut self.entries {
            if e.account != account || e.sent != Sent::Stands || e.complete || e.attention {
                continue;
            }
            if (view.resting)(&e.symbol, e.dir, id) {
                continue;
            }
            let short = e
                .expected
                .is_some_and(|x| e.filled.checked_cmp(&x) == Ok(std::cmp::Ordering::Less));
            let reason = if short || (view.held)(&e.symbol).contains(&e.dir) {
                FailReason::LostConnection
            } else {
                // Not resting does not prove it is gone (a snapshot taken before the send
                // answered looks the same), so the entry is not dropped: it stops blocking the
                // symbol and the user is told. Protective orders already placed stay in the book.
                FailReason::NotFoundOpen
            };
            Self::give_up_rest(e);
            Self::add_failure(e, reason, None);
            if !e.reported {
                e.reported = true;
                reported.push(id.clone());
            }
            notes.push(format!("{}: {}", e.symbol, reason.text()));
        }
        self.finished.extend(reported);
        for n in notes {
            self.push_note(account, n, true);
        }
    }

    /// Like `give_up`, but for a reconnect, where a fill may only be late: the take-profit and
    /// trailing stage is failed, the stop-loss stays `Todo`. It goes out only when a fill of
    /// this very entry is seen (a closePosition stop also covers fills that were missed).
    fn give_up_rest(e: &mut AutoEntry) {
        if e.rest == StageState::Todo {
            e.rest = StageState::Failed;
        }
        e.attention = true;
    }

    /// The entry was confirmed and goes out now.
    pub fn register(&mut self, entry_id: String, init: AutoInit) {
        let AutoInit {
            account,
            symbol,
            spec,
            mode,
            grant,
        } = init;
        let dir = grant.dir();
        let state = |wanted: bool| {
            if wanted {
                StageState::Todo
            } else {
                StageState::NotWanted
            }
        };
        let entry = AutoEntry {
            account,
            symbol: symbol.clone(),
            dir,
            mode,
            grant: GrantCell::new(grant),
            sent: Sent::Sending,
            filled: Decimal::ZERO,
            cost: Decimal::ZERO,
            complete: false,
            expected: None,
            stop: state(!stage_roles(&spec, Stage::Stop).is_empty()),
            rest: state(!stage_roles(&spec, Stage::Rest).is_empty()),
            spec,
            placed: Vec::new(),
            busy: false,
            closed: false,
            reported: false,
            kept_noted: false,
            closed_fill_noted: false,
            attention: false,
            failures: Vec::new(),
            expected_at: None,
            notified: false,
        };
        self.sending.insert((account, symbol), entry_id.clone());
        self.entries.insert(entry_id, entry);
    }

    /// The send of the entry ended.
    pub fn settle(&mut self, account: Uuid, symbol: &str, result: &SendResult) {
        let Some(id) = self.sending.remove(&(account, symbol.to_string())) else {
            return;
        };
        match crate::protect_attach::retention(result) {
            crate::protect_attach::Retain::Remove => self.drop_entry(&id),
            crate::protect_attach::Retain::Keep => {
                if let Some(e) = self.entries.get_mut(&id) {
                    e.sent = if *result == SendResult::Unresolved {
                        Sent::Unresolved
                    } else {
                        Sent::Stands
                    };
                }
            }
        }
    }

    /// The lookup found the entry that had an unknown result.
    pub fn entry_stands(&mut self, client_id: &str) {
        if let Some(e) = self.entries.get_mut(client_id)
            && e.sent == Sent::Unresolved
        {
            e.sent = Sent::Stands;
        }
    }

    /// The entry is known not to stand: forget it (what was placed already stays on the
    /// exchange and keeps cancelling by events only while the entry is known, so a placed order
    /// of an entry that is dropped here has none: there is none before a fill).
    pub fn entry_gone(&mut self, client_id: &str) {
        self.drop_entry(client_id);
        self.sending.retain(|_, id| id != client_id);
    }

    fn drop_entry(&mut self, id: &str) {
        if let Some(e) = self.entries.remove(id) {
            for p in e.placed {
                self.by_placed.remove(&p.id);
                self.forgotten.push_back((e.account, p.id));
                while self.forgotten.len() > MAX_FORGOTTEN {
                    self.forgotten.pop_front();
                }
            }
            if !e.reported {
                self.finished.push(id.to_string());
            }
        }
    }

    pub fn remove_account(&mut self, account: Uuid) {
        let ids: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| e.account == account)
            .map(|(k, _)| k.clone())
            .collect();
        for id in ids {
            self.drop_entry(&id);
        }
        self.sending.retain(|(a, _), _| *a != account);
        self.unclaimed.remove(&account);
        self.forgotten.retain(|(a, _)| *a != account);
        self.replacing.retain(|_, a| *a != account);
        self.clear_notes(account);
    }

    /// Entries whose attach work ended (every stage done or failed), once each: the table of
    /// pending protection forgets them.
    pub fn take_finished(&mut self) -> Vec<String> {
        std::mem::take(&mut self.finished)
    }

    /// A fill of the account. The caller has already dropped a repeated trade id, and passes
    /// only fills that have both ids.
    pub fn on_fill(
        &mut self,
        account: Uuid,
        client_id: &str,
        side: Option<Side>,
        qty: Decimal,
        price: Decimal,
    ) -> FillVerdict {
        let Some(e) = self
            .entries
            .get_mut(client_id)
            .filter(|e| e.account == account)
        else {
            return FillVerdict::NotOurs;
        };
        // B1: only a fill that adds to the entry's direction counts.
        if side != Some(e.dir.opening_side()) || !qty.is_positive() || !price.is_positive() {
            return FillVerdict::WrongSide;
        }
        let (Ok(filled), Ok(value)) = (
            e.filled.checked_add(&qty),
            price.checked_mul(&qty).and_then(|v| e.cost.checked_add(&v)),
        ) else {
            return FillVerdict::WrongSide;
        };
        // The grant takes fills of its own entry only; a refusal keeps the two in step.
        if !e.grant.lock().add_fill(client_id, qty) {
            return FillVerdict::NotOurs;
        }
        e.filled = filled;
        e.cost = value;
        // A fill is progress: the wait for the rest starts again.
        e.expected_at = None;
        let again = e.closed && !e.closed_fill_noted;
        e.closed_fill_noted |= e.closed;
        let symbol = e.symbol.clone();
        Self::check_complete(e);
        if again {
            Self::add_failure(e, FailReason::FilledAfterClose, None);
            self.push_note(account, format!("{symbol}: {FILLED_AFTER_CLOSE}"), true);
        }
        FillVerdict::Counted
    }

    fn check_complete(e: &mut AutoEntry) {
        if let Some(total) = e.expected
            && matches!(
                e.filled.checked_cmp(&total),
                Ok(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
            )
        {
            e.complete = true;
        }
    }

    /// An order update of the account: the entry finishing, or a protective order changing.
    pub fn on_order(&mut self, account: Uuid, update: &OrderUpdate) {
        let Some(id) = update.client_id.as_deref() else {
            return;
        };
        match update.category {
            OrderCategory::Plain => self.on_entry_update(account, id, update),
            OrderCategory::Algo => self.on_algo_update(account, id, update.status),
        }
    }

    fn on_entry_update(&mut self, account: Uuid, id: &str, update: &OrderUpdate) {
        if !dir_of_status_done(update.status) {
            return;
        }
        let Some(e) = self.entries.get_mut(id).filter(|e| e.account == account) else {
            return;
        };
        // What the exchange says the order holds, when that is more than the fills seen.
        let unseen = update
            .filled_qty
            .filter(|q| q.checked_cmp(&e.filled) == Ok(std::cmp::Ordering::Greater));
        let mut note = None;
        let mut report = false;
        if update.status == OrderStatus::Filled {
            // The fills may still be on their way: wait until they add up.
            e.expected = update.filled_qty.or(update.qty).or(Some(e.filled));
        } else {
            // The order is over, so nothing more will come: it is complete with the fills seen.
            e.expected = Some(e.filled);
            if unseen.is_some() {
                // Fills were not seen (a gap in the stream): the position is bigger than the
                // book says. Never removed quietly; what is known is still protected.
                note = Some(format!("{}: {FILLS_NOT_SEEN}", e.symbol));
                Self::add_failure(e, FailReason::FillsNotSeen, None);
                if !e.filled.is_positive() {
                    // R3: with no fill seen the take-profit stage is given up but the stop is
                    // kept for a fill of this very entry that may still arrive (a closePosition
                    // stop covers the part that was missed). The entry is reported, and stays in
                    // the book (it has a failure) until the user has handled it.
                    Self::give_up_rest(e);
                    if !e.reported {
                        e.reported = true;
                        report = true;
                    }
                }
            }
        }
        Self::check_complete(e);
        let account = e.account;
        // An entry that finished with nothing filled has nothing to protect.
        let empty = e.complete && !e.filled.is_positive() && unseen.is_none();
        if report {
            self.finished.push(id.to_string());
        }
        if let Some(n) = note {
            self.push_note(account, n, true);
        }
        if empty {
            let key = id.to_string();
            self.entry_gone(&key);
        }
    }

    fn on_algo_update(&mut self, account: Uuid, id: &str, status: OrderStatus) {
        let Some(entry_id) = self.by_placed.get(id).cloned() else {
            self.hold_unclaimed(account, id, status);
            return;
        };
        let Some(e) = self
            .entries
            .get_mut(&entry_id)
            .filter(|e| e.account == account)
        else {
            return;
        };
        let Some(idx) = e.placed.iter().position(|p| p.id == id) else {
            return;
        };
        let role = e.placed[idx].role;
        let state = e.placed[idx].state;
        let mut note = None;
        match status {
            OrderStatus::Triggered => {
                // A repeated event changes nothing.
                if matches!(state, Live::Triggered | Live::Finished | Live::Failed) {
                    return;
                }
                e.placed[idx].state = Live::Triggered;
                if state != Live::Cancelled {
                    Self::triggered(e, idx);
                }
            }
            OrderStatus::Finished | OrderStatus::Filled => {
                if matches!(state, Live::Finished | Live::Failed) {
                    return;
                }
                e.placed[idx].state = Live::Finished;
                // A `Finished` that came without a `Triggered` first does the same work.
                if !matches!(state, Live::Triggered | Live::Cancelled) {
                    Self::triggered(e, idx);
                }
            }
            OrderStatus::Canceled | OrderStatus::Expired | OrderStatus::Rejected => match state {
                // Refused or expired after it triggered: it did not take effect, the position
                // may still be open.
                Live::Triggered if status != OrderStatus::Canceled => {
                    e.placed[idx].state = Live::Failed;
                    if role == Role::StopLoss {
                        e.closed = false;
                    }
                    let which = e.placed[idx].which;
                    Self::add_failure(e, FailReason::NotTakingEffect, Some(which));
                    note = Some(format!(
                        "{}: the {} triggered but did not take effect. The position may still \
                         be open and not protected. Check it.",
                        e.symbol,
                        role_word(Some(role))
                    ));
                }
                // A stop-loss that triggered and was then cancelled did not take effect either.
                Live::Triggered if role == Role::StopLoss => {
                    e.placed[idx].state = Live::Failed;
                    e.closed = false;
                    let which = e.placed[idx].which;
                    Self::add_failure(e, FailReason::NotTakingEffect, Some(which));
                    note = Some(format!(
                        "{}: the {} triggered and was then cancelled. The position may still \
                         be open and not protected. Check it.",
                        e.symbol,
                        role_word(Some(role))
                    ));
                }
                Live::Triggered | Live::Finished | Live::Failed => {}
                // Also the way a cancel that failed (or had no answer) turns out to have worked:
                // the failure of that cancel goes with it.
                _ => {
                    let which = e.placed[idx].which;
                    e.placed[idx].state = Live::Cancelled;
                    e.failures.retain(|f| {
                        !(f.order == Some(which)
                            && matches!(
                                f.reason,
                                FailReason::CancelFailed | FailReason::CancelUnknown
                            ))
                    });
                }
            },
            OrderStatus::New | OrderStatus::PartiallyFilled => {}
        }
        if let Some(n) = note {
            self.push_note(account, n, true);
        }
    }

    /// One placed order triggered. A stop-loss closes the whole position, so the reducing orders
    /// go; a trailing stop takes the take-profit orders with it. A take-profit changes nothing
    /// here. The whole-position stop is never released by this: only [`Self::release_stop`]
    /// does, once the position is known to be gone. Orders leave `Open`, so this runs once.
    fn triggered(e: &mut AutoEntry, idx: usize) {
        if e.placed[idx].role == Role::StopLoss {
            e.closed = true;
        }
        let sweep = Self::sweep_of(e.placed[idx].role);
        for p in e
            .placed
            .iter_mut()
            .filter(|p| p.state == Live::Open && sweep.contains(&p.role))
        {
            p.state = Live::ToCancel;
        }
    }

    /// The roles an order of this role takes with it when it triggers.
    fn sweep_of(role: Role) -> &'static [Role] {
        match role {
            Role::StopLoss => &[Role::TakeProfit, Role::Trailing],
            Role::Trailing => &[Role::TakeProfit],
            Role::TakeProfit | Role::Entry => &[],
        }
    }

    /// Has an order of the entry that triggered already taken the orders of `role` with it?
    /// The same table as [`Self::triggered`], asked after the fact (for an order that only now
    /// joins the book).
    fn swept(e: &AutoEntry, role: Role) -> bool {
        (e.closed && Self::sweep_of(Role::StopLoss).contains(&role))
            || e.placed.iter().any(|q| {
                matches!(q.state, Live::Triggered | Live::Finished)
                    && Self::sweep_of(q.role).contains(&role)
            })
    }

    /// Cancel the whole-position stop (and whatever else stands) when an order has finished, the
    /// list of positions is fresh and shows nothing in the entry's direction. When a position
    /// remains the stop stays (one note, once the reducing orders are all done); when the list
    /// cannot be trusted nothing happens until it can.
    fn release_stop(&mut self, key: &str, env: &PollEnv<'_>) {
        let Some(e) = self.entries.get_mut(key) else {
            return;
        };
        if e.sent != Sent::Stands {
            return;
        }
        let finished = e
            .placed
            .iter()
            .any(|p| p.role != Role::StopLoss && p.state == Live::Finished);
        let stop_open = e
            .placed
            .iter()
            .any(|p| p.role == Role::StopLoss && p.state == Live::Open);
        if !finished || !stop_open {
            return;
        }
        let held = (env.held)(e.account, &e.symbol);
        if held.stale {
            return;
        }
        if !held.dirs.contains(&e.dir) {
            for p in e.placed.iter_mut().filter(|p| p.state == Live::Open) {
                p.state = Live::ToCancel;
            }
            return;
        }
        let reducing_left = e
            .placed
            .iter()
            .any(|p| p.role != Role::StopLoss && p.state == Live::Open);
        if !reducing_left && !e.kept_noted {
            e.kept_noted = true;
            let (account, text) = (e.account, format!("{}: {STOP_KEPT}", e.symbol));
            self.note(account, text);
        }
    }

    /// Jobs that are due now. `switch` says whether the account's auto-attach is still on
    /// (S3): it is read again at the moment of sending, not only when the entry was confirmed.
    pub fn poll(&mut self, now_ms: i64, env: &PollEnv<'_>) -> Vec<AutoJob> {
        let keys: Vec<String> = self.entries.keys().cloned().collect();
        let mut jobs = Vec::new();
        for key in keys {
            self.check_fills_wait(&key, env.mono_ms);
            if let Some(job) = self.poll_entry(&key, now_ms, env) {
                jobs.push(job);
            }
        }
        self.housekeeping();
        jobs
    }

    /// The exchange said the entry `Filled` but its fills have not added up: after
    /// [`FILLS_WAIT_MS`] they are written down as not seen (a lost fill would otherwise leave
    /// the entry waiting for ever, with no stop and no word). What was seen is protected as
    /// usual; with nothing seen the take-profit stage is given up and the stop waits for a fill
    /// of this entry (R3, as in the connection-lost case).
    fn check_fills_wait(&mut self, key: &str, mono_ms: i64) {
        let Some(e) = self.entries.get_mut(key) else {
            return;
        };
        if e.complete || e.attention || e.sent != Sent::Stands || e.expected.is_none() {
            return;
        }
        let since = *e.expected_at.get_or_insert(mono_ms);
        if mono_ms.saturating_sub(since) < FILLS_WAIT_MS {
            return;
        }
        let account = e.account;
        let text = format!("{}: {FILLS_NOT_SEEN}", e.symbol);
        Self::add_failure(e, FailReason::FillsNotSeen, None);
        // Nothing more is waited for: the entry is complete with the fills seen.
        e.expected = Some(e.filled);
        e.expected_at = None;
        let mut report = false;
        if !e.filled.is_positive() {
            Self::give_up_rest(e);
            if !e.reported {
                e.reported = true;
                report = true;
            }
        }
        Self::check_complete(e);
        if report {
            self.finished.push(key.to_string());
        }
        self.push_note(account, text, true);
    }

    fn poll_entry(&mut self, key: &str, now_ms: i64, env: &PollEnv<'_>) -> Option<AutoJob> {
        self.release_stop(key, env);
        let e = self.entries.get_mut(key)?;
        if e.busy || e.sent != Sent::Stands {
            return None;
        }
        let job = |e: &AutoEntry, work: Work| AutoJob {
            account: e.account,
            symbol: e.symbol.clone(),
            key: EntryKey(key.to_string()),
            work,
            grant: e.grant.clone(),
            spec: e.spec.clone(),
            dir: e.dir,
            mode: e.mode,
            avg_price: e
                .cost
                .checked_div_trunc(&e.filled, AVG_SCALE)
                .unwrap_or(Decimal::ZERO),
            filled: e.filled,
        };
        // Cancels first: a fired order leaves the others open until they are cancelled.
        let to_cancel: Vec<IssuedRef> = e
            .placed
            .iter()
            .filter(|p| p.state == Live::ToCancel && !self.replacing.contains_key(&p.id))
            .map(|p| p.which)
            .collect();
        if !to_cancel.is_empty() {
            for p in e
                .placed
                .iter_mut()
                .filter(|p| p.state == Live::ToCancel && !self.replacing.contains_key(&p.id))
            {
                p.state = Live::Cancelling;
            }
            e.busy = true;
            return Some(job(e, Work::Cancel { which: to_cancel }));
        }
        // An order held back for a replace still has its cancel to come: no stage is placed
        // before it (cancel first, then place), so wait for the replace to end.
        if (e.placed.iter())
            .any(|p| p.state == Live::ToCancel && self.replacing.contains_key(&p.id))
        {
            return None;
        }
        if !e.filled.is_positive() {
            return None;
        }
        // The stop-loss closed the position: more fills of the entry get no take-profit or
        // trailing stop (they would be placed and cancelled at once). Fills are noted instead.
        if e.closed && e.rest == StageState::Todo {
            e.rest = StageState::NotWanted;
        }
        let stage = if e.stop == StageState::Todo {
            Stage::Stop
        } else if e.complete && e.rest == StageState::Todo {
            Stage::Rest
        } else {
            return None;
        };
        let account = e.account;
        let set = |e: &mut AutoEntry, s: StageState| match stage {
            Stage::Stop => e.stop = s,
            Stage::Rest => e.rest = s,
        };
        if !(env.switch)(account) {
            set(e, StageState::Failed);
            Self::add_failure(e, FailReason::SwitchedOff, None);
            let text = format!("{}: {SWITCHED_OFF}", e.symbol);
            self.push_note(account, text, true);
            return None;
        }
        // B1: only a fill that adds to a position of the entry's direction is protected. A
        // resting limit order can fill against a position that was opened the other way in the
        // meantime and only reduce it; a stop for the entry's direction would then be wrong.
        // A fresh list that shows only the other direction stops the stage. A list that cannot
        // be trusted does not: the stop is a close-position order, which the exchange refuses
        // when it points the wrong way, and holding it back would leave a position bare.
        if stage == Stage::Stop {
            let held = (env.held)(account, &e.symbol);
            if !held.stale && !held.dirs.is_empty() && !held.dirs.contains(&e.dir) {
                set(e, StageState::Failed);
                Self::add_failure(e, FailReason::ReducedInstead, None);
                let text = format!("{}: {REDUCED_INSTEAD}", e.symbol);
                self.push_note(account, text, true);
                return None;
            }
        }
        let roles = stage_roles(&e.spec, stage);
        let mut orders = Vec::with_capacity(roles.len());
        for role in roles {
            let issued = e.grant.lock().issue(role, now_ms);
            match issued {
                Ok((which, id)) => orders.push(Planned {
                    role,
                    issued: which,
                    id,
                }),
                Err(refusal) => {
                    set(e, StageState::Failed);
                    Self::add_failure(e, FailReason::GrantRefused, None);
                    let text = format!(
                        "{}: A protective order was not placed: {refusal}. The position may not \
                         be protected.",
                        e.symbol
                    );
                    self.push_note(account, text, true);
                    return None;
                }
            }
        }
        for o in &orders {
            e.placed.push(Placed {
                which: o.issued,
                id: o.id.as_str().to_string(),
                role: o.role,
                state: Live::Open,
                unsure: false,
            });
            self.by_placed
                .insert(o.id.as_str().to_string(), key.to_string());
        }
        set(e, StageState::Running);
        e.busy = true;
        Some(job(e, Work::Place { stage, orders }))
    }

    /// The result of a job.
    pub fn done(&mut self, summary: AutoSummary) {
        let AutoSummary {
            key,
            account,
            symbol,
            steps,
        } = summary;
        let mut notes = Vec::new();
        if let Some(e) = self.entries.get_mut(&key.0) {
            e.busy = false;
            let mut place_failed = false;
            let mut placed_any = false;
            for (which, result) in &steps {
                match which {
                    Which::Placed(r) => {
                        placed_any = true;
                        let role = e.placed.iter().find(|p| p.which == *r).map(|p| p.role);
                        match result {
                            StepResult::Ok => {}
                            StepResult::Unknown => {
                                if let Some(p) = e.placed.iter_mut().find(|p| p.which == *r) {
                                    p.unsure = true;
                                }
                                Self::add_failure(e, FailReason::Unknown, Some(*r));
                                notes.push(format!(
                                    "{symbol}: the exchange did not confirm the {}; check the \
                                     open orders. It was not sent again.",
                                    role_word(role)
                                ));
                            }
                            StepResult::Failed(..) | StepResult::Gone => {
                                place_failed = true;
                                if let Some(p) = e.placed.iter_mut().find(|p| p.which == *r) {
                                    p.state = Live::Failed;
                                }
                                let (reason, why) = match result {
                                    StepResult::Failed(reason, w) => (*reason, w.as_str()),
                                    _ => (FailReason::Rejected, "the exchange has no such order"),
                                };
                                Self::add_failure(e, reason, Some(*r));
                                notes.push(format!(
                                    "{symbol}: the {} was not placed ({why}). The position may \
                                     not be protected. Nothing was sent again.",
                                    role_word(role)
                                ));
                            }
                        }
                    }
                    Which::Cancelled(r) => {
                        let role = e.placed.iter().find(|p| p.which == *r).map(|p| p.role);
                        let state = match result {
                            StepResult::Ok | StepResult::Gone => Live::Cancelled,
                            // Not cancelled as far as we know: it may still stand. Not retried.
                            StepResult::Failed(..) | StepResult::Unknown => Live::CancelFailed,
                        };
                        let failed = state == Live::CancelFailed;
                        let cancelling = e
                            .placed
                            .iter()
                            .find(|p| p.which == *r)
                            .is_some_and(|p| p.state == Live::Cancelling);
                        // An event that came first (cancelled, finished) is not overwritten by a
                        // failed or unknown cancel result, nor reported as a failure.
                        if failed && !cancelling {
                            continue;
                        }
                        if let Some(p) = e.placed.iter_mut().find(|p| p.which == *r) {
                            p.state = state;
                        }
                        match result {
                            StepResult::Ok | StepResult::Gone => {}
                            StepResult::Failed(_, why) => {
                                Self::add_failure(e, FailReason::CancelFailed, Some(*r));
                                notes.push(format!(
                                    "{symbol}: the {} could not be cancelled ({why}); it may \
                                     already be gone \u{2014} check the open orders and cancel it \
                                     by hand.",
                                    role_word(role)
                                ));
                            }
                            StepResult::Unknown => {
                                Self::add_failure(e, FailReason::CancelUnknown, Some(*r));
                                notes.push(format!(
                                    "{symbol}: the exchange did not confirm the cancel of the \
                                     {}; check the open orders and cancel it by hand. It was not \
                                     tried again.",
                                    role_word(role)
                                ));
                            }
                        }
                    }
                }
            }
            if placed_any {
                let state = if place_failed {
                    StageState::Failed
                } else {
                    StageState::Done
                };
                for stage in [&mut e.stop, &mut e.rest] {
                    if *stage == StageState::Running {
                        *stage = state;
                    }
                }
                // The stop-loss fired while the orders were going out: the new reducing orders
                // are not wanted (a stop-loss is never released here, see `release_stop`).
                if e.closed {
                    for p in e
                        .placed
                        .iter_mut()
                        .filter(|p| p.state == Live::Open && p.role != Role::StopLoss)
                    {
                        p.state = Live::ToCancel;
                    }
                }
            }
        }
        // Everything written here is a failure of the protection or of a cancel.
        for n in notes {
            self.push_note(account, n, true);
        }
        self.housekeeping();
    }

    /// Report entries whose attach work ended, and drop the ones with nothing left to watch.
    fn housekeeping(&mut self) {
        let mut remove = Vec::new();
        for (id, e) in &mut self.entries {
            let ended = e.stop.terminal() && e.rest.terminal();
            if ended && !e.reported && !e.busy {
                e.reported = true;
                self.finished.push(id.clone());
            }
            // An order that could not be cancelled stays in the book: a later event can still
            // say it is gone, and the entry is not forgotten while it may stand.
            let live = e
                .placed
                .iter()
                .any(|p| p.state.holds_the_position() || p.state == Live::CancelFailed);
            // An entry with a failure stays until the user has handled it: the failure is the
            // record the panel shows (the notes list holds twelve lines and drops the oldest).
            let replacing = e.placed.iter().any(|p| self.replacing.contains_key(&p.id));
            if ended
                && e.complete
                && !e.busy
                && !live
                && !replacing
                && e.sent == Sent::Stands
                && e.failures.is_empty()
            {
                remove.push(id.clone());
            }
        }
        for id in remove {
            self.drop_entry(&id);
        }
    }
}

fn role_word(role: Option<Role>) -> &'static str {
    match role {
        Some(Role::StopLoss) => "stop-loss",
        Some(Role::TakeProfit) => "take-profit",
        Some(Role::Trailing) => "trailing stop",
        _ => "protective order",
    }
}

// ── Sending ─────────────────────────────────────────────────────────────

pub struct RunInput {
    pub job: AutoJob,
    pub meta: AccountMeta,
    pub session: Option<SessionEntry>,
    pub signed: SignedHttp,
    pub proxy: Arc<dyn Fn() -> ProxyState + Send + Sync>,
    pub risk: SharedRisk,
    /// The market price now, when known; the fill price stands in when it is not (the stop is
    /// then checked against the fill only).
    pub ref_price: Option<Decimal>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The exchange codes that mean "no such order" for the cancel of a conditional order. Empty on
/// purpose: the code of a missing algo order has not been checked against the exchange yet, so
/// every refusal of a cancel is shown to the user (a rate limit, a block or a clock error must
/// never read as "already gone"). Add the code here once testnet has confirmed it.
const CANCEL_NOT_FOUND: &[&str] = &[];

/// A refused request as a step result. Only an exchange refusal with a code of `not_found`
/// (and only when cancelling) counts as an order that is already gone.
fn rejected_step(cancel: bool, kind: &RejectKind, code: &str, not_found: &[&str]) -> StepResult {
    if cancel && matches!(kind, RejectKind::Exchange) && not_found.contains(&code) {
        return StepResult::Gone;
    }
    match kind {
        // 429 and 418 are told apart from a refusal of the order itself: the request was not
        // executed, and it is not sent again.
        RejectKind::RateLimited(status) => StepResult::Failed(
            FailReason::RateLimited,
            format!("the exchange limited requests, HTTP {status}"),
        ),
        RejectKind::Exchange | RejectKind::ClockSkew | RejectKind::Blocked => StepResult::Failed(
            FailReason::Rejected,
            format!("the exchange refused it, code {code}"),
        ),
    }
}

/// Why a request of a stage could not be built. A quantity that does not fit the symbol's rules
/// is told apart from prices that cannot be worked out.
fn build_failure(e: &exchange::trade::TradeError) -> (FailReason, String) {
    use exchange::trade::TradeError as T;
    let reason = match e {
        T::NonPositive
        | T::QtyBelowMin
        | T::QtyAboveMax
        | T::NotionalBelowMin
        | T::NotionalAboveMax
        | T::ExceedsAvailable => FailReason::QtyCheck,
        _ => FailReason::PlanError,
    };
    (reason, e.to_string())
}

/// One request of a job: authorise it against the grant, write the risk-log line, send it, and
/// read the outcome. Never sent twice.
struct Sender<'a> {
    client: &'a TradeClient,
    gate: &'a SessionGate<'a>,
    risk: &'a SharedRisk,
    journal: &'a UnknownStore,
    log: &'a RiskLog,
    meta: &'a AccountMeta,
    symbol: &'a str,
    grant: &'a GrantCell,
}

impl Sender<'_> {
    async fn step(
        &self,
        built: Result<AutoIntent, (FailReason, String)>,
        cancel: bool,
    ) -> StepResult {
        let auto = match built {
            Ok(a) => a,
            Err((reason, why)) => return StepResult::Failed(reason, why),
        };
        let path = auto.path();
        let granted = self.grant.lock().authorise(auto, now_ms());
        let granted = match granted {
            Ok(g) => g,
            Err(refusal) => {
                return StepResult::Failed(FailReason::GrantRefused, refusal.to_string());
            }
        };
        let entry = RiskLogEntry::auto_path_send(self.meta, now_ms(), self.symbol, path);
        if let Err(e) = self.log.append(&entry) {
            log::warn!("risk log write failed: {e}");
        }
        let sent = self
            .client
            .submit_auto(self.gate, self.risk, self.journal, granted)
            .await;
        match sent {
            Err(e) => StepResult::Failed(FailReason::NotSent, describe_submit_error(&e)),
            Ok(submitted) => match submitted.outcome {
                Outcome::Accepted { .. } => StepResult::Ok,
                Outcome::Rejected { kind, code, .. } => {
                    rejected_step(cancel, &kind, &code, CANCEL_NOT_FOUND)
                }
                Outcome::Partial { .. } => StepResult::Failed(
                    FailReason::Rejected,
                    "the exchange accepted only part of it".to_string(),
                ),
                Outcome::NotSent(e) => StepResult::Failed(FailReason::NotSent, e.to_string()),
                Outcome::Unknown { .. } => match submitted.pending {
                    // Never a resend: the lookup only reads.
                    Some(p) => match self.client.resolve_unknown(p, self.journal).await {
                        Resolution::Found => StepResult::Ok,
                        Resolution::NotPlacedPresumed => StepResult::Failed(
                            FailReason::Unknown,
                            "the exchange has no such order".to_string(),
                        ),
                        Resolution::Unresolved(_) => StepResult::Unknown,
                    },
                    None => StepResult::Unknown,
                },
            },
        }
    }
}

/// Keychain → client → rules → grant → `submit_auto` for every step of the job. Never resends.
pub async fn run(input: RunInput) -> AutoSummary {
    let RunInput {
        job,
        meta,
        session,
        signed,
        proxy,
        risk,
        ref_price,
    } = input;
    let AutoJob {
        account,
        symbol,
        key,
        work,
        grant,
        spec,
        dir,
        mode,
        avg_price,
        filled,
    } = job;
    let summary = |steps: Vec<(Which, StepResult)>| AutoSummary {
        key: key.clone(),
        account,
        symbol: symbol.clone(),
        steps,
    };
    let Some(market) =
        crate::order_flow::trade_market(crate::order_flow::market_of(meta.venue, meta.product))
    else {
        return summary(all_failed(
            &work,
            FailReason::SetupFailed,
            "this account's market cannot be traded",
        ));
    };
    let setup = tokio::time::timeout(SETUP_TIMEOUT, async {
        let loaded = {
            let meta = meta.clone();
            tokio::task::spawn_blocking(move || load_credentials_with_identity(&meta)).await
        };
        let (creds, identity) = match loaded {
            Ok(Ok(pair)) => pair,
            _ => return Err("the API credentials could not be read"),
        };
        let proxy_ok = check_proxy_snapshot(signed.proxy_state(), &proxy()).is_ok();
        let client = TradeClient::new(
            signed,
            meta.venue,
            meta.env,
            meta.product,
            account_key(meta.id),
            creds,
            proxy,
        );
        let _ = client.sync_clock().await;
        let table = client
            .fetch_rules(market.rules)
            .await
            .map_err(|_| "the trading rules could not be read")?;
        Ok((client, table, identity, proxy_ok))
    })
    .await;
    let (client, table, identity, proxy_ok) = match setup {
        Ok(Ok(ready)) => ready,
        Ok(Err(why)) => return summary(all_failed(&work, FailReason::SetupFailed, why)),
        Err(_) => {
            return summary(all_failed(
                &work,
                FailReason::SetupFailed,
                "reading the account took too long",
            ));
        }
    };
    let keychain: Result<_, StoreError> = Ok(identity);
    let gate = SessionGate {
        session: session.as_ref(),
        keychain: &keychain,
        saved: &meta,
        proxy_ok,
    };
    let journal = UnknownStore::in_data_dir();
    let log = RiskLog::in_data_dir();
    let Ok(fresh) = client.fresh(&table, &symbol) else {
        return summary(all_failed(
            &work,
            FailReason::SetupFailed,
            "the trading rules are out of date",
        ));
    };
    let sender = Sender {
        client: &client,
        gate: &gate,
        risk: &risk,
        journal: &journal,
        log: &log,
        meta: &meta,
        symbol: &symbol,
        grant: &grant,
    };
    let mut steps = Vec::new();
    match work {
        Work::Place { stage, orders } => {
            // With no live price the fill price stands in: the stop is then checked against the
            // fill only.
            let reference = ref_price.unwrap_or(avg_price);
            let input = StageInput {
                rules: fresh.rules(),
                mode,
                dir,
                avg_price,
                filled,
                ref_price: reference,
                spec: &spec,
                stage,
            };
            // What every request of the stage is based on; the grant compares it with its own.
            let basis = input.basis(&key.0);
            let mut issued = Vec::with_capacity(orders.len());
            let mut ids = Vec::with_capacity(orders.len());
            for o in orders {
                issued.push(o.issued);
                ids.push((o.role, o.id));
            }
            let built = build_stage(&input, ids);
            for (which, (_, request)) in issued.into_iter().zip(built) {
                let auto = request
                    .map_err(|e| build_failure(&e))
                    .and_then(|r| {
                        OrderIntent::binance(account_key(meta.id), r, &fresh)
                            .map_err(|e| build_failure(&e))
                    })
                    .and_then(|i| {
                        AutoIntent::new(AutoPath::AttachProtective, i)
                            .map(|a| a.with_basis(basis.clone()))
                            .map_err(|e| build_failure(&e))
                    });
                let result = sender.step(auto, false).await;
                steps.push((Which::Placed(which), result));
            }
        }
        Work::Cancel { which } => {
            for r in which {
                let built = grant
                    .lock()
                    .cancel_intent(r, &fresh)
                    .map_err(|e| (FailReason::GrantRefused, e.to_string()));
                let result = sender.step(built, true).await;
                steps.push((Which::Cancelled(r), result));
            }
        }
    }
    summary(steps)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use exchange::trade::binance_futures as bf;
    use exchange::trade::protect::{Offset, TargetSpec, TriggerBasis};
    use exchange::trade::test_support::{fresh, futures_table};
    use exchange::trade::types::{Action, TimeInForce};

    const NOW: i64 = 2_000;

    fn d(s: &str) -> Decimal {
        Decimal::parse(s).unwrap()
    }

    fn account() -> Uuid {
        Uuid::from_u128(7)
    }

    pub(crate) fn spec(targets: usize, trailing: bool) -> ProtectSpec {
        ProtectSpec {
            stop: Some(Offset::Percent(d("1"))),
            targets: (0..targets)
                .map(|i| TargetSpec {
                    offset: Offset::Percent(d(&format!("{}", 2 + i))),
                    share_pct: if targets == 1 {
                        100
                    } else if i + 1 == targets {
                        100 - 50 * u8::try_from(targets - 1).unwrap().min(1)
                    } else {
                        50
                    },
                })
                .collect(),
            trailing: trailing.then(|| exchange::trade::protect::TrailingSpec {
                callback_pct: d("1"),
                activation: None,
            }),
            trigger_price: TriggerBasis::Mark,
        }
    }

    /// A confirmed entry: its client id and the init the confirmation hands over.
    fn entry(spec: ProtectSpec) -> (String, AutoInit) {
        entry_for(account(), spec)
    }

    /// The same for `account` (the other modules' tests use it).
    pub(crate) fn entry_for(account: Uuid, spec: ProtectSpec) -> (String, AutoInit) {
        let table = futures_table();
        let rules = table.lookup("BTCUSDT").unwrap();
        let id = ClientId::generate(Role::Entry);
        let request = bf::new_order(bf::FuturesEntry {
            rules,
            mode: Some(PositionMode::OneWay),
            action: Action::Open(Dir::Long),
            qty: d("0.004"),
            exec: exchange::trade::types::Exec::Limit {
                price: d("60000"),
                tif: TimeInForce::Gtc,
            },
            ref_price: Some(d("60000")),
            client_id: id,
        })
        .unwrap();
        let intent = OrderIntent::binance(
            account_key(account),
            request,
            &fresh(&table, "BTCUSDT", NOW),
        )
        .unwrap();
        let key = intent.client_ids()[0].to_string();
        let init = mint(
            &intent,
            account,
            "BTCUSDT",
            &spec,
            PositionMode::OneWay,
            NOW,
        )
        .unwrap();
        (key, init)
    }

    /// What the book holds as filled for an entry.
    pub(crate) fn filled_of(book: &AutoBook, id: &str) -> Option<Decimal> {
        book.entries.get(id).map(|e| e.filled)
    }

    fn polled(book: &mut AutoBook, now: i64, switch: bool, held: Holdings) -> Vec<AutoJob> {
        let switch = move |_: Uuid| switch;
        let held = move |_: Uuid, _: &str| held.clone();
        book.poll(
            now,
            &PollEnv {
                switch: &switch,
                held: &held,
                mono_ms: now,
            },
        )
    }

    fn poll(book: &mut AutoBook) -> Vec<AutoJob> {
        polled(book, NOW, true, Holdings::default())
    }

    fn booked(spec: ProtectSpec) -> (AutoBook, String) {
        let (key, init) = entry(spec);
        let mut book = AutoBook::default();
        book.register(key.clone(), init);
        book.settle(
            account(),
            "BTCUSDT",
            &SendResult::Accepted { order_id: None },
        );
        (book, key)
    }

    fn fill(book: &mut AutoBook, key: &str, qty: &str) -> FillVerdict {
        book.on_fill(account(), key, Some(Side::Buy), d(qty), d("60000"))
    }

    fn order(key: &str, category: OrderCategory, status: OrderStatus, filled: &str) -> OrderUpdate {
        OrderUpdate {
            symbol: "BTCUSDT".into(),
            client_id: Some(key.into()),
            order_id: None,
            category,
            side: None,
            status,
            price: None,
            qty: None,
            filled_qty: Some(d(filled)),
            reduce_only: None,
            position_side: None,
            time_ms: None,
        }
    }

    fn ok_all(job: &AutoJob) -> AutoSummary {
        let steps = match &job.work {
            Work::Place { orders, .. } => orders
                .iter()
                .map(|o| (Which::Placed(o.issued), StepResult::Ok))
                .collect(),
            Work::Cancel { which } => which
                .iter()
                .map(|r| (Which::Cancelled(*r), StepResult::Ok))
                .collect(),
        };
        AutoSummary {
            key: job.key.clone(),
            account: job.account,
            symbol: job.symbol.clone(),
            steps,
        }
    }

    fn place_ids(job: &AutoJob) -> Vec<String> {
        match &job.work {
            Work::Place { orders, .. } => {
                orders.iter().map(|o| o.id.as_str().to_string()).collect()
            }
            Work::Cancel { .. } => Vec::new(),
        }
    }

    fn cancel_count(job: &AutoJob) -> usize {
        match &job.work {
            Work::Cancel { which } => which.len(),
            Work::Place { .. } => 0,
        }
    }

    #[test]
    fn nothing_is_attached_before_the_entry_send_is_settled() {
        let (key, init) = entry(spec(1, false));
        let mut book = AutoBook::default();
        book.register(key.clone(), init);
        // The fill arrives before the send reports back: counted, but nothing goes out.
        assert_eq!(fill(&mut book, &key, "0.001"), FillVerdict::Counted);
        assert!(poll(&mut book).is_empty());
        book.settle(account(), "BTCUSDT", &SendResult::Unresolved);
        assert!(poll(&mut book).is_empty(), "unknown result: hold");
        book.entry_stands(&key);
        assert_eq!(poll(&mut book).len(), 1);
    }

    #[test]
    fn the_first_fill_sends_one_stop_and_a_repeat_sends_nothing() {
        let (mut book, key) = booked(spec(2, true));
        assert!(poll(&mut book).is_empty(), "no fill yet");
        fill(&mut book, &key, "0.001");
        let jobs = poll(&mut book);
        assert_eq!(jobs.len(), 1);
        assert!(matches!(
            jobs[0].work,
            Work::Place {
                stage: Stage::Stop,
                ..
            }
        ));
        assert_eq!(place_ids(&jobs[0]).len(), 1);
        // The job is out: another poll (and another fill) makes no second stop.
        assert!(poll(&mut book).is_empty());
        fill(&mut book, &key, "0.001");
        assert!(poll(&mut book).is_empty());
        book.done(ok_all(&jobs[0]));
        assert!(poll(&mut book).is_empty(), "stop is placed once");
    }

    #[test]
    fn a_partial_fill_sequence_is_a_stop_first_then_the_rest_when_complete() {
        let (mut book, key) = booked(spec(2, true));
        fill(&mut book, &key, "0.001");
        let stop = poll(&mut book);
        book.done(ok_all(&stop[0]));
        fill(&mut book, &key, "0.002");
        assert!(poll(&mut book).is_empty(), "entry not finished");
        // The entry says it is filled, with the quantity the fills have not reached yet: wait.
        book.on_order(
            account(),
            &order(&key, OrderCategory::Plain, OrderStatus::Filled, "0.004"),
        );
        assert!(poll(&mut book).is_empty(), "fills still on their way");
        fill(&mut book, &key, "0.001");
        let rest = poll(&mut book);
        assert_eq!(rest.len(), 1);
        assert!(matches!(
            rest[0].work,
            Work::Place {
                stage: Stage::Rest,
                ..
            }
        ));
        // Two take-profits and a trailing stop.
        assert_eq!(place_ids(&rest[0]).len(), 3);
        assert_eq!(rest[0].filled, d("0.004"));
    }

    #[test]
    fn a_partial_fill_then_a_cancel_protects_what_filled() {
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.001");
        let stop = poll(&mut book);
        book.done(ok_all(&stop[0]));
        book.on_order(
            account(),
            &order(&key, OrderCategory::Plain, OrderStatus::Canceled, "0.001"),
        );
        let rest = poll(&mut book);
        assert_eq!(rest.len(), 1, "the take-profit for what filled");
        assert_eq!(rest[0].filled, d("0.001"));
    }

    #[test]
    fn a_cancelled_entry_with_no_fill_is_forgotten() {
        let (mut book, key) = booked(spec(1, false));
        book.on_order(
            account(),
            &order(&key, OrderCategory::Plain, OrderStatus::Canceled, "0"),
        );
        assert!(poll(&mut book).is_empty());
        assert!(!book.entries.contains_key(&key));
        assert_eq!(book.take_finished(), vec![key]);
    }

    #[test]
    fn fills_that_do_not_add_or_cannot_be_matched_are_not_counted() {
        let (mut book, key) = booked(spec(1, false));
        // Selling against a long entry reduces: it never attaches (B1).
        assert_eq!(
            book.on_fill(account(), &key, Some(Side::Sell), d("0.001"), d("60000")),
            FillVerdict::WrongSide
        );
        assert_eq!(
            book.on_fill(account(), &key, None, d("0.001"), d("60000")),
            FillVerdict::WrongSide
        );
        // Another client id, or another account: not ours.
        assert_eq!(
            book.on_fill(account(), "other", Some(Side::Buy), d("1"), d("1")),
            FillVerdict::NotOurs
        );
        assert_eq!(
            book.on_fill(Uuid::from_u128(9), &key, Some(Side::Buy), d("1"), d("1")),
            FillVerdict::NotOurs
        );
        assert!(poll(&mut book).is_empty());
    }

    /// Place a stop and the rest, return the book and the ids placed.
    fn with_everything_placed() -> (AutoBook, String, Vec<String>) {
        with_placed(spec(2, false))
    }

    fn with_placed(spec: ProtectSpec) -> (AutoBook, String, Vec<String>) {
        let (mut book, key) = booked(spec);
        fill(&mut book, &key, "0.004");
        book.on_order(
            account(),
            &order(&key, OrderCategory::Plain, OrderStatus::Filled, "0.004"),
        );
        let mut ids = Vec::new();
        let stop = poll(&mut book);
        ids.extend(place_ids(&stop[0]));
        book.done(ok_all(&stop[0]));
        let rest = poll(&mut book);
        ids.extend(place_ids(&rest[0]));
        book.done(ok_all(&rest[0]));
        (book, key, ids)
    }

    #[test]
    fn a_fired_stop_cancels_every_take_profit_once() {
        let (mut book, _, ids) = with_everything_placed();
        let fired = order(&ids[0], OrderCategory::Algo, OrderStatus::Triggered, "0");
        book.on_order(account(), &fired);
        // The same event again changes nothing.
        book.on_order(account(), &fired);
        let jobs = poll(&mut book);
        assert_eq!(jobs.len(), 1);
        assert_eq!(cancel_count(&jobs[0]), 2);
        assert!(poll(&mut book).is_empty(), "the cancel is out: not again");
        book.done(ok_all(&jobs[0]));
        book.on_order(account(), &fired);
        assert!(poll(&mut book).is_empty());
    }

    fn long_held(stale: bool) -> Holdings {
        Holdings {
            stale,
            dirs: vec![Dir::Long],
        }
    }

    fn flat(stale: bool) -> Holdings {
        Holdings {
            stale,
            dirs: Vec::new(),
        }
    }

    fn state_of(book: &AutoBook, key: &str, id: &str) -> Live {
        book.entries[key]
            .placed
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.state)
            .unwrap()
    }

    fn algo(id: &str, status: OrderStatus) -> OrderUpdate {
        order(id, OrderCategory::Algo, status, "0")
    }

    // M2 (spec change: a triggered take-profit no longer cancels the stop; only a finished
    // order does, and only when the fresh list shows no position left).
    #[test]
    fn a_triggered_take_profit_does_not_cancel_the_stop() {
        let (mut book, key, ids) = with_placed(spec(1, false));
        book.on_order(account(), &algo(&ids[1], OrderStatus::Triggered));
        // Even with a list that shows nothing held: only a finished order releases the stop.
        assert!(polled(&mut book, NOW, true, flat(false)).is_empty());
        assert_eq!(state_of(&book, &key, &ids[0]), Live::Open);
    }

    #[test]
    fn a_finished_take_profit_with_a_position_left_keeps_the_stop_and_says_so_once() {
        let (mut book, key, ids) = with_placed(spec(1, false));
        book.on_order(account(), &algo(&ids[1], OrderStatus::Triggered));
        book.on_order(account(), &algo(&ids[1], OrderStatus::Finished));
        assert!(polled(&mut book, NOW, true, long_held(false)).is_empty());
        assert!(polled(&mut book, NOW, true, long_held(false)).is_empty());
        assert_eq!(state_of(&book, &key, &ids[0]), Live::Open);
        let notes = book.notes_for(account());
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("stop-loss was kept"));
    }

    #[test]
    fn a_finished_take_profit_with_a_fresh_flat_list_cancels_the_stop_once() {
        let (mut book, key, ids) = with_placed(spec(1, false));
        book.on_order(account(), &algo(&ids[1], OrderStatus::Triggered));
        book.on_order(account(), &algo(&ids[1], OrderStatus::Finished));
        let jobs = polled(&mut book, NOW, true, flat(false));
        assert_eq!(jobs.len(), 1);
        assert_eq!(cancel_count(&jobs[0]), 1, "only the stop is left to cancel");
        assert!(polled(&mut book, NOW, true, flat(false)).is_empty());
        book.done(ok_all(&jobs[0]));
        // Everything is cancelled or finished: the entry is done and leaves the book.
        assert!(!book.entries.contains_key(&key));
        assert!(polled(&mut book, NOW, true, flat(false)).is_empty());
        assert!(book.notes_for(account()).is_empty());
    }

    // ── f3c: dragging an order the app placed, and following its replacement ──

    /// What the sender does for a placing job, without the exchange: the same requests, put
    /// through the entry's grant, so that the grant holds the orders as sent.
    fn grant_sends(job: AutoJob) {
        let AutoJob {
            work: Work::Place { stage, orders },
            key,
            grant,
            spec,
            dir,
            mode,
            filled,
            ..
        } = job
        else {
            return;
        };
        let table = futures_table();
        let f = fresh(&table, "BTCUSDT", NOW);
        let price = d("60000");
        let input = StageInput {
            rules: f.rules(),
            mode,
            dir,
            avg_price: price,
            filled,
            ref_price: price,
            spec: &spec,
            stage,
        };
        let basis = input.basis(&key.0);
        let issued: Vec<IssuedRef> = orders.iter().map(|o| o.issued).collect();
        let ids = orders.into_iter().map(|o| (o.role, o.id)).collect();
        for (_, (_, request)) in issued.into_iter().zip(build_stage(&input, ids)) {
            let intent =
                OrderIntent::binance(account_key(account()), request.unwrap(), &f).unwrap();
            let auto = AutoIntent::new(AutoPath::AttachProtective, intent)
                .unwrap()
                .with_basis(basis.clone());
            grant
                .lock()
                .authorise(auto, NOW)
                .expect("the grant covers it");
        }
    }

    /// [`with_placed`], with the grant holding every order as sent.
    fn with_sent(spec: ProtectSpec) -> (AutoBook, String, Vec<String>) {
        let (mut book, key) = booked(spec);
        fill(&mut book, &key, "0.004");
        book.on_order(
            account(),
            &order(&key, OrderCategory::Plain, OrderStatus::Filled, "0.004"),
        );
        let mut ids = Vec::new();
        for _ in 0..2 {
            let job = poll(&mut book).remove(0);
            ids.extend(place_ids(&job));
            book.done(ok_all(&job));
            grant_sends(job);
        }
        (book, key, ids)
    }

    fn replaced(kind: exchange::trade::ReplaceKind, old: &str) -> (Replacement, String) {
        let new = ClientId::generate(match kind {
            exchange::trade::ReplaceKind::Stop => Role::StopLoss,
            exchange::trade::ReplaceKind::Target => Role::TakeProfit,
        })
        .as_str()
        .to_string();
        let r = Replacement::for_test(account_key(account()), "BTCUSDT", kind, old, &new);
        (r, new)
    }

    /// The client ids the cancel job would send a cancel for, built by the grant as the sender
    /// does.
    fn cancel_targets(job: &AutoJob) -> Vec<String> {
        let Work::Cancel { which } = &job.work else {
            return Vec::new();
        };
        let table = futures_table();
        which
            .iter()
            .flat_map(|w| {
                job.grant
                    .lock()
                    .cancel_intent(*w, &fresh(&table, "BTCUSDT", NOW))
                    .expect("the grant builds the cancel")
                    .client_ids()
                    .iter()
                    .map(|c| (*c).to_string())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn an_order_the_app_placed_is_draggable_while_it_stands_and_no_job_works() {
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.004");
        let stop = poll(&mut book);
        let stop_id = place_ids(&stop[0]).remove(0);
        // The job that places it is still out: not draggable.
        assert!(book.is_working_on(&stop_id));
        book.done(ok_all(&stop[0]));
        // Placed, standing, nothing running: draggable although the book still knows it.
        assert!(book.knows_client_id(&stop_id));
        assert!(!book.is_working_on(&stop_id));
        // The entry order is the job's own, and an id the book does not know is nobody's.
        assert!(book.is_working_on(&key));
        assert!(!book.is_working_on("sNOTKNOWNNOTKNOWNNOT"));
    }

    #[test]
    fn an_order_that_is_marked_cancelling_or_over_is_not_draggable() {
        let (mut book, key, ids) = with_placed(spec(1, false));
        // The stop fires: the take-profit is marked, then a cancel job is out for it.
        book.on_order(account(), &algo(&ids[0], OrderStatus::Triggered));
        assert_eq!(state_of(&book, &key, &ids[1]), Live::ToCancel);
        assert!(book.is_working_on(&ids[1]));
        let _cancel_job = poll(&mut book);
        assert_eq!(state_of(&book, &key, &ids[1]), Live::Cancelling);
        assert!(book.is_working_on(&ids[1]));
        // The order that fired is over.
        assert!(book.is_working_on(&ids[0]));
        // A job that runs on the entry holds every order of it, whatever its own state.
        let (mut book, key, ids) = with_everything_placed();
        book.entries.get_mut(&key).unwrap().busy = true;
        assert!(ids.iter().all(|id| book.is_working_on(id)));
    }

    #[test]
    fn after_a_replace_a_finished_take_profit_cancels_the_new_stop() {
        let (mut book, key, ids) = with_sent(spec(1, false));
        let (r, new_stop) = replaced(exchange::trade::ReplaceKind::Stop, &ids[0]);
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::Adopted);
        assert_eq!(state_of(&book, &key, &new_stop), Live::Open);
        assert!(book.knows_client_id(&new_stop));
        assert!(!book.knows_client_id(&ids[0]), "the old id is gone");
        assert!(!book.is_working_on(&new_stop));
        // The exchange's cancel event for the old stop is not the book's any more.
        book.on_order(account(), &algo(&ids[0], OrderStatus::Canceled));
        assert_eq!(state_of(&book, &key, &new_stop), Live::Open);
        // The take-profit fills and the position is gone: exactly one cancel, of the new stop.
        book.on_order(account(), &algo(&ids[1], OrderStatus::Triggered));
        book.on_order(account(), &algo(&ids[1], OrderStatus::Finished));
        let jobs = polled(&mut book, NOW, true, flat(false));
        assert_eq!(jobs.len(), 1);
        assert_eq!(cancel_count(&jobs[0]), 1);
        assert_eq!(cancel_targets(&jobs[0]), vec![new_stop.clone()]);
        book.done(ok_all(&jobs[0]));
        assert!(!book.entries.contains_key(&key));
    }

    #[test]
    fn after_a_replace_a_fired_stop_cancels_the_new_take_profit_and_the_other_one() {
        let (mut book, key, ids) = with_sent(spec(2, false));
        // ids: the stop, then two take-profits. The second one is replaced.
        let (r, new_tp) = replaced(exchange::trade::ReplaceKind::Target, &ids[2]);
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::Adopted);
        book.on_order(account(), &algo(&ids[0], OrderStatus::Triggered));
        let jobs = poll(&mut book);
        assert_eq!(jobs.len(), 1);
        assert_eq!(cancel_count(&jobs[0]), 2);
        let mut got = cancel_targets(&jobs[0]);
        got.sort();
        let mut want = vec![ids[1].clone(), new_tp.clone()];
        want.sort();
        assert_eq!(got, want);
        assert!(!got.contains(&ids[2]));
        assert_eq!(state_of(&book, &key, &new_tp), Live::Cancelling);
    }

    #[test]
    fn the_exchanges_cancel_event_that_comes_before_the_answer_does_not_lose_the_order() {
        let (mut book, key, ids) = with_sent(spec(1, false));
        // The stream is faster than the answer of the replace.
        book.on_order(account(), &algo(&ids[0], OrderStatus::Canceled));
        assert_eq!(state_of(&book, &key, &ids[0]), Live::Cancelled);
        let (r, new_stop) = replaced(exchange::trade::ReplaceKind::Stop, &ids[0]);
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::Adopted);
        assert_eq!(state_of(&book, &key, &new_stop), Live::Open);
        book.on_order(account(), &algo(&ids[1], OrderStatus::Finished));
        let jobs = polled(&mut book, NOW, true, flat(false));
        assert_eq!(cancel_targets(&jobs[0]), vec![new_stop]);
    }

    #[test]
    fn a_replace_that_was_not_completed_is_not_followed() {
        // No proof exists then (`ReplaceDone` carries it, an error does not): the old stop is
        // whatever the exchange's events say. Here the cancel went through and the new order
        // was not placed.
        let (mut book, key, ids) = with_placed(spec(1, false));
        book.on_order(account(), &algo(&ids[0], OrderStatus::Canceled));
        assert_eq!(state_of(&book, &key, &ids[0]), Live::Cancelled);
        assert!(
            book.is_working_on(&ids[0]),
            "a cancelled order is not draggable"
        );
        // A take-profit that finishes cancels nothing: the stop is already gone.
        book.on_order(account(), &algo(&ids[1], OrderStatus::Finished));
        assert!(polled(&mut book, NOW, true, flat(false)).is_empty());
        // And nothing else is adopted: a proof for an order the book never placed is nobody's.
        let (r, _) = replaced(exchange::trade::ReplaceKind::Stop, "sNOTKNOWNNOTKNOWNNOT");
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::NotOurs);
    }

    #[test]
    fn an_adoption_the_book_or_the_grant_cannot_take_changes_nothing() {
        let (mut book, key, ids) = with_sent(spec(1, false));
        // Another account's proof is nobody's.
        let (r, _) = replaced(exchange::trade::ReplaceKind::Stop, &ids[0]);
        assert_eq!(
            book.adopt_replacement(Uuid::from_u128(9), &r),
            Adoption::NotOurs
        );
        // The wrong kind for the order: the grant refuses, a note says so.
        let (r, new_id) = replaced(exchange::trade::ReplaceKind::Target, &ids[0]);
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::Refused);
        assert_eq!(state_of(&book, &key, &ids[0]), Live::Open);
        assert!(!book.knows_client_id(&new_id));
        assert_eq!(book.notes_for(account()).len(), 1);
        // A job is cancelling the old stop: refused as well.
        book.on_order(account(), &algo(&ids[1], OrderStatus::Triggered));
        book.on_order(account(), &algo(&ids[1], OrderStatus::Finished));
        let jobs = polled(&mut book, NOW, true, flat(false));
        assert_eq!(state_of(&book, &key, &ids[0]), Live::Cancelling);
        let (r, new_id) = replaced(exchange::trade::ReplaceKind::Stop, &ids[0]);
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::Refused);
        assert!(!book.knows_client_id(&new_id));
        assert!(book.knows_client_id(&ids[0]));
        // The grant still builds the cancel for the old id.
        assert_eq!(cancel_targets(&jobs[0]), vec![ids[0].clone()]);
    }

    fn replacement_as(kind: exchange::trade::ReplaceKind, old: &str, new: &str) -> Replacement {
        Replacement::for_test(account_key(account()), "BTCUSDT", kind, old, new)
    }

    fn new_id(role: Role) -> String {
        ClientId::generate(role).as_str().to_string()
    }

    /// The new take-profit of a replaced take-profit, once a stop-loss fired while the replace
    /// was out: it is marked, and the one job cancels exactly that order.
    fn assert_new_tp_is_cancelled(book: &mut AutoBook, key: &str, new_tp: &str) {
        assert_eq!(state_of(book, key, new_tp), Live::ToCancel);
        let jobs = poll(book);
        assert_eq!(jobs.len(), 1);
        assert_eq!(cancel_count(&jobs[0]), 1);
        assert_eq!(cancel_targets(&jobs[0]), vec![new_tp.to_string()]);
        assert_eq!(state_of(book, key, new_tp), Live::Cancelling);
    }

    #[test]
    fn a_stop_that_fires_after_the_cancel_event_of_a_replace_takes_the_new_take_profit() {
        let (mut book, key, ids) = with_sent(spec(1, false));
        // T Canceled (the replace's own cancel), then S Triggered, then the answer.
        book.on_order(account(), &algo(&ids[1], OrderStatus::Canceled));
        book.on_order(account(), &algo(&ids[0], OrderStatus::Triggered));
        let (r, new_tp) = replaced(exchange::trade::ReplaceKind::Target, &ids[1]);
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::Adopted);
        assert_new_tp_is_cancelled(&mut book, &key, &new_tp);
    }

    #[test]
    fn a_stop_that_fires_before_the_cancel_event_of_a_replace_takes_the_new_take_profit() {
        let (mut book, key, ids) = with_sent(spec(1, false));
        // S Triggered (T is marked), then T Canceled (it overwrites the mark), then the answer.
        book.on_order(account(), &algo(&ids[0], OrderStatus::Triggered));
        assert_eq!(state_of(&book, &key, &ids[1]), Live::ToCancel);
        book.on_order(account(), &algo(&ids[1], OrderStatus::Canceled));
        assert_eq!(state_of(&book, &key, &ids[1]), Live::Cancelled);
        let (r, new_tp) = replaced(exchange::trade::ReplaceKind::Target, &ids[1]);
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::Adopted);
        assert_new_tp_is_cancelled(&mut book, &key, &new_tp);
    }

    #[test]
    fn a_poll_between_the_events_of_a_replace_does_not_lose_the_new_take_profit_order_a() {
        let (mut book, key, ids) = with_sent(spec(1, false));
        book.replace_begun(account(), &row_of(&ids[1]));
        // T Canceled (the replace's own cancel), a poll, S Triggered, a poll, then the answer.
        book.on_order(account(), &algo(&ids[1], OrderStatus::Canceled));
        assert!(poll(&mut book).is_empty());
        book.on_order(account(), &algo(&ids[0], OrderStatus::Triggered));
        assert!(poll(&mut book).is_empty());
        assert!(book.entries.contains_key(&key), "the entry is kept");
        book.replace_ended(account());
        let (r, new_tp) = replaced(exchange::trade::ReplaceKind::Target, &ids[1]);
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::Adopted);
        assert_new_tp_is_cancelled(&mut book, &key, &new_tp);
    }

    #[test]
    fn a_poll_between_the_events_of_a_replace_does_not_cancel_the_old_id_order_b() {
        let (mut book, key, ids) = with_sent(spec(1, false));
        book.replace_begun(account(), &row_of(&ids[1]));
        // S Triggered, a poll (the old take-profit is not cancelled by id), T Canceled, a poll.
        book.on_order(account(), &algo(&ids[0], OrderStatus::Triggered));
        assert!(poll(&mut book).is_empty());
        assert_eq!(state_of(&book, &key, &ids[1]), Live::ToCancel);
        book.on_order(account(), &algo(&ids[1], OrderStatus::Canceled));
        assert!(poll(&mut book).is_empty());
        book.replace_ended(account());
        let (r, new_tp) = replaced(exchange::trade::ReplaceKind::Target, &ids[1]);
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::Adopted);
        assert_new_tp_is_cancelled(&mut book, &key, &new_tp);
    }

    #[test]
    fn a_replace_that_did_not_complete_gives_the_old_order_back_to_the_usual_paths() {
        let (mut book, key, ids) = with_sent(spec(1, false));
        book.replace_begun(account(), &row_of(&ids[1]));
        book.on_order(account(), &algo(&ids[0], OrderStatus::Triggered));
        assert!(poll(&mut book).is_empty());
        assert_eq!(state_of(&book, &key, &ids[1]), Live::ToCancel);
        // The replace ended with no proof: the old id is cancelled as before.
        book.replace_ended(account());
        let jobs = poll(&mut book);
        assert_eq!(jobs.len(), 1);
        assert_eq!(cancel_targets(&jobs[0]), vec![ids[1].clone()]);
        assert_eq!(state_of(&book, &key, &ids[1]), Live::Cancelling);
    }

    /// The row the replace is sent for: the exchange id is not the client id.
    fn row_of(client_id: &str) -> exchange::trade::OpenOrder {
        use exchange::trade::{OpenKind, ProtectRole, Side};
        exchange::trade::OpenOrder {
            symbol: "BTCUSDT".into(),
            id: "9001".into(),
            kind: OpenKind::Algo(None),
            side: Some(Side::Sell),
            position_side: None,
            qty: None,
            trigger_price: None,
            close_position: true,
            reduce_only: false,
            role: Some(ProtectRole::Target),
            attached: None,
            client_id: Some(client_id.to_string()),
            limit: false,
            limit_price: None,
            activation_price: None,
            callback_rate: None,
            price: None,
        }
    }

    #[test]
    fn a_replace_is_held_back_by_the_client_id_not_the_exchange_id() {
        let (mut book, key, ids) = with_sent(spec(1, false));
        let row = row_of(&ids[1]);
        assert_ne!(row.id, ids[1]);
        book.replace_begun(account(), &row);
        assert_eq!(book.replacing.len(), 1, "recorded while in flight");
        book.on_order(account(), &algo(&ids[0], OrderStatus::Triggered));
        assert!(poll(&mut book).is_empty(), "the old order is not cancelled");
        assert_eq!(state_of(&book, &key, &ids[1]), Live::ToCancel);
        book.on_order(account(), &algo(&ids[1], OrderStatus::Canceled));
        assert!(poll(&mut book).is_empty());
        assert!(book.entries.contains_key(&key), "the entry is kept");
        book.replace_ended(account());
        assert!(book.replacing.is_empty());
        let (r, new_tp) = replaced(exchange::trade::ReplaceKind::Target, &ids[1]);
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::Adopted);
        assert_new_tp_is_cancelled(&mut book, &key, &new_tp);
    }

    #[test]
    fn a_replace_without_a_client_id_or_of_another_account_is_not_recorded() {
        let (mut book, _key, ids) = with_sent(spec(1, false));
        let mut row = row_of(&ids[1]);
        row.client_id = None;
        book.replace_begun(account(), &row);
        assert!(book.replacing.is_empty());
        book.replace_begun(Uuid::from_u128(77), &row_of(&ids[1]));
        assert!(book.replacing.is_empty());
        // The exchange id is never a key.
        book.replace_begun(account(), &row_of("9001"));
        assert!(book.replacing.is_empty());
    }

    #[test]
    fn the_end_of_a_replace_clears_the_account_whatever_the_result() {
        let (mut book, key, ids) = with_sent(spec(1, false));
        book.replace_begun(account(), &row_of(&ids[1]));
        book.on_order(account(), &algo(&ids[0], OrderStatus::Triggered));
        assert!(poll(&mut book).is_empty());
        // Another account's end does not clear it.
        book.replace_ended(Uuid::from_u128(77));
        assert_eq!(book.replacing.len(), 1);
        // No proof, no client id in the summary: the old order goes back to the usual paths.
        book.replace_ended(account());
        let jobs = poll(&mut book);
        assert_eq!(jobs.len(), 1);
        assert_eq!(cancel_targets(&jobs[0]), vec![ids[1].clone()]);
        assert_eq!(state_of(&book, &key, &ids[1]), Live::Cancelling);
    }

    #[test]
    fn a_held_back_cancel_keeps_the_stages_from_being_placed() {
        // Only the stop is placed; it is marked for cancel while the take-profit stage is still
        // to come, and it is held back for a replace: nothing is placed before its cancel.
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.004");
        book.on_order(
            account(),
            &order(&key, OrderCategory::Plain, OrderStatus::Filled, "0.004"),
        );
        let stop = poll(&mut book).remove(0);
        let ids = place_ids(&stop);
        book.done(ok_all(&stop));
        grant_sends(stop);
        book.entries.get_mut(&key).unwrap().placed[0].state = Live::ToCancel;
        book.replace_begun(account(), &row_of(&ids[0]));
        assert!(poll(&mut book).is_empty(), "nothing is placed or cancelled");
        book.replace_ended(account());
        let jobs = poll(&mut book);
        assert_eq!(jobs.len(), 1);
        assert_eq!(cancel_targets(&jobs[0]), vec![ids[0].clone()]);
    }

    #[test]
    fn a_forgotten_order_is_told_apart_per_account() {
        let (mut book, key, ids) = with_sent(spec(1, false));
        book.drop_entry(&key);
        let (r, _) = replaced(exchange::trade::ReplaceKind::Stop, &ids[0]);
        // Another account's replace of the same id is nobody's here.
        assert_eq!(
            book.adopt_replacement(Uuid::from_u128(77), &r),
            Adoption::NotOurs
        );
        book.remove_account(account());
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::NotOurs);
    }

    #[test]
    fn a_new_order_whose_events_came_before_the_answer_is_handled_once_adopted() {
        let (mut book, key, ids) = with_sent(spec(2, false));
        // ids: the stop, then two take-profits. The stop is replaced, and the new stop fires
        // at once: its events come before the answer of the replace.
        let new_stop = new_id(Role::StopLoss);
        book.on_order(account(), &algo(&ids[0], OrderStatus::Canceled));
        book.on_order(account(), &algo(&new_stop, OrderStatus::Triggered));
        book.on_order(account(), &algo(&new_stop, OrderStatus::Finished));
        // Nothing is known of it yet.
        assert!(poll(&mut book).is_empty());
        let r = replacement_as(exchange::trade::ReplaceKind::Stop, &ids[0], &new_stop);
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::Adopted);
        assert_eq!(state_of(&book, &key, &new_stop), Live::Finished);
        // The take-profits are cancelled with it, the new stop is not cancelled again.
        let jobs = poll(&mut book);
        assert_eq!(jobs.len(), 1);
        let mut got = cancel_targets(&jobs[0]);
        got.sort();
        let mut want = vec![ids[1].clone(), ids[2].clone()];
        want.sort();
        assert_eq!(got, want);
        // The replayed events are used once.
        assert!(book.unclaimed[&account()].is_empty());
    }

    #[test]
    fn the_events_kept_for_unknown_orders_are_bounded_and_only_for_app_ids() {
        let mut book = AutoBook::default();
        for _ in 0..(MAX_UNCLAIMED * 3) {
            book.on_order(
                account(),
                &algo(&new_id(Role::TakeProfit), OrderStatus::Triggered),
            );
        }
        assert_eq!(book.unclaimed[&account()].len(), MAX_UNCLAIMED);
        // Another software's id, and events that change no state, are not kept.
        let before = book.unclaimed[&account()].clone();
        book.on_order(
            account(),
            &algo("someone-elses-order", OrderStatus::Triggered),
        );
        book.on_order(account(), &algo(&new_id(Role::StopLoss), OrderStatus::New));
        assert_eq!(book.unclaimed[&account()], before);
        book.remove_account(account());
        assert!(!book.unclaimed.contains_key(&account()));
    }

    #[test]
    fn a_grant_slot_that_the_book_names_differently_changes_nothing() {
        let (mut book, key, ids) = with_sent(spec(1, false));
        // The book thinks the stop is the grant's take-profit slot: they disagree.
        let which = book.entries[&key].placed[1].which;
        book.entries.get_mut(&key).unwrap().placed[0].which = which;
        let (r, new_stop) = replaced(exchange::trade::ReplaceKind::Stop, &ids[0]);
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::Refused);
        assert!(!book.knows_client_id(&new_stop));
        // The grant still names the old id (a change would have moved it to the new one).
        let grant = book.entries[&key].grant.clone();
        assert!(grant.lock().issued_ref_of(&ids[0]).is_some());
        assert!(grant.lock().issued_ref_of(&new_stop).is_none());
    }

    #[test]
    fn an_order_of_an_entry_that_left_the_book_is_told_from_one_never_placed() {
        let (mut book, key, ids) = with_sent(spec(1, false));
        book.drop_entry(&key);
        let (r, _) = replaced(exchange::trade::ReplaceKind::Stop, &ids[0]);
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::Dropped);
        let (r, _) = replaced(exchange::trade::ReplaceKind::Stop, "sNOTKNOWNNOTKNOWNNOT");
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::NotOurs);
    }

    #[test]
    fn an_adoption_never_lets_the_grant_cancel_an_order_it_did_not_send() {
        let (mut book, key, ids) = with_sent(spec(2, false));
        let (r, new_stop) = replaced(exchange::trade::ReplaceKind::Stop, &ids[0]);
        assert_eq!(book.adopt_replacement(account(), &r), Adoption::Adopted);
        // The cancel of an id the grant did not issue is refused (the old id, which the grant
        // no longer knows, is covered by the exchange crate's tests: its text cannot be
        // rebuilt into an id here).
        let grant = book.entries[&key].grant.clone();
        let table = futures_table();
        let f = fresh(&table, "BTCUSDT", NOW);
        let stranger = ClientId::generate(Role::StopLoss);
        let req = bf::cancel_algo_order(&exchange::trade::OrderRef::Client(stranger)).unwrap();
        let intent = OrderIntent::binance(account_key(account()), req, &f).unwrap();
        let auto = AutoIntent::new(AutoPath::CancelSibling, intent).unwrap();
        assert!(grant.lock().authorise(auto, NOW).is_err());
        // The adopted stop is still the one the grant cancels for that slot.
        let which = book.entries[&key].placed[0].which;
        let built = grant.lock().cancel_intent(which, &f).unwrap();
        assert_eq!(built.client_ids(), vec![new_stop.as_str()]);
    }

    #[test]
    fn a_finished_trailing_stop_with_a_position_left_never_cancels_the_stop() {
        let (mut book, key, ids) = with_placed(spec(1, true));
        // ids: the stop, the take-profit, the trailing stop.
        book.on_order(account(), &algo(&ids[2], OrderStatus::Triggered));
        book.on_order(account(), &algo(&ids[2], OrderStatus::Finished));
        // The take-profit goes (a reducing order); the stop stays.
        let jobs = polled(&mut book, NOW, true, long_held(false));
        assert_eq!(jobs.len(), 1);
        assert_eq!(cancel_count(&jobs[0]), 1);
        assert_eq!(state_of(&book, &key, &ids[0]), Live::Open);
        assert_eq!(state_of(&book, &key, &ids[1]), Live::Cancelling);
    }

    #[test]
    fn a_stale_list_holds_the_stop_until_it_is_fresh() {
        let (mut book, key, ids) = with_placed(spec(1, false));
        book.on_order(account(), &algo(&ids[1], OrderStatus::Finished));
        assert!(polled(&mut book, NOW, true, flat(true)).is_empty());
        assert!(book.notes_for(account()).is_empty(), "no note, no cancel");
        assert_eq!(state_of(&book, &key, &ids[0]), Live::Open);
        assert_eq!(polled(&mut book, NOW, true, flat(false)).len(), 1);
    }

    #[test]
    fn an_order_refused_after_it_triggered_did_not_take_effect_and_says_so() {
        let (mut book, key, ids) = with_placed(spec(1, false));
        book.on_order(account(), &algo(&ids[0], OrderStatus::Triggered));
        assert!(book.entries[&key].closed);
        book.on_order(account(), &algo(&ids[0], OrderStatus::Rejected));
        assert_eq!(state_of(&book, &key, &ids[0]), Live::Failed);
        assert!(!book.entries[&key].closed, "the position was not closed");
        let notes = book.notes_for(account());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("did not take effect"));
    }

    #[test]
    fn a_take_profit_cancelled_by_hand_keeps_the_stop_when_the_other_one_fires() {
        let (mut book, _, ids) = with_everything_placed();
        // One take-profit is cancelled by the user; the other one then fires.
        book.on_order(
            account(),
            &order(&ids[1], OrderCategory::Algo, OrderStatus::Canceled, "0"),
        );
        book.on_order(
            account(),
            &order(&ids[2], OrderCategory::Algo, OrderStatus::Triggered, "0"),
        );
        assert!(
            poll(&mut book).is_empty(),
            "part of the position has no take-profit: the stop stays"
        );
    }

    #[test]
    fn a_sibling_that_is_already_gone_is_not_an_error() {
        let (mut book, _, ids) = with_everything_placed();
        book.on_order(
            account(),
            &order(&ids[0], OrderCategory::Algo, OrderStatus::Triggered, "0"),
        );
        // One take-profit was cancelled by someone else in the meantime.
        book.on_order(
            account(),
            &order(&ids[1], OrderCategory::Algo, OrderStatus::Canceled, "0"),
        );
        let jobs = poll(&mut book);
        assert_eq!(cancel_count(&jobs[0]), 1);
        let mut summary = ok_all(&jobs[0]);
        summary.steps[0].1 = StepResult::Gone;
        book.done(summary);
        assert!(book.notes_for(account()).is_empty());
    }

    #[test]
    fn a_switch_turned_off_before_the_fill_sends_nothing_and_says_so() {
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.001");
        assert!(polled(&mut book, NOW, false, Holdings::default()).is_empty());
        let notes = book.notes_for(account());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("protected"));
        // Not retried when the switch comes back.
        assert!(poll(&mut book).is_empty());
    }

    #[test]
    fn a_failed_placement_is_written_down_and_never_retried() {
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.001");
        let jobs = poll(&mut book);
        let mut summary = ok_all(&jobs[0]);
        summary.steps[0].1 = StepResult::Failed(
            FailReason::Rejected,
            "the exchange refused it, code -2021".into(),
        );
        book.done(summary);
        let notes = book.notes_for(account());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("stop-loss") && notes[0].contains("-2021"));
        fill(&mut book, &key, "0.001");
        assert!(poll(&mut book).is_empty());
    }

    #[test]
    fn an_expired_grant_makes_a_visible_failure() {
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.001");
        assert!(
            polled(
                &mut book,
                NOW + GRANT_VALID_MS + 1,
                true,
                Holdings::default()
            )
            .is_empty()
        );
        assert!(book.notes_for(account())[0].contains("not placed"));
    }

    #[test]
    fn nothing_prints_a_client_id() {
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.001");
        let jobs = poll(&mut book);
        let shown = format!(
            "{:?} {:?} {:?} {:?}",
            jobs[0],
            book,
            EntryKey(key.clone()),
            jobs[0].grant
        );
        assert!(!shown.contains(&key), "{shown}");
        for id in place_ids(&jobs[0]) {
            assert!(!shown.contains(&id));
        }
    }

    #[test]
    fn a_removed_account_loses_its_entries_and_notes() {
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.001");
        polled(&mut book, NOW, false, Holdings::default());
        book.remove_account(account());
        assert!(!book.entries.contains_key(&key));
        assert!(book.notes_for(account()).is_empty());
    }

    #[test]
    fn a_fill_that_only_reduced_the_other_way_gets_no_stop() {
        let held = |dirs: Vec<Dir>, stale: bool| Holdings { stale, dirs };
        // A fresh list that shows only a short: the long entry reduced it.
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.001");
        assert!(polled(&mut book, NOW, true, held(vec![Dir::Short], false)).is_empty());
        assert!(book.notes_for(account())[0].contains("reduced that position"));
        // A fresh list with the entry's own direction, or a hedge list with both: goes out.
        for dirs in [vec![Dir::Long], vec![Dir::Long, Dir::Short]] {
            let (mut book, key) = booked(spec(1, false));
            fill(&mut book, &key, "0.001");
            assert_eq!(polled(&mut book, NOW, true, held(dirs, false)).len(), 1);
        }
        // A list that cannot be trusted does not hold the stop back.
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.001");
        assert_eq!(
            polled(&mut book, NOW, true, held(vec![Dir::Short], true)).len(),
            1
        );
    }

    // ── M1: a cancel that is refused or unanswered is never "already gone" ─

    #[test]
    fn a_refused_cancel_is_a_failure_unless_the_code_says_the_order_is_gone() {
        let list: &[&str] = &["-9999"];
        let table: [(&str, RejectKind, &str, &[&str], bool); 7] = [
            (
                "rate limit",
                RejectKind::RateLimited(429),
                "429",
                list,
                false,
            ),
            ("ip ban", RejectKind::RateLimited(418), "418", list, false),
            ("waf", RejectKind::Blocked, "403", list, false),
            ("clock", RejectKind::ClockSkew, "-1021", list, false),
            ("other code", RejectKind::Exchange, "-2011", list, false),
            // The list is empty today: not even a plausible code counts as gone.
            (
                "empty list",
                RejectKind::Exchange,
                "-2011",
                CANCEL_NOT_FOUND,
                false,
            ),
            ("listed code", RejectKind::Exchange, "-9999", list, true),
        ];
        for (name, kind, code, not_found, gone) in table {
            let got = rejected_step(true, &kind, code, not_found);
            assert_eq!(got == StepResult::Gone, gone, "{name}: {got:?}");
        }
        // A refused placement is never "gone", even with a listed code.
        assert!(matches!(
            rejected_step(false, &RejectKind::Exchange, "-9999", list),
            StepResult::Failed(..)
        ));
        assert!(CANCEL_NOT_FOUND.is_empty(), "not confirmed on testnet yet");
    }

    #[test]
    fn a_cancel_that_failed_or_is_unknown_is_written_down_and_not_taken_as_cancelled() {
        let (mut book, key, ids) = with_everything_placed();
        book.on_order(account(), &algo(&ids[0], OrderStatus::Triggered));
        let jobs = poll(&mut book);
        assert_eq!(cancel_count(&jobs[0]), 2);
        let mut summary = ok_all(&jobs[0]);
        summary.steps[0].1 = StepResult::Failed(
            FailReason::RateLimited,
            "the exchange refused it, code 429".into(),
        );
        summary.steps[1].1 = StepResult::Unknown;
        book.done(summary);
        let notes = book.notes_for(account());
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].contains("could not be cancelled") && notes[0].contains("429"));
        assert!(notes[1].contains("did not confirm the cancel"));
        assert_eq!(state_of(&book, &key, &ids[1]), Live::CancelFailed);
        assert_eq!(state_of(&book, &key, &ids[2]), Live::CancelFailed);
        // Never retried, and the entry stays in the book while an order may still stand.
        assert!(poll(&mut book).is_empty());
        assert!(book.entries.contains_key(&key));
        // A later event that says it is gone settles it.
        book.on_order(account(), &algo(&ids[1], OrderStatus::Canceled));
        assert_eq!(state_of(&book, &key, &ids[1]), Live::Cancelled);
    }

    // ── M3: one entry at a time per symbol ─────────────────────────────────

    #[test]
    fn an_entry_with_work_left_is_active_and_one_that_was_given_up_is_not() {
        let (mut book, key) = booked(spec(1, false));
        // A resting entry whose protection is still owed.
        assert!(book.is_active(account(), "BTCUSDT"));
        assert!(book.is_active(account(), "btcusdt"));
        assert!(!book.is_active(account(), "ETHUSDT"));
        assert!(!book.is_active(Uuid::from_u128(9), "BTCUSDT"));
        // Given up with a note (fills not seen, connection lost): it no longer blocks.
        book.on_order(
            account(),
            &order(&key, OrderCategory::Plain, OrderStatus::Canceled, "0.002"),
        );
        assert!(!book.is_active(account(), "BTCUSDT"));
    }

    #[test]
    fn an_unknown_entry_or_order_does_not_block_the_symbol_for_ever() {
        // An entry whose send has no known result: the Unknown lock handles that, not this rule.
        let (key, init) = entry(spec(1, false));
        let mut book = AutoBook::default();
        book.register(key, init);
        assert!(book.is_active(account(), "BTCUSDT"), "still being sent");
        book.settle(account(), "BTCUSDT", &SendResult::Unresolved);
        assert!(!book.is_active(account(), "BTCUSDT"));
        // A protective order with an unknown result, and one that could not be cancelled.
        let (mut book, key, ids) = with_placed(spec(1, false));
        assert!(book.is_active(account(), "BTCUSDT"), "orders stand");
        book.on_order(account(), &algo(&ids[1], OrderStatus::Triggered));
        book.on_order(account(), &algo(&ids[1], OrderStatus::Finished));
        let jobs = polled(&mut book, NOW, true, flat(false));
        let mut summary = ok_all(&jobs[0]);
        summary.steps[0].1 = StepResult::Failed(
            FailReason::RateLimited,
            "the exchange refused it, code 429".into(),
        );
        book.done(summary);
        assert_eq!(state_of(&book, &key, &ids[0]), Live::CancelFailed);
        assert!(!book.is_active(account(), "BTCUSDT"));
        // Placed with an unknown result: the stop stays Open but is marked unsure.
        let (mut book, key) = booked(spec(0, false));
        fill(&mut book, &key, "0.001");
        let jobs = poll(&mut book);
        let mut summary = ok_all(&jobs[0]);
        summary.steps[0].1 = StepResult::Unknown;
        book.done(summary);
        assert!(!book.is_active(account(), "BTCUSDT"));
    }

    // ── M4 ─────────────────────────────────────────────────────────────────

    #[test]
    fn an_entry_that_ended_with_fills_not_seen_is_not_removed_quietly() {
        for status in [
            OrderStatus::Canceled,
            OrderStatus::Expired,
            OrderStatus::Rejected,
        ] {
            let (mut book, key) = booked(spec(1, false));
            // The exchange says 0.003 filled; no fill event was seen.
            book.on_order(
                account(),
                &order(&key, OrderCategory::Plain, status, "0.003"),
            );
            assert!(book.entries.contains_key(&key), "{status:?}");
            let notes = book.notes_for(account());
            assert_eq!(notes.len(), 1);
            assert!(notes[0].contains("fills of the entry were not seen"));
            // Nothing is sent for what was never counted, and it does not block the symbol.
            assert!(poll(&mut book).is_empty());
            assert!(!book.is_active(account(), "BTCUSDT"));
        }
    }

    #[test]
    fn a_status_with_more_filled_than_seen_still_protects_what_was_seen() {
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.001");
        let stop = poll(&mut book);
        book.done(ok_all(&stop[0]));
        book.on_order(
            account(),
            &order(&key, OrderCategory::Plain, OrderStatus::Canceled, "0.003"),
        );
        assert_eq!(book.notes_for(account()).len(), 1);
        let rest = poll(&mut book);
        assert_eq!(rest.len(), 1, "a take-profit for the 0.001 that was seen");
        assert_eq!(rest[0].filled, d("0.001"));
    }

    #[test]
    fn a_stop_that_closed_the_position_gets_no_take_profit_and_a_later_fill_is_noted() {
        let (mut book, key) = booked(spec(2, true));
        fill(&mut book, &key, "0.001");
        let stop = poll(&mut book);
        let stop_id = place_ids(&stop[0]).remove(0);
        book.done(ok_all(&stop[0]));
        book.on_order(account(), &algo(&stop_id, OrderStatus::Triggered));
        // The resting entry fills again after the stop closed the position.
        fill(&mut book, &key, "0.003");
        fill(&mut book, &key, "0.001");
        book.on_order(
            account(),
            &order(&key, OrderCategory::Plain, OrderStatus::Filled, "0.005"),
        );
        let notes = book.notes_for(account());
        assert_eq!(notes.len(), 1, "once: {notes:?}");
        assert!(notes[0].contains("filled again after its stop-loss closed"));
        // No take-profit or trailing stop is placed (they would be cancelled at once).
        assert!(poll(&mut book).is_empty());
        // The entry ended without a second stage: it left the book as finished.
        assert!(book.take_finished().contains(&key));
    }

    fn reconnect(book: &mut AutoBook, resting: bool, held: Vec<Dir>) {
        let resting = move |_: &str, _: Dir, _: &str| resting;
        let held_fn = move |_: &str| held.clone();
        book.after_reconnect(
            account(),
            &ReconnectView {
                resting: &resting,
                held: &held_fn,
            },
        );
    }

    #[test]
    fn a_reconnect_that_finds_a_position_and_no_entry_order_notes_it_and_sends_nothing() {
        let (mut book, key) = booked(spec(1, false));
        // The entry was open when the connection dropped; it filled while no event came.
        reconnect(&mut book, false, vec![Dir::Long]);
        let notes = book.notes_for(account());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("connection was lost"));
        assert!(poll(&mut book).is_empty(), "no stop is sent by itself");
        assert!(book.entries[&key].attention);
        assert!(!book.is_active(account(), "BTCUSDT"));
        assert!(book.take_finished().contains(&key));
        // A fill of the entry seen later still gets its stop, and no take-profit.
        fill(&mut book, &key, "0.001");
        let jobs = poll(&mut book);
        assert_eq!(jobs.len(), 1);
        assert!(matches!(
            jobs[0].work,
            Work::Place {
                stage: Stage::Stop,
                ..
            }
        ));
    }

    #[test]
    fn a_reconnect_that_finds_no_entry_and_no_position_stops_and_notes_once() {
        let (mut book, key) = booked(spec(1, false));
        reconnect(&mut book, false, Vec::new());
        let notes = book.notes_for(account());
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("not found open"));
        assert!(book.entries[&key].attention);
        assert!(!book.is_active(account(), "BTCUSDT"));
        assert!(poll(&mut book).is_empty());
        assert!(book.take_finished().contains(&key));
        fill(&mut book, &key, "0.001");
        let jobs = poll(&mut book);
        assert_eq!(jobs.len(), 1);
        assert!(matches!(
            jobs[0].work,
            Work::Place {
                stage: Stage::Stop,
                ..
            }
        ));
    }

    #[test]
    fn a_reconnect_with_a_fill_seen_keeps_the_stop_and_sends_no_take_profit() {
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.001");
        let stop = poll(&mut book);
        book.done(ok_all(&stop[0]));
        reconnect(&mut book, false, Vec::new());
        assert_eq!(book.notes_for(account()).len(), 1);
        assert!(book.entries.contains_key(&key));
        assert!(
            book.entries[&key]
                .placed
                .iter()
                .any(|p| p.state == Live::Open)
        );
        assert!(poll(&mut book).is_empty(), "no take-profit is sent");
    }

    #[test]
    fn a_cancel_result_that_comes_after_the_event_does_not_report_a_failure() {
        let (mut book, key) = booked(spec(2, true));
        fill(&mut book, &key, "0.001");
        let stop = poll(&mut book);
        let stop_id = place_ids(&stop[0]).remove(0);
        book.done(ok_all(&stop[0]));
        book.on_order(account(), &algo(&stop_id, OrderStatus::Canceled));
        assert_eq!(state_of(&book, &key, &stop_id), Live::Cancelled);
        let which = book.entries[&key].placed[0].which;
        book.done(AutoSummary {
            key: stop[0].key.clone(),
            account: account(),
            symbol: "BTCUSDT".into(),
            steps: vec![(Which::Cancelled(which), StepResult::Unknown)],
        });
        assert_eq!(state_of(&book, &key, &stop_id), Live::Cancelled);
        assert!(book.notes_for(account()).is_empty());
    }

    #[test]
    fn a_stop_that_triggered_and_was_cancelled_is_failed_and_noted() {
        let (mut book, key) = booked(spec(2, true));
        fill(&mut book, &key, "0.001");
        let stop = poll(&mut book);
        let stop_id = place_ids(&stop[0]).remove(0);
        book.done(ok_all(&stop[0]));
        book.on_order(account(), &algo(&stop_id, OrderStatus::Triggered));
        book.on_order(account(), &algo(&stop_id, OrderStatus::Canceled));
        assert_eq!(state_of(&book, &key, &stop_id), Live::Failed);
        assert!(!book.entries[&key].closed);
        assert_eq!(book.notes_for(account()).len(), 1);
    }

    #[test]
    fn a_reconnect_does_not_cry_wolf() {
        // Still resting: nothing to say.
        let (mut book, _) = booked(spec(1, false));
        reconnect(&mut book, true, vec![Dir::Long]);
        assert!(book.notes_for(account()).is_empty());
        // Complete and protected: events were seen, nothing was lost.
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.004");
        book.on_order(
            account(),
            &order(&key, OrderCategory::Plain, OrderStatus::Filled, "0.004"),
        );
        reconnect(&mut book, false, vec![Dir::Long]);
        assert!(book.notes_for(account()).is_empty());
        // The entry of another account is not looked at.
        let (mut book, _) = booked(spec(1, false));
        book.after_reconnect(
            Uuid::from_u128(9),
            &ReconnectView {
                resting: &|_, _, _| false,
                held: &|_| vec![Dir::Long],
            },
        );
        assert!(book.notes_for(account()).is_empty());
    }

    // ── The grant is bound to the entry and the settings ───────────────────

    #[test]
    fn a_fill_of_another_entry_changes_neither_the_book_nor_the_grant() {
        let (mut book, key) = booked(spec(1, false));
        let (other, init) = entry(spec(1, false));
        book.register(other.clone(), init);
        book.settle(
            account(),
            "BTCUSDT",
            &SendResult::Accepted { order_id: None },
        );
        fill(&mut book, &other, "0.001");
        assert_eq!(filled_of(&book, &key), Some(Decimal::ZERO));
        assert_eq!(filled_of(&book, &other), Some(d("0.001")));
        assert_eq!(book.entries[&key].grant.lock().filled(), Decimal::ZERO);
    }

    // ---- d4: protection failures (condition 65) ----

    #[test]
    fn every_fail_reason_has_a_text() {
        let mut seen = std::collections::HashSet::new();
        for (i, reason) in FailReason::ALL.iter().enumerate() {
            assert_eq!(reason.index(), i, "{reason:?} is out of place in ALL");
            let text = reason.text();
            assert!(text.len() > 20, "{reason:?}");
            assert!(text.is_ascii(), "{reason:?}");
            assert!(!text.contains('{'), "{reason:?}");
            assert!(seen.insert(text), "{reason:?} repeats a text");
        }
    }

    #[test]
    fn the_reference_price_is_used_only_when_it_is_fresh_and_of_the_right_kind() {
        use exchange::trade::TriggerBasis::{Last, Mark};
        let p = d("60000");
        let fresh = Duration::from_secs(REF_PRICE_MAX_AGE_SECS);
        let stale = Duration::from_secs(REF_PRICE_MAX_AGE_SECS + 1);
        assert_eq!(reference_price(Last, Some((p, fresh))), Ok(Some(p)));
        assert_eq!(
            reference_price(Last, Some((p, stale))),
            Err(FailReason::StaleReference)
        );
        // No quote: the fill price stands in. A mark-price spec has no mark source.
        assert_eq!(reference_price(Last, None), Ok(None));
        assert_eq!(reference_price(Mark, Some((p, fresh))), Ok(None));
        assert_eq!(reference_price(Mark, Some((p, stale))), Ok(None));
    }

    /// A stop that was refused: the entry holds one failure and offers a retry.
    fn with_failed_stop() -> (AutoBook, String) {
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.001");
        let jobs = poll(&mut book);
        let mut summary = ok_all(&jobs[0]);
        summary.steps[0].1 = StepResult::Failed(FailReason::Rejected, "refused".into());
        book.done(summary);
        (book, key)
    }

    #[test]
    fn a_failed_stop_stays_in_the_book_with_its_reason_and_offers_a_retry() {
        let (mut book, key) = with_failed_stop();
        // Housekeeping does not remove it, even after every stage ended.
        poll(&mut book);
        let views = book.failures_for(account());
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].key, key);
        assert_eq!(views[0].symbol, "BTCUSDT");
        assert_eq!(views[0].reasons, vec![FailReason::Rejected]);
        assert!(views[0].may_retry);
        // The debug text never carries the client id.
        assert!(!format!("{:?}", views[0]).contains(&key));
        // The other account sees nothing.
        assert!(book.failures_for(Uuid::from_u128(9)).is_empty());
    }

    #[test]
    fn a_stop_that_stands_is_not_offered_again() {
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.001");
        let jobs = poll(&mut book);
        book.done(ok_all(&jobs[0]));
        // Something else failed (the switch was off for the take-profits), the stop stands.
        let e = book.entries.get_mut(&key).unwrap();
        AutoBook::add_failure(e, FailReason::NotSent, None);
        let views = book.failures_for(account());
        assert_eq!(views.len(), 1);
        assert!(!views[0].may_retry);
    }

    #[test]
    fn a_stop_added_by_a_retry_clears_the_bare_failures_and_the_old_grant_places_no_second_one() {
        let (mut book, key) = booked(spec(1, false));
        // The connection was lost before any fill was seen: the entry's stop is still to do.
        reconnect(&mut book, false, vec![Dir::Long]);
        let views = book.failures_for(account());
        assert_eq!(views.len(), 1);
        assert!(views[0].may_retry, "{:?}", views[0].reasons);
        // The user confirms a new stop (a new client id, no grant of this entry).
        book.protection_added(account(), "BTCUSDT", Dir::Long);
        assert!(
            book.failures_for(account())
                .iter()
                .all(|v| v.reasons.iter().all(|r| !r.may_leave_bare()))
        );
        // A late fill of the old entry would have sent its own stop with the old grant.
        fill(&mut book, &key, "0.001");
        assert!(
            poll(&mut book).is_empty(),
            "the old grant is not used again"
        );
        // The other direction or another symbol is untouched.
        let (mut other, _) = booked(spec(1, false));
        reconnect(&mut other, false, vec![Dir::Long]);
        other.protection_added(account(), "ETHUSDT", Dir::Long);
        other.protection_added(account(), "BTCUSDT", Dir::Short);
        assert!(!other.failures_for(account()).is_empty());
    }

    #[test]
    fn i_have_handled_it_removes_the_entry_and_unblocks_the_symbol() {
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.001");
        let jobs = poll(&mut book);
        // The stop is going out: the entry cannot be removed under it.
        assert_eq!(
            book.dismiss(account(), &key),
            Err(DismissError::Busy),
            "while a job runs"
        );
        book.done(ok_all(&jobs[0]));
        assert!(book.is_active(account(), "BTCUSDT"));
        // The wrong account or key finds nothing.
        assert_eq!(
            book.dismiss(Uuid::from_u128(9), &key),
            Err(DismissError::NotFound)
        );
        assert_eq!(book.dismiss(account(), "nope"), Err(DismissError::NotFound));
        assert_eq!(book.dismiss(account(), &key), Ok("BTCUSDT".to_string()));
        assert!(!book.is_active(account(), "BTCUSDT"));
        assert!(book.failures_for(account()).is_empty());
        assert_eq!(book.dismiss(account(), &key), Err(DismissError::NotFound));
    }

    #[test]
    fn a_failed_entry_can_be_handled_and_is_gone() {
        let (mut book, key) = with_failed_stop();
        assert_eq!(book.dismiss(account(), &key), Ok("BTCUSDT".to_string()));
        assert!(book.failures_for(account()).is_empty());
        assert!(poll(&mut book).is_empty());
    }

    #[test]
    fn the_handled_warning_facts_follow_the_entry_and_its_orders() {
        // An entry still resting with its stop to come: the entry is open, nothing stands.
        let (mut book, key) = booked(spec(2, true));
        let v = book.failures_for(account());
        assert_eq!(v.len(), 1);
        assert!(v[0].entry_open);
        assert_eq!(v[0].standing, 0);
        // A fill and the stop placed: the stop stands, the entry is still open.
        fill(&mut book, &key, "0.001");
        let stop = poll(&mut book);
        book.done(ok_all(&stop[0]));
        let v = book.failures_for(account());
        assert!(v[0].entry_open);
        assert_eq!(v[0].standing, 1);
        // The entry finished: only what stands is left to say.
        book.on_order(
            account(),
            &order(&key, OrderCategory::Plain, OrderStatus::Filled, "0.001"),
        );
        let v = book.failures_for(account());
        assert!(!v[0].entry_open);
        assert_eq!(v[0].standing, 1);
    }

    #[test]
    fn the_book_speaks_for_a_position_with_a_failure_only_and_knows_its_ids() {
        let (book, key) = booked(spec(1, false));
        // Work left (a stop still to be sent) does not hide a position: the lists decide.
        assert!(!book.covers_part(account(), "BTCUSDT", Dir::Long));
        assert!(book.knows_client_id(&key));
        assert!(!book.knows_client_id("sNOTKNOWNNOTKNOWNNOT"));
        let (failed, _) = with_failed_stop();
        assert!(failed.covers_part(account(), "BTCUSDT", Dir::Long));
    }

    #[test]
    fn a_rate_limit_is_its_own_reason() {
        let list: &[&str] = &[];
        for kind in [RejectKind::RateLimited(429)] {
            match rejected_step(false, &kind, "429", list) {
                StepResult::Failed(reason, _) => assert_eq!(reason, FailReason::RateLimited),
                other => panic!("{other:?}"),
            }
        }
        match rejected_step(false, &RejectKind::Exchange, "-2021", list) {
            StepResult::Failed(reason, _) => assert_eq!(reason, FailReason::Rejected),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_notification_comes_once_per_entry() {
        let (mut book, key) = with_failed_stop();
        assert_eq!(book.take_to_notify(), vec![account()]);
        assert!(book.take_to_notify().is_empty());
        // A second failure on the same entry is not told again.
        let e = book.entries.get_mut(&key).unwrap();
        AutoBook::add_failure(e, FailReason::NotSent, None);
        assert!(book.take_to_notify().is_empty());
        // An entry without a failure is never told.
        let (mut quiet, _) = booked(spec(1, false));
        assert!(quiet.take_to_notify().is_empty());
    }

    #[test]
    fn fills_that_never_add_up_write_one_note_after_the_wait() {
        let (mut book, key) = booked(spec(1, false));
        fill(&mut book, &key, "0.001");
        let stop = poll(&mut book);
        book.done(ok_all(&stop[0]));
        // The exchange says 0.003 filled; only 0.001 was seen.
        book.on_order(
            account(),
            &order(&key, OrderCategory::Plain, OrderStatus::Filled, "0.003"),
        );
        let early = |book: &mut AutoBook, at: i64| polled(book, at, true, Holdings::default());
        assert!(early(&mut book, NOW).is_empty());
        assert!(early(&mut book, NOW + FILLS_WAIT_MS - 1).is_empty());
        assert!(book.notes_for(account()).is_empty());
        // After the wait: a note, once, and the 0.001 seen is protected as usual.
        let rest = early(&mut book, NOW + FILLS_WAIT_MS);
        let notes = book.notes_for(account());
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("fills of the entry were not seen"));
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].filled, d("0.001"));
        early(&mut book, NOW + 3 * FILLS_WAIT_MS);
        assert_eq!(book.notes_for(account()).len(), 1);
        assert_eq!(
            book.failures_for(account())[0].reasons,
            vec![FailReason::FillsNotSeen]
        );
    }

    #[test]
    fn the_fills_wait_follows_the_monotonic_clock_not_the_wall_clock() {
        let (mut book, key) = booked(spec(1, false));
        book.on_order(
            account(),
            &order(&key, OrderCategory::Plain, OrderStatus::Filled, "0.003"),
        );
        let switch = |_: Uuid| true;
        let held = |_: Uuid, _: &str| Holdings::default();
        let at = |book: &mut AutoBook, wall: i64, mono: i64| {
            book.poll(
                wall,
                &PollEnv {
                    switch: &switch,
                    held: &held,
                    mono_ms: mono,
                },
            )
        };
        at(&mut book, NOW, 1_000);
        // The wall clock jumps a day ahead: the wait has not passed.
        at(&mut book, NOW + 86_400_000, 1_000 + FILLS_WAIT_MS - 1);
        assert!(book.notes_for(account()).is_empty());
        // The wall clock goes back: the wait still ends on the monotonic time.
        at(&mut book, NOW - 86_400_000, 1_000 + FILLS_WAIT_MS);
        assert_eq!(book.notes_for(account()).len(), 1);
    }

    #[test]
    fn with_no_fill_seen_the_wait_gives_up_the_rest_but_keeps_the_stop_for_a_late_own_fill() {
        let (mut book, key) = booked(spec(1, false));
        book.on_order(
            account(),
            &order(&key, OrderCategory::Plain, OrderStatus::Filled, "0.003"),
        );
        let at = |book: &mut AutoBook, t: i64| polled(book, t, true, Holdings::default());
        assert!(at(&mut book, NOW).is_empty());
        assert!(at(&mut book, NOW + FILLS_WAIT_MS).is_empty());
        assert_eq!(book.notes_for(account()).len(), 1);
        // The entry is reported once and stays in the book for the user.
        assert!(book.take_finished().contains(&key));
        assert!(book.entries.contains_key(&key));
        assert!(book.take_finished().is_empty());
        let views = book.failures_for(account());
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].reasons, vec![FailReason::FillsNotSeen]);
        // A fill of this very entry that arrives late still gets its closePosition stop.
        fill(&mut book, &key, "0.003");
        let jobs = at(&mut book, NOW + FILLS_WAIT_MS + 1);
        assert_eq!(jobs.len(), 1);
        assert_eq!(place_ids(&jobs[0]).len(), 1);
    }
}
