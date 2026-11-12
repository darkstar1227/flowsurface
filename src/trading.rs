//! Trading state of the GUI (M5 c2b-1, c2b-3): startup, private streams, account state, saved
//! unresolved sends and their phases, standing alerts. **No order is sent from here**: nothing in
//! this module calls `submit`, `cancel_all`, `flatten`, `resolve_unknown` or `auto_close`
//! (the background steps that do are in `order_flow` and `account_flow`), and the only requests
//! it makes are signed reads (equity).
//!
//! Everything is held in memory. Nothing of it is written to `saved-state.json` (condition
//! 78); the risk settings that are saved live in `AccountMeta::risk`.
//!
//! The account's private stream is created only for an account whose connection test passed in
//! this session, and it is kept in [`TradingState`] (not derived from the session) so that a
//! terminal state such as `AuthFailed` is not lost when the session is voided.

use crate::auto_flow::{
    AutoBook, AutoInit, AutoJob, AutoSummary, DismissError, FailureView, FillVerdict, PollEnv,
    ReconnectView,
};
use crate::order_flow::SendResult;
use crate::protect_attach::{Holdings, PendingAttach, PendingAttachTable};
use crate::widget::{BannerKind, banner};
use data::config::exchange_accounts::{AccountMeta, load_credentials};
use data::config::protect::ProtectDefaults;
use data::config::risk::AccountRiskSettings;
use data::config::trade_gate::TradeSession;
use data::config::unknown_store::UnknownStore;
use exchange::account::{
    AccountEnv, AccountKey, AccountProduct, AccountVenue, ProxyState, check_proxy_snapshot,
};
use exchange::signed_http::SignedHttp;
use exchange::trade::stream::{
    CredentialsUnavailable, OrderCategory, PositionSide, PrivateEvent, StreamCloser, StreamContext,
    StreamKey, StreamOutput, StreamState, run,
};
use exchange::trade::{
    Decimal, Dir, EquityBasis, EquityFreshness, EquityObservation, OpenOrder, Position,
    PositionProtection, RestoreError, SharedRisk, StreamEquityFeed, TradeClient, UnknownRecord,
    block_all_entries, lock_from_record,
};
use iced::futures::{Stream, StreamExt, stream};
use iced::{Element, Subscription, Task};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;
use uuid::Uuid;

/// How long deleting or replacing an account waits for its listenKey to be closed before it
/// goes on (security review S4, R7). The Keychain item is deleted afterwards either way.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

/// Equity is read over REST because the stream snapshot carries none. The interval follows the
/// shortest `max_equity_age_secs` of the connected accounts, never longer than this.
const MAX_POLL_INTERVAL: Duration = Duration::from_secs(60);
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Fills remembered for the de-duplication by trade id.
const SEEN_TRADES_CAP: usize = 2048;
/// Stands for a position that cannot be valued: not a valid symbol, so the tracker drops its
/// position book and entries under a per-symbol cap are refused (`PositionUnknown`).
const UNPRICED: &str = "?";

/// The account identity the exchange layer, the lock tables, the journal and the send gate use:
/// always derived from the saved account id in this one way.
pub fn account_key(id: Uuid) -> AccountKey {
    AccountKey::from_bytes(*id.as_bytes())
}

// ── Messages ────────────────────────────────────────────────────────────

/// Carries no credential, listenKey or client id: `StreamOutput` and `StreamKey` print neither,
/// and the boot report's records print a redacted `Debug`.
#[derive(Debug, Clone)]
pub enum Message {
    Booted(BootReport),
    Stream(StreamKey, StreamOutput),
    /// The old stream of the account was closed (or the wait ran out).
    Closed(Uuid),
    EquityTick,
    EquityPolled(Uuid, Option<EquityObservation>),
}

/// What the caller must do after [`TradingState::update`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Effects {
    /// Accounts whose session verdict is void (condition 57).
    pub invalidate: Vec<Uuid>,
}

// ── Startup ─────────────────────────────────────────────────────────────

/// The saved unresolved sends could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadFailed;

/// Where a saved record stands in this run (c2b-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordPhase {
    /// Waits for the account's connection test of this session: no client exists yet, so the
    /// order cannot be looked up. The symbol stays locked ("pending verification").
    AwaitingTest,
    /// The lookup by client id is running.
    Looking,
    /// The lookup ended without an answer. Locked until released by hand or until restart.
    StillUnknown,
    /// An invalid record the user dropped from the file. The exchange layer still blocks the
    /// account's new entries until restart, so the view keeps blocking them too.
    Discarded,
}

/// A saved record. The record carries its client id (needed to look it up, release or discard
/// it); its `Debug` hides the id, and nothing here is ever shown with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordView {
    pub account: AccountKey,
    pub symbol: String,
    /// `Some`: the record could not be turned into a lock by symbol. It is kept, and entries
    /// of its account stay blocked until restart.
    pub error: Option<RestoreError>,
    pub record: UnknownRecord,
    pub phase: RecordPhase,
}

impl RecordView {
    pub fn new(record: UnknownRecord, error: Option<RestoreError>) -> Self {
        Self {
            account: record.account,
            symbol: record.symbol.clone(),
            error,
            record,
            phase: RecordPhase::AwaitingTest,
        }
    }
}

/// How the lookup of a saved record ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupEnd {
    /// The exchange knows the order. Not a fill.
    Found,
    /// Presumed not placed. Nothing was resent.
    NotPlaced,
    /// No answer, or the lookup could not be set up. The lock stays.
    StillUnknown,
}

/// What the unknown list says about one stored record (no client id in the text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordStatus {
    /// The id is invalid: only discarding is possible; the account is blocked until restart.
    Invalid,
    /// Restored at startup; verified only after this session's test of the account.
    AwaitingTest,
    Looking,
    /// The lookup found nothing conclusive; only a manual release or a restart clears it.
    StillUnknown,
    /// Made by a send of this run whose result stayed unknown.
    SessionUnknown,
}

impl RecordStatus {
    pub fn text(self) -> &'static str {
        match self {
            RecordStatus::Invalid => {
                "Invalid id, kept. The account is blocked for new entries until the app restarts."
            }
            RecordStatus::AwaitingTest => {
                "Pending verification: test the account in Exchange accounts first. The symbol \
                 stays locked."
            }
            RecordStatus::Looking => "Looking it up on the exchange…",
            RecordStatus::StillUnknown => {
                "Still unknown after the lookups. The symbol stays locked until you release it \
                 by hand or the app restarts."
            }
            RecordStatus::SessionUnknown => {
                "Result unknown (a send of this run). The symbol stays locked until you release \
                 it by hand or the app restarts."
            }
        }
    }

    /// The user may start a manual release (a valid id, and no lookup running). Not before the
    /// account's test of this session: the test is followed by an automatic lookup, which may
    /// find the order on the exchange, and a release before it would skip that.
    pub fn may_release(self) -> bool {
        matches!(
            self,
            RecordStatus::StillUnknown | RecordStatus::SessionUnknown
        )
    }

    pub fn may_discard(self) -> bool {
        self == RecordStatus::Invalid
    }
}

/// An account as the user is told about it. Two saved accounts with the same alias (a saved
/// state made before duplicates were refused) are told apart by venue and the end of the id.
pub fn display_name(saved: &[AccountMeta], meta: &AccountMeta) -> String {
    let same = |a: &str, b: &str| a.trim().to_lowercase() == b.trim().to_lowercase();
    let duplicated = saved
        .iter()
        .any(|o| o.id != meta.id && same(&o.alias, &meta.alias));
    if duplicated {
        let id = meta.id.simple().to_string();
        let tail = id.get(id.len().saturating_sub(4)..).unwrap_or(&id);
        format!("{} [{} …{tail}]", meta.alias, meta.venue)
    } else {
        meta.alias.clone()
    }
}

/// The name of the saved account behind an exchange-layer account key.
pub fn name_of(saved: &[AccountMeta], account: AccountKey) -> Option<String> {
    saved
        .iter()
        .find(|m| account_key(m.id) == account)
        .map(|m| display_name(saved, m))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootReport {
    /// Every new entry of every account is blocked until restart.
    pub journal_unreadable: bool,
    pub records: Vec<RecordView>,
}

/// The startup order (review B6): a failed load blocks every entry; each loaded record locks
/// its (account, symbol) before any client exists; a record that fails stays in the report,
/// never skipped or deleted. The effects are passed in so the order can be tested; the real
/// ones are [`block_all_entries`] and [`lock_from_record`].
pub fn plan_boot(
    loaded: Result<Vec<UnknownRecord>, LoadFailed>,
    block_all: impl FnOnce(),
    mut lock: impl FnMut(&UnknownRecord) -> Result<(), RestoreError>,
) -> BootReport {
    match loaded {
        Err(LoadFailed) => {
            block_all();
            BootReport {
                journal_unreadable: true,
                records: Vec::new(),
            }
        }
        Ok(records) => BootReport {
            journal_unreadable: false,
            records: records
                .iter()
                .map(|r| RecordView::new(r.clone(), lock(r).err()))
                .collect(),
        },
    }
}

/// Load the saved unresolved sends and lock them. No client, no credential and no Keychain
/// access is needed for this, so it runs before any stream or request.
pub async fn boot() -> BootReport {
    let loaded = UnknownStore::in_data_dir().load_async().await.map_err(|_| {
        log::error!("saved unresolved orders could not be read");
        LoadFailed
    });
    plan_boot(loaded, block_all_entries, lock_from_record)
}

// ── Per-account state ───────────────────────────────────────────────────

/// What a stream needs to be built and to be closed from outside.
struct StreamHandle {
    ctx: StreamContext,
    /// Filled by the subscription when it creates the stream.
    closer: Arc<Mutex<Option<StreamCloser>>>,
}

impl StreamHandle {
    fn closer(&self) -> Option<StreamCloser> {
        self.closer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

enum Phase {
    /// No stream (spot, a failed test, or the account was not tested yet).
    Idle,
    Running(StreamHandle),
    /// The old stream is being closed. `then`: start a new one when that is done.
    Closing {
        then: Option<SignedHttp>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldPosition {
    pub symbol: String,
    pub side: PositionSide,
    /// Long or short. One-way size is signed on the wire, so this is where the direction is
    /// kept (the protection status needs it).
    pub dir: Dir,
    /// Absolute size.
    pub qty: Decimal,
    /// The margin mode the snapshot read for the position. `None` is unknown (a position made
    /// by a stream event carries none): never a mismatch.
    pub margin: Option<exchange::trade::MarginMode>,
    /// The average entry price a position event carried. A snapshot reads none, so it is `None`
    /// until an event brings one; it is only ever drawn, never used to judge anything.
    pub entry_price: Option<Decimal>,
}

struct AccountRuntime {
    meta: AccountMeta,
    /// Bumped for every stream the account gets, so a replaced stream is a new identity.
    generation: u64,
    phase: Phase,
    /// Last state the stream reported (kept when the stream ended in a terminal one).
    state: Option<StreamState>,
    /// One tracker per account for the whole run: a new stream never resets a lock or a peak.
    risk: SharedRisk,
    feed: StreamEquityFeed,
    orders: Vec<OpenOrder>,
    /// A conditional (algo) order event arrived after the last snapshot: the order list may
    /// not match the exchange until the next snapshot (the stream reads it again on such an
    /// event). Never cleared by an event, only by a snapshot.
    orders_dirty: bool,
    positions: Vec<HeldPosition>,
    seen_trades: HashSet<String>,
    seen_order: VecDeque<String>,
    fills_seen: u64,
    /// The stream voided the session, or the last test failed.
    needs_retest: bool,
    risk_unavailable: bool,
    equity_inflight: bool,
    /// A snapshot of the current stream was applied and the state has not left `Connected`
    /// since: only then is a flat position list known to be current (an empty book may be
    /// refreshed, never before the first snapshot of a new stream).
    book_trusted: bool,
    /// Events may have been missed (the connection dropped, the stream was rebuilt, or the
    /// stream asked for a resync): the automatic entries are checked once the next
    /// `Connected` state arrives, after the snapshot and the events held back during the read.
    /// A resync caused only by a conditional order event does not set it.
    recheck_auto: bool,
}

fn equity_basis(venue: AccountVenue, product: AccountProduct) -> EquityBasis {
    match (venue, product) {
        (AccountVenue::Okx, _) => EquityBasis::OkxTotalEq,
        (AccountVenue::Binance, AccountProduct::UsdM) => EquityBasis::BinanceTotalMarginBalance,
        (AccountVenue::Binance, AccountProduct::Spot) => EquityBasis::SpotMarkValued,
    }
}

fn is_spot(meta: &AccountMeta) -> bool {
    meta.venue == AccountVenue::Binance && meta.product == AccountProduct::Spot
}

impl AccountRuntime {
    fn new(meta: &AccountMeta, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        let account = account_key(meta.id);
        let tracker = meta
            .risk
            .tracker(account, equity_basis(meta.venue, meta.product));
        let key = StreamKey {
            account,
            venue: meta.venue,
            env: meta.env,
            product: meta.product,
            generation: 0,
        };
        Self {
            meta: meta.clone(),
            generation: 0,
            phase: Phase::Idle,
            state: is_spot(meta).then_some(StreamState::Unavailable),
            risk: SharedRisk::new(tracker, clock),
            feed: StreamEquityFeed::new(&key),
            orders: Vec::new(),
            positions: Vec::new(),
            seen_trades: HashSet::new(),
            seen_order: VecDeque::new(),
            fills_seen: 0,
            needs_retest: false,
            risk_unavailable: false,
            equity_inflight: false,
            book_trusted: false,
            recheck_auto: false,
            orders_dirty: false,
        }
    }

    fn account(&self) -> AccountKey {
        account_key(self.meta.id)
    }

    fn current_key(&self) -> Option<StreamKey> {
        match &self.phase {
            Phase::Running(h) => Some(h.ctx.key),
            _ => None,
        }
    }

    fn forget_lists(&mut self) {
        self.orders.clear();
        self.positions.clear();
    }

    fn note_poisoned<T>(&mut self, r: Result<T, exchange::trade::RiskPoisoned>) {
        if r.is_err() {
            self.risk_unavailable = true;
        }
    }

    /// Remember a fill once. `true` when it is new. A fill without a trade id cannot be told
    /// apart, so it counts every time. The key is the symbol and the trade id: trade ids are
    /// numbered per symbol, so the same id on two symbols is two fills.
    fn note_fill(&mut self, symbol: &str, trade_id: Option<&str>) -> bool {
        if let Some(id) = trade_id {
            let key = format!("{symbol}\u{0}{id}");
            if !self.seen_trades.insert(key.clone()) {
                return false;
            }
            self.seen_order.push_back(key);
            while self.seen_order.len() > SEEN_TRADES_CAP {
                if let Some(old) = self.seen_order.pop_front() {
                    self.seen_trades.remove(&old);
                }
            }
        }
        self.fills_seen = self.fills_seen.saturating_add(1);
        true
    }

    /// Positions from the exchange's reading (a snapshot): replaces the book.
    fn set_positions_from_snapshot(&mut self, positions: &[Position]) {
        self.positions = positions
            .iter()
            .filter(|p| !p.qty.is_zero())
            .map(|p| HeldPosition {
                symbol: p.symbol.clone(),
                side: if p.hedge_side {
                    match p.dir {
                        Dir::Long => PositionSide::Long,
                        Dir::Short => PositionSide::Short,
                    }
                } else {
                    PositionSide::Both
                },
                dir: p.dir,
                qty: p.qty,
                margin: p.margin,
                // A zero or negative price is no price (a flat row, a bad field).
                entry_price: p.entry_price.filter(Decimal::is_positive),
            })
            .collect();
    }

    /// Take a snapshot: the lists, then the risk tracker's position book, both from this
    /// snapshot alone. `apply` hands the values to the tracker (the real one is
    /// `SharedRisk::snapshot_applied`, which also takes the snapshot's epoch).
    fn apply_snapshot(
        &mut self,
        orders: &[OpenOrder],
        positions: &[Position],
        apply: impl FnOnce(
            &SharedRisk,
            Vec<(String, Decimal)>,
        ) -> Result<(), exchange::trade::RiskPoisoned>,
    ) {
        self.orders = orders.to_vec();
        self.orders_dirty = false;
        self.book_trusted = true;
        self.set_positions_from_snapshot(positions);
        let notionals = position_notionals(&self.positions);
        let r = apply(&self.risk, notionals);
        self.note_poisoned(r);
    }

    /// A position event: the only thing besides a snapshot that changes the held positions (a
    /// fill never adds to them).
    fn apply_position_event(
        &mut self,
        symbol: &str,
        side: PositionSide,
        qty: Decimal,
        entry_price: Option<Decimal>,
    ) {
        // The margin mode is read from snapshots only: an event keeps what the snapshot said.
        let margin = self
            .positions
            .iter()
            .find(|p| p.symbol == symbol && p.side == side)
            .and_then(|p| p.margin);
        self.positions
            .retain(|p| !(p.symbol == symbol && p.side == side));
        if qty.is_zero() {
            return;
        }
        // One-way size is signed on the wire; the direction is kept, then only the absolute
        // size is held.
        let abs = qty.checked_abs().unwrap_or(qty);
        let dir = match side {
            PositionSide::Long => Dir::Long,
            PositionSide::Short => Dir::Short,
            PositionSide::Both if qty.is_negative() => Dir::Short,
            PositionSide::Both => Dir::Long,
        };
        self.positions.push(HeldPosition {
            symbol: symbol.to_string(),
            side,
            dir,
            qty: abs,
            margin,
            entry_price,
        });
    }

    /// The held positions in the exchange crate's form, for the protection functions.
    fn exchange_positions(&self) -> Vec<Position> {
        self.positions
            .iter()
            .map(|p| Position {
                symbol: p.symbol.clone(),
                dir: p.dir,
                qty: p.qty,
                hedge_side: p.side != PositionSide::Both,
                margin: p.margin,
                leverage: None,
                entry_price: p.entry_price,
            })
            .collect()
    }

    fn has_equity_limits(&self) -> bool {
        self.meta.risk.daily_loss_limit.is_some() || self.meta.risk.drawdown_bps.is_some()
    }
}

/// The position values to give the risk tracker, **from these positions only** (never from an
/// earlier book). A snapshot carries sizes, not prices, and this build has no price source
/// yet, so a held position cannot be valued: it is reported as [`UNPRICED`], which makes the
/// tracker refuse entries under a per-symbol cap instead of counting the position as zero.
/// A flat account gives an empty list, which is a known empty book.
pub fn position_notionals(positions: &[HeldPosition]) -> Vec<(String, Decimal)> {
    if positions.is_empty() {
        Vec::new()
    } else {
        vec![(UNPRICED.to_string(), Decimal::ZERO)]
    }
}

// ── Subscription ────────────────────────────────────────────────────────

/// What an iced subscription hashes for one private stream: the [`StreamKey`] only (the
/// generation inside it is what replaces a stream). The rest travels along and is never part of
/// the identity.
#[derive(Clone)]
struct PrivateRun {
    ctx: StreamContext,
    closer: Arc<Mutex<Option<StreamCloser>>>,
}

impl Hash for PrivateRun {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.ctx.key.hash(state);
    }
}

/// Created lazily on the first poll, so [`run`] certainly happens inside the runtime. The
/// closer is handed to the GUI before the first output, because the subscription owns the
/// stream from here on.
fn run_private(d: &PrivateRun) -> impl Stream<Item = Message> + use<> {
    let ctx = d.ctx.clone();
    let slot = d.closer.clone();
    let key = ctx.key;
    stream::once(async move {
        let stream = run(ctx);
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(stream.closer());
        stream
    })
    .flatten()
    .map(move |output| Message::Stream(key, output))
}

/// Wait for `close` for at most `limit`. `true` when it finished in time. Either way the caller
/// goes on (deleting the Keychain item, starting the next stream).
pub async fn close_within(limit: Duration, close: impl Future<Output = ()>) -> bool {
    tokio::time::timeout(limit, close).await.is_ok()
}

// ── State ───────────────────────────────────────────────────────────────

pub struct TradingState {
    /// The proxy setting as it is now. Streams and clients read it on every connect and every
    /// request; a broken setting reads as `Broken`.
    proxy: Arc<RwLock<ProxyState>>,
    /// `None` until the startup load has been applied: no stream starts before.
    boot: Option<BootReport>,
    accounts: BTreeMap<Uuid, AccountRuntime>,
    /// A send reported that its journal could not be written.
    journal_failed: bool,
    /// Sends of this run that are still going or ended without an answer.
    /// Only `Unresolved` is kept here: it must outlive any later send on the same pair.
    pairs: BTreeMap<(Uuid, String), PairState>,
    /// Sends that are going now, kept apart so finishing one never erases an unresolved result.
    inflight: BTreeSet<(Uuid, String)>,
    /// Accounts with a cancel-all or close-all running: no other order of the account is
    /// prepared or sent meanwhile.
    bulk_running: BTreeSet<Uuid>,
    /// The protection each confirmed entry listed (d2): what the panel and the close warning
    /// say. The orders themselves are sent by `auto`.
    attach: PendingAttachTable,
    /// What is sent after an entry fills, and the state of it (d3).
    auto: AutoBook,
    /// Positions with no stop-loss, as the watch has confirmed them (d5).
    watch: BareWatch,
    /// Client ids of the requests this session confirmed (memory only, capped): an algo order
    /// with an id of the app's shape that is in neither this list nor the book may be left over
    /// from an earlier session.
    sent_ids: VecDeque<String>,
    /// Cancels opened from a stray-order button: checked again when the user confirms.
    stray_cancels: HashSet<(Uuid, String)>,
    /// Stop and target replaces in flight, and the failures of those that left a position
    /// without its order (f3a2).
    replace: crate::replace_flow::ReplaceFlow,
}

/// How long a position has to stay without a stop on fresh lists before it is called bare. The
/// stop of a normal automatic entry is sent right after the fill and shows in the order list a
/// moment later: that gap is not a bare position.
pub const BARE_GRACE_MS: i64 = 15_000;

const MAX_SENT_IDS: usize = 512;

/// One position without a whole stop, as the banner shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BareRow {
    pub symbol: String,
    pub dir: Dir,
    pub protection: exchange::trade::Protection,
}

impl BareRow {
    /// The lower severity: only take-profit orders stand (user decision).
    pub fn lower_severity(&self) -> bool {
        self.protection == exchange::trade::Protection::TargetOnly
    }

    /// The banner text. Said plainly, never as "protected".
    pub fn text(&self, alias: &str) -> String {
        let side = match self.dir {
            Dir::Long => "long",
            Dir::Short => "short",
        };
        let what = match self.protection {
            exchange::trade::Protection::TargetOnly => "has take-profit orders but no stop-loss",
            exchange::trade::Protection::PartialStop => {
                "has a stop-loss that may not cover the whole position"
            }
            exchange::trade::Protection::TrailingOnly => {
                "has only a trailing stop, which may not be active yet"
            }
            _ => "has no stop-loss",
        };
        format!("{alias}: the {side} position on {} {what}.", self.symbol)
    }
}

/// What the watch for bare positions remembers. Memory only.
#[derive(Default)]
struct BareWatch {
    /// Since when (monotonic ms) a position has been bare on fresh lists without a break.
    first_seen: HashMap<(Uuid, String, Dir), i64>,
    /// Positions past the grace time, per account.
    confirmed: HashMap<Uuid, Vec<BareRow>>,
    /// Positions a system notification went out for. Cleared only by a fresh verdict that the
    /// position is gone or has a stop; going stale does not clear it.
    told: HashSet<(Uuid, String, Dir)>,
    /// Accounts to notify (a bare position was confirmed that nobody was told about).
    to_notify: Vec<Uuid>,
}

/// An order the panel offers to cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrayOrder {
    /// The exchange's id of the order (algo id), used to name it in a click.
    pub id: String,
    pub symbol: String,
    pub text: String,
    /// Looks like an order of this app that nothing in this session owns (as opposed to a
    /// protective order with no position).
    pub residual: bool,
    pub can_cancel: bool,
    /// The order is the stop-loss of a position that is still held: the panel only tells, and
    /// offers no cancel (cancelling it would leave the position without a stop-loss).
    pub guards_position: bool,
}

/// What the account's lists say about orders without a position (d5): `None` when the lists may
/// be stale (nothing is concluded then).
pub fn stray_orders(
    positions: &[Position],
    orders: &[OpenOrder],
    lists_stale: bool,
    okx: bool,
    owns: &dyn Fn(&str) -> bool,
) -> Option<Vec<StrayOrder>> {
    use exchange::trade::{cancellable_by_id, describe_protective};
    let orphans = exchange::trade::orphan_protectives(positions, orders, lists_stale)?;
    let mut out: Vec<StrayOrder> = orphans
        .into_iter()
        .map(|o| StrayOrder {
            id: o.id.clone(),
            symbol: o.symbol.clone(),
            text: format!(
                "{}: no position to protect. {}",
                o.symbol,
                describe_protective(o).unwrap_or_else(|| "Protective order".to_string())
            ),
            residual: false,
            can_cancel: cancellable_by_id(o, okx),
            guards_position: false,
        })
        .collect();
    // Orders of the app's shape that nothing owns. On OKX the algo orders an entry attached
    // carry ids of the same shape and are not tracked here: not judged.
    if !okx {
        for o in exchange::trade::residual_auto_orders(orders, lists_stale, owns)? {
            if out.iter().any(|s| s.id == o.id) {
                continue;
            }
            let line = describe_protective(o).unwrap_or_else(|| "Conditional order".to_string());
            // A stop that stands for a position still held is not offered for cancelling: after
            // a restart the app cannot tell it from one it still owns.
            let guarded = exchange::trade::stop_guarded_position(positions, o);
            let held_symbol = positions
                .iter()
                .any(|p| p.symbol == o.symbol && !p.qty.is_zero());
            let text = if let Some(p) = guarded {
                let side = match p.dir {
                    Dir::Long => "long",
                    Dir::Short => "short",
                };
                format!(
                    "This order looks like it was placed by this app in an earlier session. It is \
                     the stop-loss of your {side} {} position; keep it unless you replace it. {line}",
                    o.symbol
                )
            } else {
                let mut t = format!(
                    "{}: this looks like an automatic order of this app that nothing in this \
                     session owns (left over from an earlier session, or its entry was \
                     removed). {line}",
                    o.symbol
                );
                if held_symbol {
                    t.push_str(&format!(
                        " Cancelling it changes the protection of the {} position.",
                        o.symbol
                    ));
                }
                t
            };
            out.push(StrayOrder {
                id: o.id.clone(),
                symbol: o.symbol.clone(),
                text,
                residual: true,
                can_cancel: guarded.is_none() && cancellable_by_id(o, okx),
                guards_position: guarded.is_some(),
            });
        }
    }
    Some(out)
}

/// What blocks the buttons of one (account, symbol) (condition 61).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairState {
    /// A send (and the lookup that follows an unknown result) is running.
    InFlight,
    /// The result stayed unknown after the lookups: the symbol is locked for entries until
    /// the user releases it by hand (c2b-3) or the app restarts.
    Unresolved,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl TradingState {
    pub fn new(proxy: ProxyState) -> Self {
        Self {
            proxy: Arc::new(RwLock::new(proxy)),
            boot: None,
            accounts: BTreeMap::new(),
            journal_failed: false,
            pairs: BTreeMap::new(),
            inflight: BTreeSet::new(),
            bulk_running: BTreeSet::new(),
            attach: PendingAttachTable::default(),
            auto: AutoBook::default(),
            watch: BareWatch::default(),
            sent_ids: VecDeque::new(),
            stray_cancels: HashSet::new(),
            replace: crate::replace_flow::ReplaceFlow::default(),
        }
    }

    pub fn set_proxy(&self, proxy: ProxyState) {
        *self.proxy.write().unwrap_or_else(PoisonError::into_inner) = proxy;
    }

    /// For a stream or a client: the live proxy setting. Poisoned reads as `Broken` (refuse).
    pub fn proxy_reader(&self) -> Arc<dyn Fn() -> ProxyState + Send + Sync> {
        let cell = self.proxy.clone();
        Arc::new(move || cell.read().map_or(ProxyState::Broken, |p| p.clone()))
    }

    pub fn on_booted(&mut self, report: BootReport) {
        self.boot = Some(report);
    }

    /// The account's stream was built for another proxy setting than the one now.
    fn proxy_changed_for(&self, rt: &AccountRuntime) -> bool {
        let Phase::Running(h) = &rt.phase else {
            return false;
        };
        let now = self.proxy_reader()();
        check_proxy_snapshot(h.ctx.signed.proxy_state(), &now).is_err()
    }

    /// The saved unresolved sends were loaded and locked (N4). Until then no order may be
    /// prepared or sent: an entry could go out for a symbol that a saved record locks.
    pub fn orders_allowed(&self) -> bool {
        self.boot.is_some()
    }

    /// The account's one risk tracker, for the send gate (never another one).
    pub fn risk_for(&self, id: Uuid) -> Option<SharedRisk> {
        self.accounts.get(&id).map(|rt| rt.risk.clone())
    }

    pub fn stream_state(&self, id: Uuid) -> Option<StreamState> {
        self.accounts.get(&id)?.state
    }

    pub fn needs_retest(&self, id: Uuid) -> bool {
        self.accounts.get(&id).is_some_and(|rt| rt.needs_retest)
    }

    /// The open orders of the last snapshot of the account's stream (may be stale: see
    /// `stream_state`).
    pub fn open_orders(&self, id: Uuid) -> &[OpenOrder] {
        self.accounts
            .get(&id)
            .map_or(&[], |rt| rt.orders.as_slice())
    }

    /// Symbols with an open entry order that carries attached take-profit / stop-loss on the
    /// exchange (OKX): they exist but act only once the entry is fully filled.
    pub fn attached_on_exchange(&self, id: Uuid) -> Vec<String> {
        let mut v: Vec<String> = self
            .open_orders(id)
            .iter()
            .filter(|o| {
                o.attached.is_some() && !matches!(o.kind, exchange::trade::OpenKind::Algo(_))
            })
            .map(|o| o.symbol.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// The positions of the last snapshot and the events after it (may be stale: see
    /// `lists_stale`).
    pub fn positions(&self, id: Uuid) -> &[HeldPosition] {
        self.accounts
            .get(&id)
            .map_or(&[], |rt| rt.positions.as_slice())
    }

    /// A replace of a protective order of the account is being sent (f3a2).
    pub fn replace_busy(&self, id: Uuid) -> bool {
        self.replace.is_busy(id)
    }

    pub fn replace_flow_mut(&mut self) -> &mut crate::replace_flow::ReplaceFlow {
        &mut self.replace
    }

    /// What decides whether a stop or target line of the account can be dragged (f3a2), from the
    /// account's own state. `None` for an account the app has no runtime for.
    pub fn replace_guard(&self, id: Uuid, live_open: bool) -> Option<crate::replace_flow::Guard> {
        let rt = self.accounts.get(&id)?;
        Some(crate::replace_flow::Guard {
            venue: rt.meta.venue,
            product: rt.meta.product,
            live: rt.meta.env == exchange::account::AccountEnv::Live,
            live_open,
            stream: rt.state,
            lists_stale: self.lists_stale(id),
            busy: self.replace.is_busy(id),
        })
    }

    /// An automatic protection job of the app works on this order (d3). The order is known by its
    /// client id; a row that carries none is judged by the symbol having a job at work.
    pub fn is_auto_order(&self, id: Uuid, row: &OpenOrder) -> bool {
        match row.client_id.as_deref() {
            Some(client_id) => self.auto.knows_client_id(client_id),
            None => self.auto.is_active(id, &row.symbol),
        }
    }

    /// The order and position lists may not match the exchange: no stream, one that is not
    /// synchronised, or none whose snapshot was applied since it last connected.
    pub fn lists_stale(&self, id: Uuid) -> bool {
        self.accounts.get(&id).is_none_or(|rt| {
            rt.state != Some(StreamState::Connected)
                || !rt.book_trusted
                || rt.needs_retest
                || rt.orders_dirty
        })
    }

    /// The protection of each held position, judged from the last snapshot and the events
    /// after it. When the lists may be stale every answer is `Undetermined`, which reads
    /// "cannot tell", never "protected".
    pub fn protection(&self, id: Uuid) -> Vec<PositionProtection> {
        let mut all = self.protection_listed(id);
        // Protection that was listed for an entry and is not in force yet: said so, and never
        // "protected", whatever the lists show.
        for p in &mut all {
            if self.attach.has_pending(id, &p.symbol) {
                p.protection = exchange::trade::Protection::EntryPending;
            }
        }
        all
    }

    /// What the lists alone say, before the pending-entry overlay: the bare judgment uses this,
    /// so a pending entry never hides a position that has no stop.
    fn protection_listed(&self, id: Uuid) -> Vec<PositionProtection> {
        let Some(rt) = self.accounts.get(&id) else {
            return Vec::new();
        };
        exchange::trade::protected_status(
            &rt.exchange_positions(),
            &rt.orders,
            self.lists_stale(id),
        )
    }

    /// Look again at every account's positions (d5). Called on the poll tick with a monotonic
    /// clock. Only fresh lists give a verdict: while they may be stale nothing is concluded, the
    /// banner is empty ("cannot tell" is the panel's stale line). The grace clock of a
    /// position and the record of who was told are kept: going stale gives no new grace time.
    pub fn watch_bare(&mut self, mono_ms: i64) {
        let ids: Vec<Uuid> = self.accounts.keys().copied().collect();
        for id in ids {
            if self.lists_stale(id) {
                // Nothing is shown or concluded, but the clock of a position keeps running: going
                // stale must not give a bare position a new grace time.
                self.watch.confirmed.remove(&id);
                continue;
            }
            let all = self.protection_listed(id);
            let bare: Vec<BareRow> = exchange::trade::bare_positions(&all)
                .into_iter()
                // A failure the d4 banner already shows for this very position is not repeated.
                .filter(|b| !self.auto.covers_part(id, &b.symbol, b.dir))
                .map(|b| BareRow {
                    symbol: b.symbol.clone(),
                    dir: b.dir,
                    protection: b.protection,
                })
                .collect();
            let key = |r: &BareRow| (id, r.symbol.clone(), r.dir);
            self.watch
                .first_seen
                .retain(|k, _| k.0 != id || bare.iter().any(|r| key(r) == *k));
            let mut confirmed = Vec::new();
            for row in bare {
                let first = *self.watch.first_seen.entry(key(&row)).or_insert(mono_ms);
                if mono_ms.saturating_sub(first) < BARE_GRACE_MS {
                    continue;
                }
                // A take-profit-only position is the lower severity: shown, never notified.
                if !row.lower_severity() && self.watch.told.insert(key(&row)) {
                    self.watch.to_notify.push(id);
                }
                confirmed.push(row);
            }
            self.watch.confirmed.insert(id, confirmed);
            // Told again only after the position was seen gone or with a stop on fresh lists.
            let resolved = |k: &(Uuid, String, Dir)| {
                k.0 == id
                    && !all.iter().any(|p| {
                        p.symbol == k.1
                            && p.dir == k.2
                            && p.protection != exchange::trade::Protection::Stop
                    })
            };
            self.watch.told.retain(|k| !resolved(k));
        }
    }

    /// The positions of the account that have no whole stop-loss, once confirmed on fresh lists.
    pub fn bare_rows(&self, id: Uuid) -> &[BareRow] {
        self.watch.confirmed.get(&id).map_or(&[], Vec::as_slice)
    }

    /// The accounts with a bare position that nobody was told about yet.
    pub fn take_bare_notices(&mut self) -> Vec<Uuid> {
        std::mem::take(&mut self.watch.to_notify)
    }

    /// The protective orders that stand for each held position, as lines for the panel. Empty
    /// while the lists may be stale: what stands cannot be said then.
    pub fn protective_lines(&self, id: Uuid, symbol: &str, dir: Dir) -> Vec<String> {
        let Some(rt) = self.accounts.get(&id) else {
            return Vec::new();
        };
        if self.lists_stale(id) {
            return Vec::new();
        }
        rt.exchange_positions()
            .iter()
            .filter(|p| p.symbol == symbol && p.dir == dir)
            .flat_map(|p| exchange::trade::protecting_orders(p, &rt.orders))
            .filter_map(exchange::trade::describe_protective)
            .collect()
    }

    /// Protective orders with no position, and orders that look left over from an earlier
    /// session. `None` when the lists may be stale.
    pub fn stray_orders(&self, id: Uuid) -> Option<Vec<StrayOrder>> {
        let rt = self.accounts.get(&id)?;
        let okx = rt.meta.venue == exchange::account::AccountVenue::Okx;
        let owns = |client: &str| {
            self.auto.knows_client_id(client) || self.sent_ids.iter().any(|s| s == client)
        };
        stray_orders(
            &rt.exchange_positions(),
            &rt.orders,
            self.lists_stale(id),
            okx,
            &owns,
        )
    }

    /// The order with this exchange id is, on fresh lists, still one the panel offers to cancel
    /// (an order with no position, or one that looks left over and is not the stop-loss of a
    /// position still held) and can be cancelled by id.
    pub fn is_stray(&self, id: Uuid, order_id: &str) -> bool {
        self.stray_orders(id)
            .is_some_and(|v| v.iter().any(|s| s.id == order_id && s.can_cancel))
    }

    /// A cancel was opened from a stray-order button: it is judged again at the confirmation.
    pub fn mark_stray_cancel(&mut self, id: Uuid, order_id: &str) {
        self.stray_cancels.insert((id, order_id.to_string()));
    }

    /// Was this cancel opened from a stray-order button?
    pub fn has_stray_mark(&self, id: Uuid, order_id: &str) -> bool {
        self.stray_cancels.contains(&(id, order_id.to_string()))
    }

    /// Drops every mark: the confirmation window was closed without confirming, so a later
    /// cancel of the same order from elsewhere is not judged as a stray one.
    pub fn clear_stray_marks(&mut self) {
        self.stray_cancels.clear();
    }

    /// Takes the mark away (the cancel was confirmed, or it was refused).
    pub fn take_stray_mark(&mut self, id: Uuid, order_id: &str) -> bool {
        self.stray_cancels.remove(&(id, order_id.to_string()))
    }

    /// The client ids of a request the user confirmed this session, kept in memory only.
    pub fn note_sent_ids(&mut self, ids: &[&str]) {
        for id in ids {
            if !self.sent_ids.iter().any(|s| s == id) {
                self.sent_ids.push_back((*id).to_string());
            }
        }
        while self.sent_ids.len() > MAX_SENT_IDS {
            self.sent_ids.pop_front();
        }
    }

    /// OKX: a position of the symbol is held in a margin mode other than the cross mode this
    /// build sends orders in. Only a known mode counts; an unknown one is not a mismatch.
    pub fn margin_conflict(&self, id: Uuid, symbol: &str) -> bool {
        self.accounts.get(&id).is_some_and(|rt| {
            rt.meta.venue == exchange::account::AccountVenue::Okx
                && rt.positions.iter().any(|p| {
                    p.symbol.eq_ignore_ascii_case(symbol)
                        && p.margin == Some(exchange::trade::MarginMode::Isolated)
                })
        })
    }

    /// The reason of the account's risk lock, when one is on.
    pub fn trip_reason(&self, id: Uuid) -> Option<&'static str> {
        match self.accounts.get(&id)?.risk.is_tripped() {
            Ok(Some(reason)) => Some(reason.as_str()),
            _ => None,
        }
    }

    /// The user acknowledged the risk lock (after a confirmation step in the GUI).
    pub fn acknowledge_trip(&mut self, id: Uuid) -> bool {
        let Some(rt) = self.accounts.get_mut(&id) else {
            return false;
        };
        let r = rt.risk.acknowledge_trip();
        let ok = r.is_ok();
        rt.note_poisoned(r);
        ok
    }

    /// The saved "close everything when a limit trips" setting. Status only: this build does
    /// not run an automatic close.
    pub fn auto_close_setting(&self, id: Uuid) -> Option<bool> {
        self.accounts.get(&id).map(|rt| rt.meta.risk.auto_close)
    }

    pub fn begin_bulk(&mut self, id: Uuid) {
        self.bulk_running.insert(id);
    }

    pub fn finish_bulk(&mut self, id: Uuid) {
        self.bulk_running.remove(&id);
    }

    pub fn bulk_running(&self, id: Uuid) -> bool {
        self.bulk_running.contains(&id)
    }

    /// What the unknown list says about a stored record.
    pub fn record_status(&self, id: Uuid, record: &UnknownRecord) -> RecordStatus {
        if !record.has_valid_client_id() {
            return RecordStatus::Invalid;
        }
        // A bulk run looks its unknown sends up and writes their records meanwhile: until it
        // ends, nothing of this account may be released by hand.
        if self.bulk_running.contains(&id) {
            return RecordStatus::Looking;
        }
        let kept = self.boot.as_ref().and_then(|b| {
            b.records
                .iter()
                .find(|r| r.account == record.account && r.record.client_id == record.client_id)
        });
        match kept {
            Some(r) if r.error.is_some() => RecordStatus::Invalid,
            Some(r) => match r.phase {
                RecordPhase::AwaitingTest => RecordStatus::AwaitingTest,
                RecordPhase::Looking => RecordStatus::Looking,
                RecordPhase::StillUnknown | RecordPhase::Discarded => RecordStatus::StillUnknown,
            },
            None => match self.pair_state(id, &record.symbol) {
                Some(PairState::InFlight) => RecordStatus::Looking,
                _ => RecordStatus::SessionUnknown,
            },
        }
    }

    /// The account was tested and may be used: the saved records of it that have a valid id and
    /// no lookup running are looked up now (once; a second call while they run gives none). A
    /// record with an invalid id is never looked up.
    pub fn begin_lookup(&mut self, id: Uuid) -> Vec<UnknownRecord> {
        let account = account_key(id);
        let Some(boot) = self.boot.as_mut() else {
            return Vec::new();
        };
        boot.records
            .iter_mut()
            .filter(|r| {
                r.account == account
                    && r.error.is_none()
                    && matches!(
                        r.phase,
                        RecordPhase::AwaitingTest | RecordPhase::StillUnknown
                    )
            })
            .map(|r| {
                r.phase = RecordPhase::Looking;
                r.record.clone()
            })
            .collect()
    }

    /// A lookup ended. An answer drops the record from the locked list (the exchange layer
    /// already released its lock and cleared the file); no answer keeps it locked.
    pub fn lookup_finished(&mut self, record: &UnknownRecord, end: LookupEnd) {
        let Some(boot) = self.boot.as_mut() else {
            return;
        };
        let same =
            |r: &RecordView| r.account == record.account && r.record.client_id == record.client_id;
        match end {
            LookupEnd::Found | LookupEnd::NotPlaced => {
                boot.records.retain(|r| !same(r));
                if end == LookupEnd::Found {
                    self.auto.entry_stands(&record.client_id);
                }
            }
            LookupEnd::StillUnknown => {
                for r in boot.records.iter_mut().filter(|r| same(r)) {
                    r.phase = RecordPhase::StillUnknown;
                }
            }
        }
    }

    /// The user released a send by hand and its saved record was removed. The symbol stops
    /// counting as unknown unless another saved record still names it (`still_stored`).
    pub fn record_released(&mut self, id: Uuid, symbol: &str, client_id: &str, still_stored: bool) {
        let account = account_key(id);
        if let Some(boot) = self.boot.as_mut() {
            boot.records
                .retain(|r| !(r.account == account && r.record.client_id == client_id));
        }
        if !still_stored {
            let key = (id, symbol.to_string());
            if self.pairs.get(&key) == Some(&PairState::Unresolved) {
                self.pairs.remove(&key);
            }
        }
    }

    /// An invalid record was dropped from the file. The account stays blocked until restart
    /// (the exchange layer keeps its block), so the record stays here as `Discarded`.
    pub fn record_discarded(&mut self, id: Uuid, client_id: &str) {
        let account = account_key(id);
        if let Some(boot) = self.boot.as_mut() {
            for r in boot
                .records
                .iter_mut()
                .filter(|r| r.account == account && r.record.client_id == client_id)
            {
                r.phase = RecordPhase::Discarded;
            }
        }
    }

    /// A saved unresolved send locks the (account, symbol) (or the whole account) until restart.
    pub fn is_record_locked(&self, id: Uuid, symbol: &str) -> bool {
        let account = account_key(id);
        self.boot.as_ref().is_some_and(|b| {
            b.journal_unreadable
                || b.records
                    .iter()
                    .any(|r| r.account == account && (r.error.is_some() || r.symbol == symbol))
        })
    }

    /// A send went out although its journal record could not be written (condition: standing
    /// warning that unknown results will not be remembered).
    pub fn set_journal_failed(&mut self) {
        self.journal_failed = true;
    }

    pub fn pair_state(&self, id: Uuid, symbol: &str) -> Option<PairState> {
        let key = (id, symbol.to_string());
        if self.inflight.contains(&key) {
            Some(PairState::InFlight)
        } else {
            self.pairs.get(&key).copied()
        }
    }

    /// A send on the (account, symbol) starts. An earlier unresolved result stays.
    pub fn begin_send(&mut self, id: Uuid, symbol: &str) {
        self.inflight.insert((id, symbol.to_string()));
    }

    /// The send ended. Only an unknown result is kept (until restart); a later send that ends
    /// well never clears an earlier unknown one.
    pub fn finish_send(&mut self, id: Uuid, symbol: &str, unresolved: bool) {
        let key = (id, symbol.to_string());
        self.inflight.remove(&key);
        if unresolved {
            self.pairs.insert(key, PairState::Unresolved);
        }
    }

    /// A replace ended: its send ends, and only a result that stayed unknown keeps the symbol
    /// blocked. Returns whether it does (the unknown list is then read again). The order lists
    /// are never marked here: only an exchange event marks them, and a snapshot clears the mark.
    pub fn on_replace_result(&mut self, summary: &crate::replace_flow::ReplaceSummary) -> bool {
        let unresolved = crate::replace_flow::keeps_symbol_blocked(&summary.result);
        self.finish_send(summary.account, &summary.symbol, unresolved);
        if summary.journal_failed {
            self.set_journal_failed();
        }
        unresolved
    }

    /// The closer of the account's running stream, for deleting the account.
    pub fn closer_for(&self, id: Uuid) -> Option<StreamCloser> {
        match &self.accounts.get(&id)?.phase {
            Phase::Running(h) => h.closer(),
            _ => None,
        }
    }

    /// The account is gone (deleted): drop everything about it. The stream was closed first by
    /// whoever deleted it; dropping it here removes its subscription.
    pub fn remove(&mut self, id: Uuid) {
        self.accounts.remove(&id);
        self.attach.remove_account(id);
        self.auto.remove_account(id);
    }

    /// An entry was confirmed: remember the protection its confirmation listed, by the
    /// entry's client id.
    /// `false` when it could not be listed (a send for the symbol is in flight): the caller
    /// then does not send the entry, which the confirmation said would be protected.
    #[must_use]
    pub fn register_attach(
        &mut self,
        entry_id: String,
        pending: PendingAttach,
        auto: AutoInit,
    ) -> bool {
        if !self.attach.register(entry_id.clone(), pending) {
            log::warn!("Protection was not listed: a send for the same symbol is in flight.");
            return false;
        }
        self.auto.register(entry_id, auto);
        true
    }

    /// An entry that carries its protection on the exchange (OKX): only the listing is kept.
    /// `false` when it could not be listed; the caller does not send the entry then.
    #[must_use]
    pub fn register_okx_attach(&mut self, entry_id: String, pending: PendingAttach) -> bool {
        let listed = self.attach.register(entry_id, pending);
        if !listed {
            log::warn!("Protection was not listed: a send for the same symbol is in flight.");
        }
        listed
    }

    /// Is automatic protection still being worked on for an earlier entry of the account on
    /// `symbol` (M3)? A new entry with automatic protection is refused meanwhile: two entries
    /// would share one whole-position stop and cancel each other's orders.
    pub fn auto_active(&self, id: Uuid, symbol: &str) -> bool {
        self.auto.is_active(id, symbol)
    }

    /// The send of the (account, symbol) ended: keep or drop what was remembered for it.
    pub fn settle_attach(&mut self, id: Uuid, symbol: &str, result: &SendResult) {
        self.attach.settle(id, symbol, result);
        self.auto.settle(id, symbol, result);
    }

    /// The entry with this client id is known not to stand on the exchange (a lookup found
    /// nothing, or the user released or discarded its record): its listed protection goes.
    pub fn entry_gone(&mut self, client_id: &str) {
        self.attach.remove_entry(client_id);
        self.auto.entry_gone(client_id);
    }

    /// What the account holds on `symbol`, as far as the app knows (N3): the positions of the
    /// symbol and whether the list may be out of date.
    pub fn holdings(&self, id: Uuid, symbol: &str) -> Holdings {
        Holdings {
            stale: self.lists_stale(id),
            dirs: self
                .positions(id)
                .iter()
                .filter(|p| p.symbol.eq_ignore_ascii_case(symbol))
                .map(|p| p.dir)
                .collect(),
        }
    }

    /// The automatic jobs that are due now (never twice: a job leaves the book as it is made).
    /// `switch` says whether the account's auto-attach is on. Entries whose work ended leave the
    /// table of pending protection.
    /// `mono_ms` is a monotonic clock for the waits (see [`PollEnv::mono_ms`]).
    pub fn poll_auto(
        &mut self,
        now_ms: i64,
        mono_ms: i64,
        switch: &dyn Fn(Uuid) -> bool,
    ) -> Vec<AutoJob> {
        // The book is taken out while it looks at the positions (which live in `self`).
        let mut auto = std::mem::take(&mut self.auto);
        let held = |id: Uuid, symbol: &str| self.holdings(id, symbol);
        let env = PollEnv {
            switch,
            held: &held,
            mono_ms,
        };
        let jobs = auto.poll(now_ms, &env);
        self.auto = auto;
        self.forget_finished();
        self.watch_bare(mono_ms);
        jobs
    }

    /// The result of an automatic job.
    pub fn auto_done(&mut self, summary: AutoSummary) {
        self.auto.done(summary);
        self.forget_finished();
    }

    fn forget_finished(&mut self) {
        for entry in self.auto.take_finished() {
            self.attach.remove_entry(&entry);
        }
    }

    /// Every note of the automatic orders of the account (failures included).
    #[cfg(test)]
    pub fn auto_notes(&self, id: Uuid) -> Vec<String> {
        self.auto.notes_for(id)
    }

    /// The notes of the automatic orders that are not failures of an entry (panel lines).
    pub fn auto_info_notes(&self, id: Uuid) -> Vec<String> {
        self.auto.info_notes_for(id)
    }

    /// The entries of the account whose protection needs the user (condition 65).
    pub fn auto_failures(&self, id: Uuid) -> Vec<FailureView> {
        let mut all = self.auto.failures_for(id);
        // The new order of a replace that did not come out (f3a2) is a banner of the same kind.
        all.extend(self.replace.failure_views(id));
        all
    }

    /// The user says an entry's protection is handled: it leaves the book and no longer blocks
    /// its symbol. Only the book is touched, never an unknown result's record or lock.
    pub fn dismiss_failure(&mut self, id: Uuid, key: &str) -> Result<String, DismissError> {
        if crate::replace_flow::ReplaceFlow::owns_key(key) {
            return self.replace.dismiss(id, key).ok_or(DismissError::NotFound);
        }
        let symbol = self.auto.dismiss(id, key)?;
        self.forget_finished();
        Ok(symbol)
    }

    /// A whole-position stop was accepted for the position: the failures that left it bare are
    /// cleared and the entry's own stop is never sent.
    pub fn protection_added(&mut self, id: Uuid, symbol: &str, dir: exchange::trade::Dir) {
        self.auto.protection_added(id, symbol, dir);
        self.replace.protection_added(id, symbol, dir);
        self.forget_finished();
    }

    /// The accounts that have a protection failure the user has not been told about by a system
    /// notification, once per entry.
    pub fn take_failure_notices(&mut self) -> Vec<Uuid> {
        let mut all = self.auto.take_to_notify();
        all.extend(self.replace.take_notices());
        all
    }

    /// Symbols with protection listed for an entry that has no position yet (a resting limit
    /// entry): the panel says so, since no position row can.
    pub fn pending_without_position(&self, id: Uuid) -> Vec<String> {
        let held: Vec<String> = self
            .positions(id)
            .iter()
            .map(|p| p.symbol.clone())
            .collect();
        self.attach
            .pending_symbols(id)
            .into_iter()
            .filter(|s| !held.contains(s))
            .collect()
    }

    /// A resting limit entry with protection still owed (closing the app is warned about).
    pub fn has_resting_limit_attach(&self) -> bool {
        self.attach.has_resting_limit()
    }

    /// New risk settings were saved for the account: the same tracker takes the new limits
    /// (a lock or a peak is not reset by replacing it).
    pub fn apply_risk_settings(&mut self, id: Uuid, settings: &AccountRiskSettings) {
        let Some(rt) = self.accounts.get_mut(&id) else {
            return;
        };
        rt.meta.risk = settings.clone();
        match settings.limits() {
            Ok(limits) => {
                let r = rt.risk.set_limits(limits);
                rt.note_poisoned(r);
            }
            Err(e) => log::warn!("risk settings not applied: {e:?}"),
        }
    }

    /// New protective defaults were saved for the account: the running copy takes them.
    pub fn apply_protect_defaults(&mut self, id: Uuid, defaults: &ProtectDefaults) {
        if let Some(rt) = self.accounts.get_mut(&id) {
            rt.meta.protect = defaults.clone();
        }
    }

    /// A connection test finished. `usable`: the verdict allows trading for the environment.
    /// A usable account gets a new stream (generation + 1) after the old one is closed (a
    /// re-test of an `AuthFailed` account is how it comes back, S1-b); anything else closes the
    /// stream and starts none.
    pub fn on_tested(
        &mut self,
        meta: &AccountMeta,
        usable: bool,
        signed: Result<SignedHttp, String>,
    ) -> Task<Message> {
        let start = if usable { signed.ok() } else { None };
        let proxy = self.proxy_reader();
        if start.is_none() && !self.accounts.contains_key(&meta.id) {
            return Task::none();
        }
        let rt = self
            .accounts
            .entry(meta.id)
            .or_insert_with(|| AccountRuntime::new(meta, now_ms));
        // The saved settings may have changed since the runtime was made.
        rt.meta = meta.clone();
        rt.needs_retest = start.is_none();
        if start.is_none() {
            rt.state = is_spot(meta).then_some(StreamState::Unavailable);
        }
        Self::replace_stream(meta.id, rt, start, proxy)
    }

    /// Close the running stream (if any), then start `then`.
    fn replace_stream(
        id: Uuid,
        rt: &mut AccountRuntime,
        then: Option<SignedHttp>,
        proxy: Arc<dyn Fn() -> ProxyState + Send + Sync>,
    ) -> Task<Message> {
        match &mut rt.phase {
            // Already waiting for the old stream: only what comes next changes.
            Phase::Closing { then: waiting } => {
                *waiting = then;
                Task::none()
            }
            Phase::Running(handle) => {
                let closer = handle.closer();
                rt.phase = Phase::Closing { then };
                match closer {
                    Some(closer) => Task::perform(
                        async move {
                            close_within(SHUTDOWN_TIMEOUT, async move { closer.shutdown().await })
                                .await;
                            id
                        },
                        Message::Closed,
                    ),
                    // The stream never started: there is no listenKey to close.
                    None => {
                        Self::finish_close(rt, proxy);
                        Task::none()
                    }
                }
            }
            Phase::Idle => {
                rt.phase = Phase::Closing { then };
                Self::finish_close(rt, proxy);
                Task::none()
            }
        }
    }

    fn finish_close(rt: &mut AccountRuntime, proxy: Arc<dyn Fn() -> ProxyState + Send + Sync>) {
        let Phase::Closing { then } = std::mem::replace(&mut rt.phase, Phase::Idle) else {
            return;
        };
        rt.forget_lists();
        let Some(signed) = then else {
            return;
        };
        if is_spot(&rt.meta) {
            // No verified private stream for spot: trading stays off, and it says so.
            rt.state = Some(StreamState::Unavailable);
            return;
        }
        rt.generation = rt.generation.saturating_add(1);
        let key = StreamKey {
            account: rt.account(),
            venue: rt.meta.venue,
            env: rt.meta.env,
            product: rt.meta.product,
            generation: rt.generation,
        };
        let meta = rt.meta.clone();
        rt.feed = StreamEquityFeed::new(&key);
        rt.book_trusted = false;
        rt.recheck_auto = true;
        rt.state = Some(StreamState::Reconnecting);
        rt.needs_retest = false;
        rt.phase = Phase::Running(StreamHandle {
            ctx: StreamContext {
                key,
                signed,
                current_proxy: proxy,
                // Read from the Keychain on every connect, held only while used.
                load_credentials: Arc::new(move || {
                    load_credentials(&meta).map_err(|_| CredentialsUnavailable)
                }),
            },
            closer: Arc::new(Mutex::new(None)),
        });
    }

    pub fn update(&mut self, message: Message, now_ms: i64) -> (Task<Message>, Effects) {
        let mut effects = Effects::default();
        let task = match message {
            Message::Booted(report) => {
                self.on_booted(report);
                Task::none()
            }
            Message::Stream(key, output) => {
                self.on_output(key, output, now_ms, &mut effects);
                Task::none()
            }
            Message::Closed(id) => {
                let proxy = self.proxy_reader();
                if let Some(rt) = self.accounts.get_mut(&id) {
                    Self::finish_close(rt, proxy);
                }
                Task::none()
            }
            Message::EquityTick => {
                self.refresh_flat_books(now_ms);
                self.poll_equity()
            }
            Message::EquityPolled(id, observation) => {
                if let Some(rt) = self.accounts.get_mut(&id) {
                    rt.equity_inflight = false;
                    if let Some(obs) = observation {
                        let r = rt.risk.observe(obs);
                        rt.note_poisoned(r);
                    }
                }
                Task::none()
            }
        };
        (task, effects)
    }

    /// Apply one output of one stream, in this one call: the snapshot first (lists, then the
    /// risk tracker), the events that follow it on top.
    fn on_output(&mut self, key: StreamKey, output: StreamOutput, now_ms: i64, fx: &mut Effects) {
        let id = self
            .accounts
            .iter()
            .find(|(_, rt)| rt.account() == key.account)
            .map(|(id, _)| *id);
        let Some(id) = id else {
            return;
        };
        let Some(rt) = self.accounts.get_mut(&id) else {
            return;
        };
        // An output of a replaced stream (an older generation) says nothing about the new one.
        if rt.current_key() != Some(key) {
            return;
        }
        match output {
            StreamOutput::State(state) => {
                if state != StreamState::Connected {
                    rt.book_trusted = false;
                }
                if matches!(
                    state,
                    StreamState::Reconnecting | StreamState::AuthFailed | StreamState::Unavailable
                ) {
                    rt.recheck_auto = true;
                }
                rt.state = Some(state);
                if state == StreamState::Connected
                    && rt.book_trusted
                    && std::mem::take(&mut rt.recheck_auto)
                {
                    Self::check_after_reconnect(rt, &mut self.auto, id);
                }
            }
            StreamOutput::InvalidateSession(_) => {
                rt.needs_retest = true;
                fx.invalidate.push(id);
            }
            StreamOutput::Snapshot(snapshot) => {
                // The tracker takes the snapshot itself: the epoch cannot be passed as a number.
                rt.apply_snapshot(&snapshot.orders, &snapshot.positions, |risk, n| {
                    risk.snapshot_applied(&snapshot, now_ms, n)
                });
            }
            StreamOutput::Event(event) => Self::apply_event(rt, &mut self.auto, id, &event, now_ms),
        }
    }

    /// The stream is connected again and the snapshot and held-back events are applied: entries that were open when the
    /// connection dropped may have filled without any event reaching the app (M4-4).
    fn check_after_reconnect(rt: &AccountRuntime, auto: &mut AutoBook, id: Uuid) {
        let resting = |symbol: &str, dir: Dir, client: &str| {
            rt.orders.iter().any(|o| {
                o.symbol.eq_ignore_ascii_case(symbol)
                    && matches!(o.kind, exchange::trade::OpenKind::Plain)
                    && !o.reduce_only
                    && o.side == Some(dir.opening_side())
                    // Matched by client id when the row has one (d5); a row without one keeps
                    // the symbol and side match.
                    && o.client_id.as_deref().is_none_or(|c| c == client)
            })
        };
        let held = |symbol: &str| {
            rt.positions
                .iter()
                .filter(|p| p.symbol.eq_ignore_ascii_case(symbol))
                .map(|p| p.dir)
                .collect::<Vec<_>>()
        };
        auto.after_reconnect(
            id,
            &ReconnectView {
                resting: &resting,
                held: &held,
            },
        );
    }

    fn apply_event(
        rt: &mut AccountRuntime,
        auto: &mut AutoBook,
        id: Uuid,
        event: &PrivateEvent,
        now_ms: i64,
    ) {
        if let Some(obs) = rt.feed.on_event(event, now_ms) {
            let r = rt.risk.observe(obs);
            rt.note_poisoned(r);
        }
        match event {
            PrivateEvent::Fill(fill) => {
                // A record of what happened; it never changes the held positions.
                let new = rt.note_fill(&fill.symbol, fill.trade_id.as_deref());
                // A fill with no trade id cannot be told from a repeat, and one with no client
                // id cannot be tied to an entry: neither is counted (no guessing).
                match (new, fill.trade_id.is_some(), fill.client_id.as_deref()) {
                    (true, true, Some(client)) => {
                        let verdict = auto.on_fill(id, client, fill.side, fill.qty, fill.price);
                        if verdict == FillVerdict::WrongSide {
                            log::info!("A fill that does not add to the entry was not counted.");
                        }
                    }
                    (true, _, None) => {
                        log::info!("A fill without a client id was not matched to an entry.");
                    }
                    // Tied to an entry by its client id, but with no trade id it cannot be told from
                    // a repeat: not counted, so the wait for fills may end as "not seen". The
                    // client id is not written to the log.
                    (true, false, Some(_)) => {
                        log::info!("A fill with a client id but no trade id was not counted.");
                    }
                    _ => {}
                }
            }
            PrivateEvent::Position(update) => {
                rt.apply_position_event(
                    &update.symbol,
                    update.position_side,
                    update.qty,
                    update.entry_price,
                );
                let notionals = position_notionals(&rt.positions);
                let r = rt.risk.set_positions(now_ms, notionals);
                rt.note_poisoned(r);
            }
            // A conditional order changed (placed, triggered, cancelled, finished): the order
            // list is out of date. Conservative on purpose: nothing is patched in place (a
            // `Triggered` or `Finished` status does not say what remains open); the list is
            // marked out of date and the stream reads it again (see the stream machine).
            PrivateEvent::Order(update) if update.category == OrderCategory::Algo => {
                rt.orders_dirty = true;
                auto.on_order(id, update);
            }
            PrivateEvent::Order(update) => auto.on_order(id, update),
            // The stream read everything again because it may have missed events.
            PrivateEvent::ResyncRequired(_) => rt.recheck_auto = true,
            _ => {}
        }
    }

    /// The shortest wait between equity reads of the connected accounts, `None` when none is
    /// connected (nothing to poll).
    fn equity_poll_interval(&self) -> Option<Duration> {
        self.accounts
            .values()
            .filter(|rt| rt.state == Some(StreamState::Connected) && !rt.needs_retest)
            .map(|rt| {
                let age = Duration::from_secs(u64::from(rt.meta.risk.max_equity_age_secs));
                (age / 3).clamp(MIN_POLL_INTERVAL, MAX_POLL_INTERVAL)
            })
            .min()
    }

    /// A flat book goes stale with time although nothing changed, and a stale book makes every
    /// entry under a per-symbol cap `PositionUnknown`. While the stream is connected, has
    /// applied its snapshot and stayed connected, the position events keep the list current,
    /// so an empty list is still true and is stamped again. A book with a position, or one
    /// from before the first snapshot of a stream, is never refreshed here.
    fn refresh_flat_books(&mut self, now_ms: i64) {
        for rt in self.accounts.values_mut() {
            if rt.state == Some(StreamState::Connected)
                && rt.book_trusted
                && !rt.needs_retest
                && rt.positions.is_empty()
            {
                let r = rt.risk.set_positions(now_ms, Vec::new());
                rt.note_poisoned(r);
            }
        }
    }

    fn poll_equity(&mut self) -> Task<Message> {
        let proxy = self.proxy_reader();
        let tasks: Vec<_> = self
            .accounts
            .iter_mut()
            .filter(|(_, rt)| {
                rt.state == Some(StreamState::Connected) && !rt.needs_retest && !rt.equity_inflight
            })
            .filter_map(|(id, rt)| {
                let Phase::Running(handle) = &rt.phase else {
                    return None;
                };
                let signed = handle.ctx.signed.clone();
                let meta = rt.meta.clone();
                let id = *id;
                let proxy = proxy.clone();
                rt.equity_inflight = true;
                Some(Task::perform(
                    async move { (id, read_equity(signed, meta, proxy).await) },
                    |(id, obs)| Message::EquityPolled(id, obs),
                ))
            })
            .collect();
        Task::batch(tasks)
    }

    /// The streams that should be running, in a fixed order (by account id) so the set of
    /// subscriptions, and with it each identity, does not shuffle between frames. Nothing
    /// starts before the saved unresolved sends are loaded and locked.
    fn runs(&self) -> Vec<PrivateRun> {
        if self.boot.is_none() {
            return Vec::new();
        }
        self.accounts
            .values()
            .filter_map(|rt| match &rt.phase {
                Phase::Running(h) => Some(PrivateRun {
                    ctx: h.ctx.clone(),
                    closer: h.closer.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let mut subs: Vec<Subscription<Message>> = self
            .runs()
            .into_iter()
            .map(|run| Subscription::run_with(run, run_private))
            .collect();
        if let Some(every) = self.equity_poll_interval() {
            subs.push(iced::time::every(every).map(|_| Message::EquityTick));
        }
        Subscription::batch(subs)
    }

    // ── For the views ───────────────────────────────────────────────────

    /// One line for the account row of the settings modal.
    pub fn account_line(&self, id: Uuid) -> Option<String> {
        let rt = self.accounts.get(&id)?;
        let state = match rt.state {
            Some(StreamState::Connected) => "connected",
            Some(StreamState::Reconnecting) => "reconnecting",
            Some(StreamState::Resyncing) => "resyncing",
            Some(StreamState::AuthFailed) => "credentials rejected",
            Some(StreamState::Unavailable) => "unavailable",
            None => "not started",
        };
        Some(format!(
            "Private stream: {state} · {} open orders · {} positions · {} fills seen",
            rt.orders.len(),
            rt.positions.len(),
            rt.fills_seen
        ))
    }
}

/// One signed read of the account's equity. Credentials are loaded for this read only. Any
/// failure is `None`: the tracker then sees the reading as old and refuses entries under an
/// equity limit; nothing is closed because of it.
async fn read_equity(
    signed: SignedHttp,
    meta: AccountMeta,
    proxy: Arc<dyn Fn() -> ProxyState + Send + Sync>,
) -> Option<EquityObservation> {
    check_proxy_snapshot(signed.proxy_state(), &proxy()).ok()?;
    let loaded = {
        let meta = meta.clone();
        tokio::task::spawn_blocking(move || load_credentials(&meta))
            .await
            .ok()?
            .ok()?
    };
    let client = TradeClient::new(
        signed,
        meta.venue,
        meta.env,
        meta.product,
        account_key(meta.id),
        loaded,
        proxy,
    );
    // Stores the measured offset in the client; a failure keeps offset zero.
    let _ = client.sync_clock().await;
    client.equity().await.ok()
}

// ── Standing alerts ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub severity: Severity,
    pub text: String,
}

fn alert(severity: Severity, text: impl Into<String>) -> Alert {
    Alert {
        severity,
        text: text.into(),
    }
}

fn no_limits_set(r: &AccountRiskSettings) -> bool {
    r.max_order_notional.is_none()
        && r.max_symbol_notional.is_none()
        && r.daily_loss_limit.is_none()
        && r.drawdown_bps.is_none()
}

/// Every alert that must stay on screen (a toast fades). Shown in the main window and in
/// popouts. A pure function of the state, so a table of states can be tested. `saved` is the
/// saved account list: names come from it (L1), not from the accounts tested this run.
pub fn standing_alerts(
    state: &TradingState,
    session: &TradeSession,
    saved: &[AccountMeta],
) -> Vec<Alert> {
    let mut out = Vec::new();

    if let Some(boot) = &state.boot {
        if boot.journal_unreadable {
            out.push(alert(
                Severity::Error,
                "Saved unresolved orders could not be read: all new entries are blocked until \
                 the app restarts.",
            ));
        }
        for rec in &boot.records {
            let who = name_of(saved, rec.account).unwrap_or_else(|| "an account".to_string());
            out.push(match (rec.error, rec.phase) {
                (_, RecordPhase::Discarded) => alert(
                    Severity::Error,
                    format!(
                        "A saved record of {who} ({}) with an invalid id was discarded: all new \
                         entries of this account stay blocked until the app restarts.",
                        rec.symbol
                    ),
                ),
                (Some(_), _) => alert(
                    Severity::Error,
                    format!(
                        "A saved unresolved order of {who} ({}) has an invalid id and was kept: \
                         all new entries of this account are blocked until the app restarts. \
                         You can discard the record in the Order ticket.",
                        rec.symbol
                    ),
                ),
                (None, RecordPhase::AwaitingTest) => alert(
                    Severity::Warning,
                    format!(
                        "An unresolved order of {who} ({}) was restored: this symbol stays \
                         locked for new entries. It is looked up after you test the account.",
                        rec.symbol
                    ),
                ),
                (None, RecordPhase::Looking) => alert(
                    Severity::Warning,
                    format!(
                        "An unresolved order of {who} ({}) is being looked up: this symbol \
                         stays locked for new entries.",
                        rec.symbol
                    ),
                ),
                (None, RecordPhase::StillUnknown) => alert(
                    Severity::Error,
                    format!(
                        "An unresolved order of {who} ({}) is still unknown after the lookups: \
                         this symbol stays locked. Check the exchange, then release it in the \
                         Order ticket or restart the app.",
                        rec.symbol
                    ),
                ),
            });
        }
    }
    if state.journal_failed {
        out.push(alert(
            Severity::Error,
            "The order journal is damaged: after a restart, unknown order results will not be \
             remembered.",
        ));
    }

    for ((id, symbol), pair) in &state.pairs {
        if *pair == PairState::Unresolved {
            let who = saved
                .iter()
                .find(|m| m.id == *id)
                .or_else(|| state.accounts.get(id).map(|rt| &rt.meta))
                .map_or_else(|| "an account".to_string(), |m| display_name(saved, m));
            // Without a saved record there is nothing to release in the Order ticket (a task
            // that ended with no answer leaves none): only a restart lifts the block.
            let account = account_key(*id);
            let has_record = state.boot.as_ref().is_some_and(|b| {
                b.records
                    .iter()
                    .any(|r| r.account == account && r.symbol == *symbol)
            });
            let how = if has_record {
                "until you release it by hand (Order ticket) or the app restarts"
            } else {
                "until the app restarts"
            };
            out.push(alert(
                Severity::Error,
                format!(
                    "{who}: the result of an order on {symbol} is unknown. New entries on this \
                     symbol stay blocked {how}; check the exchange."
                ),
            ));
        }
    }

    for rt in state.accounts.values() {
        let who_owned = display_name(saved, &rt.meta);
        let who = who_owned.as_str();
        if rt.needs_retest || (session.get(rt.meta.id).is_none() && rt.state.is_some()) {
            out.push(alert(
                Severity::Warning,
                format!("{who}: not tested in this session. Trading is off until you test it."),
            ));
        }
        // L2: after a proxy change the stream only shows "reconnecting"; say why.
        if state.proxy_changed_for(rt) {
            out.push(alert(
                Severity::Warning,
                format!(
                    "{who}: the proxy setting changed since this account was tested. Its stream \
                     and all orders are refused until you test the account again."
                ),
            ));
        }
        match rt.state {
            Some(StreamState::AuthFailed) => out.push(alert(
                Severity::Error,
                format!(
                    "{who}: credentials were rejected. All orders (including cancels and closes) \
                     are paused. Test the account again, or use the exchange website."
                ),
            )),
            Some(StreamState::Reconnecting) => out.push(alert(
                Severity::Warning,
                format!(
                    "{who}: private stream is reconnecting. Order and position lists may be stale."
                ),
            )),
            Some(StreamState::Resyncing) => out.push(alert(
                Severity::Warning,
                format!(
                    "{who}: private stream is resyncing. Order and position lists may be stale."
                ),
            )),
            Some(StreamState::Unavailable) if is_spot(&rt.meta) => out.push(alert(
                Severity::Info,
                format!("{who}: spot trading is disabled (not verified on the testnet yet)."),
            )),
            Some(StreamState::Unavailable) => out.push(alert(
                Severity::Error,
                format!(
                    "{who}: private stream is unavailable. New entries are blocked and lists \
                     may be stale."
                ),
            )),
            Some(StreamState::Connected) | None => {}
        }

        if rt.risk_unavailable {
            out.push(alert(
                Severity::Error,
                format!("{who}: risk state is unavailable. New entries are disabled."),
            ));
        } else {
            match rt.risk.is_tripped() {
                Ok(Some(reason)) => out.push(alert(
                    Severity::Error,
                    format!(
                        "{who}: risk lock ({}). New entries are refused.",
                        reason.as_str()
                    ),
                )),
                Ok(None) => {}
                Err(_) => out.push(alert(
                    Severity::Error,
                    format!("{who}: risk state is unavailable. New entries are disabled."),
                )),
            }
            if rt.risk.with_tracker(|t| t.settings_invalid()) == Ok(true) {
                out.push(alert(
                    Severity::Error,
                    format!("{who}: risk settings are invalid. New entries are refused."),
                ));
            }
            if rt.has_equity_limits()
                && rt.state == Some(StreamState::Connected)
                && rt.risk.freshness() != Ok(EquityFreshness::Fresh)
            {
                out.push(alert(
                    Severity::Warning,
                    format!("{who}: equity reading is missing or old. New entries are refused."),
                ));
            }
        }
        if no_limits_set(&rt.meta.risk) {
            out.push(alert(
                Severity::Warning,
                format!("{who}: no risk limits set."),
            ));
        }
    }

    let binance_both = |env: AccountEnv| {
        let has = |p: AccountProduct| {
            state.accounts.values().any(|rt| {
                rt.meta.venue == AccountVenue::Binance && rt.meta.env == env && rt.meta.product == p
            })
        };
        has(AccountProduct::Spot) && has(AccountProduct::UsdM)
    };
    if binance_both(AccountEnv::Live) || binance_both(AccountEnv::Testnet) {
        out.push(alert(
            Severity::Info,
            "Binance spot and USDⓈ-M accounts of one environment: if they use the same API key \
             they share one exchange account, and risk limits are counted separately.",
        ));
    }
    if !state.accounts.is_empty() {
        out.push(alert(
            Severity::Info,
            "Local risk limits only work while the app is running. The equity peak is kept in \
             memory only, and a transfer can trip a lock.",
        ));
    }
    out
}

/// The alerts as a stack of banners to put above a window's content (`None` when there are
/// none).
pub fn alerts_view<'a, M: 'a>(alerts: Vec<Alert>) -> Option<Element<'a, M>> {
    if alerts.is_empty() {
        return None;
    }
    let mut col = iced::widget::column![].spacing(2);
    for a in alerts {
        let kind = match a.severity {
            Severity::Info => BannerKind::Normal,
            Severity::Warning => BannerKind::Warning,
            Severity::Error => BannerKind::Error,
        };
        col = col.push(banner(a.text, kind));
    }
    Some(col.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use data::config::trade_gate::TradeSession;
    use exchange::account::{CheckReport, PositionMode, Verdict};
    use exchange::trade::stream::{Fill, InvalidReason, PositionUpdate, TradeAction, allows};
    use exchange::trade::{Dir, Exposure, RiskHook, RiskRefusal, RiskView};
    use std::cell::RefCell;
    use std::collections::hash_map::DefaultHasher;

    const SECRET_CID: &str = "SECRETCLIENTID123";

    fn meta(alias: &str, venue: AccountVenue, product: AccountProduct) -> AccountMeta {
        AccountMeta::new(
            alias.into(),
            venue,
            AccountEnv::Testnet,
            product,
            PositionMode::OneWay,
        )
    }

    fn signed() -> Result<SignedHttp, String> {
        SignedHttp::build(ProxyState::Direct).map_err(|e| e.to_string())
    }

    fn booted() -> TradingState {
        let mut t = TradingState::new(ProxyState::Direct);
        t.on_booted(BootReport {
            journal_unreadable: false,
            records: Vec::new(),
        });
        t
    }

    /// A state with one tested futures account whose stream is "running" (never polled).
    fn running() -> (TradingState, AccountMeta, StreamKey) {
        let mut t = booted();
        let m = meta("main", AccountVenue::Binance, AccountProduct::UsdM);
        let _ = t.on_tested(&m, true, signed());
        let key = t.accounts[&m.id].current_key().expect("a running stream");
        (t, m, key)
    }

    fn session_with(m: &AccountMeta) -> TradeSession {
        let mut s = TradeSession::default();
        s.record(
            m,
            &CheckReport {
                verdict: Verdict::Usable,
                warnings: vec![],
                actual_mode: None,
                can_trade: Some(true),
            },
        );
        s
    }

    fn texts(alerts: &[Alert]) -> String {
        alerts
            .iter()
            .map(|a| a.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn fill(trade_id: Option<&str>) -> Box<PrivateEvent> {
        Box::new(PrivateEvent::Fill(Fill {
            symbol: "BTCUSDT".into(),
            client_id: Some(SECRET_CID.into()),
            order_id: None,
            trade_id: trade_id.map(str::to_string),
            side: None,
            qty: Decimal::from_int(1),
            price: Decimal::from_int(100),
            fee: None,
            fee_asset: None,
            realized_pnl: None,
            time_ms: None,
        }))
    }

    fn position_event(symbol: &str, side: PositionSide, qty: i64) -> Box<PrivateEvent> {
        Box::new(PrivateEvent::Position(PositionUpdate {
            symbol: symbol.into(),
            position_side: side,
            qty: Decimal::from_int(qty),
            entry_price: None,
        }))
    }

    fn held(symbol: &str, qty: i64) -> Position {
        Position {
            symbol: symbol.into(),
            dir: Dir::Long,
            qty: Decimal::from_int(qty),
            hedge_side: false,
            margin: None,
            leverage: None,
            entry_price: None,
        }
    }

    // ── Startup order ───────────────────────────────────────────────────

    fn record(account: u8, symbol: &str, client_id: &str) -> UnknownRecord {
        UnknownRecord {
            account: AccountKey::from_bytes([account; 16]),
            symbol: symbol.into(),
            client_id: client_id.into(),
            kind: "x".into(),
            recorded_at_ms: 0,
        }
    }

    #[test]
    fn a_failed_load_blocks_every_entry_and_locks_nothing() {
        let calls = RefCell::new(Vec::new());
        let report = plan_boot(
            Err(LoadFailed),
            || calls.borrow_mut().push("block_all"),
            |_| {
                calls.borrow_mut().push("lock");
                Ok(())
            },
        );
        assert!(report.journal_unreadable);
        assert!(report.records.is_empty());
        assert_eq!(*calls.borrow(), ["block_all"]);
    }

    #[test]
    fn every_loaded_record_is_locked_and_a_failure_is_kept_never_swallowed() {
        let calls = RefCell::new(Vec::new());
        let report = plan_boot(
            Ok(vec![
                record(1, "BTCUSDT", "good"),
                record(2, "ETHUSDT", "bad"),
                record(3, "SOLUSDT", "good"),
            ]),
            || calls.borrow_mut().push("block_all".to_string()),
            |r| {
                calls.borrow_mut().push(r.symbol.clone());
                if r.client_id == "bad" {
                    Err(RestoreError::BadClientId)
                } else {
                    Ok(())
                }
            },
        );
        assert!(!report.journal_unreadable);
        // Every record was tried, in order, and nothing blocked everything.
        assert_eq!(*calls.borrow(), ["BTCUSDT", "ETHUSDT", "SOLUSDT"]);
        // All three are still listed; the failure carries its reason.
        let errors: Vec<_> = report.records.iter().map(|r| r.error).collect();
        assert_eq!(errors, [None, Some(RestoreError::BadClientId), None]);
    }

    #[test]
    fn a_boot_report_shows_no_client_id() {
        let report = plan_boot(
            Ok(vec![record(1, "BTCUSDT", SECRET_CID)]),
            || {},
            |_| Ok(()),
        );
        assert!(!format!("{report:?} {:?}", Message::Booted(report.clone())).contains(SECRET_CID));
    }

    #[test]
    fn nothing_starts_before_the_boot_report_is_applied() {
        let mut t = TradingState::new(ProxyState::Direct);
        let m = meta("main", AccountVenue::Okx, AccountProduct::UsdM);
        let _ = t.on_tested(&m, true, signed());
        assert!(t.runs().is_empty());
        t.on_booted(BootReport {
            journal_unreadable: false,
            records: Vec::new(),
        });
        assert_eq!(t.runs().len(), 1);
    }

    // ── Streams ─────────────────────────────────────────────────────────

    #[test]
    fn a_stream_is_only_made_for_a_usable_test_and_only_one_per_account() {
        let mut t = booted();
        let m = meta("main", AccountVenue::Okx, AccountProduct::UsdM);
        // A failed or unusable test of an account without a stream makes nothing.
        let _ = t.on_tested(&m, false, signed());
        assert!(t.runs().is_empty());
        // No usable client: no stream either.
        let _ = t.on_tested(&m, true, Err("proxy".into()));
        assert!(t.runs().is_empty());
        let _ = t.on_tested(&m, true, signed());
        assert_eq!(t.runs().len(), 1);
        assert_eq!(t.runs()[0].ctx.key.generation, 1);
    }

    #[test]
    fn spot_gets_no_stream_and_says_it_is_disabled() {
        let mut t = booted();
        let m = meta("spot", AccountVenue::Binance, AccountProduct::Spot);
        let _ = t.on_tested(&m, true, signed());
        assert!(t.runs().is_empty());
        assert_eq!(t.accounts[&m.id].state, Some(StreamState::Unavailable));
        let alerts = standing_alerts(&t, &session_with(&m), &[]);
        assert!(texts(&alerts).contains("spot trading is disabled (not verified"));
    }

    #[test]
    fn streams_are_listed_in_a_fixed_order() {
        let mut t = booted();
        let ms: Vec<_> = (0..4)
            .map(|i| meta(&format!("a{i}"), AccountVenue::Okx, AccountProduct::UsdM))
            .collect();
        for m in ms.iter().rev() {
            let _ = t.on_tested(m, true, signed());
        }
        let mut ids: Vec<_> = ms.iter().map(|m| account_key(m.id).to_bytes()).collect();
        ids.sort();
        let order: Vec<_> = t
            .runs()
            .iter()
            .map(|r| r.ctx.key.account.to_bytes())
            .collect();
        assert_eq!(order, ids);
    }

    fn hash_of(run: &PrivateRun) -> u64 {
        let mut h = DefaultHasher::new();
        run.hash(&mut h);
        h.finish()
    }

    #[test]
    fn the_subscription_identity_is_the_stream_key_and_nothing_else() {
        let (t, m, _) = running();
        let first = t.runs().remove(0);
        // Same key, everything else different (another closer slot, another credential loader,
        // another proxy reader): the same identity.
        let mut other = first.clone();
        other.closer = Arc::new(Mutex::new(None));
        other.ctx.load_credentials = Arc::new(|| Err(CredentialsUnavailable));
        other.ctx.current_proxy = Arc::new(|| ProxyState::Broken);
        assert_eq!(hash_of(&first), hash_of(&other));
        // Another generation: a new identity (the old stream is replaced).
        let mut next = first.clone();
        next.ctx.key.generation += 1;
        assert_ne!(hash_of(&first), hash_of(&next));
        // Another account: a new identity.
        let mut t2 = booted();
        let m2 = meta("two", AccountVenue::Binance, AccountProduct::UsdM);
        let _ = t2.on_tested(&m2, true, signed());
        assert_ne!(m.id, m2.id);
        assert_ne!(hash_of(&first), hash_of(&t2.runs().remove(0)));
    }

    #[test]
    fn a_terminal_state_is_kept_and_the_stream_is_not_rebuilt_by_itself() {
        let (mut t, m, key) = running();
        let (_, fx) = t.update(
            Message::Stream(key, StreamOutput::State(StreamState::AuthFailed)),
            0,
        );
        assert_eq!(fx, Effects::default());
        let (_, fx) = t.update(
            Message::Stream(
                key,
                StreamOutput::InvalidateSession(InvalidReason::StreamAuthFailed),
            ),
            0,
        );
        // The session verdict is void, the entry (and its terminal state) stays, and the stream
        // is the same one: no automatic rebuild.
        assert_eq!(fx.invalidate, vec![m.id]);
        let rt = &t.accounts[&m.id];
        assert_eq!(rt.state, Some(StreamState::AuthFailed));
        assert_eq!(rt.generation, 1);
        assert_eq!(rt.current_key(), Some(key));
        let alerts = standing_alerts(&t, &TradeSession::default(), &[]);
        let all = texts(&alerts);
        assert!(all.contains("credentials were rejected"));
        assert!(all.contains("cancels and closes"));
        assert!(all.contains("not tested in this session"));
    }

    #[test]
    fn testing_again_after_auth_failed_makes_a_new_stream_and_closing_is_allowed_again() {
        let (mut t, m, key) = running();
        let _ = t.update(
            Message::Stream(key, StreamOutput::State(StreamState::AuthFailed)),
            0,
        );
        let _ = t.update(
            Message::Stream(
                key,
                StreamOutput::InvalidateSession(InvalidReason::StreamAuthFailed),
            ),
            0,
        );
        // The send gate refuses everything while AuthFailed is the stream's state (condition 57);
        // that gate cannot be driven from here, so this checks what feeds it: the state.
        let before = t.accounts[&m.id].state.unwrap();
        assert_eq!(before, StreamState::AuthFailed);
        // Testing again (usable) closes the old stream and starts generation 2.
        let _ = t.on_tested(&m, true, signed());
        let rt = &t.accounts[&m.id];
        assert_eq!(rt.generation, 2);
        let new_key = rt.current_key().expect("a new stream");
        assert_eq!(new_key.generation, 2);
        assert_ne!(rt.state, Some(StreamState::AuthFailed));
        assert!(!rt.needs_retest);
        assert!(allows(rt.state.unwrap(), TradeAction::Close));
        // The old stream's late outputs do not touch the new one.
        let (_, fx) = t.update(
            Message::Stream(
                key,
                StreamOutput::InvalidateSession(InvalidReason::StreamAuthFailed),
            ),
            0,
        );
        assert!(fx.invalidate.is_empty());
        let _ = t.update(
            Message::Stream(key, StreamOutput::State(StreamState::AuthFailed)),
            0,
        );
        assert_ne!(t.accounts[&m.id].state, Some(StreamState::AuthFailed));
        // The new stream's outputs do.
        let _ = t.update(
            Message::Stream(new_key, StreamOutput::State(StreamState::Connected)),
            0,
        );
        assert_eq!(t.accounts[&m.id].state, Some(StreamState::Connected));
    }

    #[test]
    fn a_failed_test_closes_the_stream_and_keeps_the_account_listed() {
        let (mut t, m, _) = running();
        let _ = t.on_tested(&m, false, signed());
        assert!(t.runs().is_empty());
        assert!(t.accounts[&m.id].needs_retest);
        t.remove(m.id);
        assert!(t.accounts.is_empty());
    }

    #[test]
    fn a_risk_tracker_survives_a_new_stream() {
        let (mut t, m, _) = running();
        // Poison nothing; mark the tracker with a limit and check it is the same one after.
        let limits = AccountRiskSettings {
            max_order_notional: Some("5".into()),
            ..AccountRiskSettings::default()
        };
        t.apply_risk_settings(m.id, &limits);
        let _ = t.on_tested(&m, true, signed());
        let rt = &t.accounts[&m.id];
        let cap = rt
            .risk
            .with_tracker(|tr| tr.limits().max_order_notional)
            .unwrap();
        assert_eq!(cap.map(|d| d.to_plain_string()), Some("5".to_string()));
    }

    // ── Snapshot and events ─────────────────────────────────────────────

    #[test]
    fn a_snapshot_replaces_the_lists_and_values_positions_from_itself_only() {
        let (mut t, m, _) = running();
        let rt = t.accounts.get_mut(&m.id).unwrap();
        let given = RefCell::new(Vec::<Vec<(String, Decimal)>>::new());
        let apply = |risk: &SharedRisk, n: Vec<(String, Decimal)>| {
            given.borrow_mut().push(n.clone());
            risk.set_positions(0, n)
        };
        rt.apply_snapshot(&[], &[held("BTCUSDT", 2)], apply);
        assert_eq!(rt.positions.len(), 1);
        rt.apply_snapshot(&[], &[], apply);
        assert!(rt.positions.is_empty());
        let given = given.into_inner();
        // The first book says "a position I cannot value"; the second, built from a flat
        // snapshot, carries nothing over from the first.
        assert_eq!(given[0].len(), 1);
        assert_eq!(given[0][0].0, UNPRICED);
        assert!(given[1].is_empty());
    }

    #[test]
    fn a_held_position_that_cannot_be_valued_refuses_entries_under_a_symbol_cap() {
        let (mut t, m, _) = running();
        t.apply_risk_settings(
            m.id,
            &AccountRiskSettings {
                max_symbol_notional: Some("1000".into()),
                ..AccountRiskSettings::default()
            },
        );
        let account = account_key(m.id);
        let rt = t.accounts.get_mut(&m.id).unwrap();
        let view = RiskView {
            account,
            symbol: "BTCUSDT",
            exposure: Exposure::Adds {
                notional: Some(Decimal::from_int(10)),
            },
        };
        let now = now_ms();
        rt.apply_snapshot(&[], &[], |r, n| r.set_positions(now, n));
        assert_eq!(rt.risk.check(&view), Ok(()));
        rt.apply_snapshot(&[], &[held("BTCUSDT", 1)], |r, n| r.set_positions(now, n));
        assert_eq!(rt.risk.check(&view), Err(RiskRefusal::PositionUnknown));
    }

    #[test]
    fn events_apply_on_top_of_the_snapshot_and_a_fill_never_adds_to_a_position() {
        let (mut t, m, key) = running();
        let rt = t.accounts.get_mut(&m.id).unwrap();
        rt.apply_snapshot(&[], &[held("BTCUSDT", 2)], |r, n| r.set_positions(0, n));
        assert_eq!(rt.positions[0].qty, Decimal::from_int(2));
        // The position event after the snapshot wins; a signed one-way size is held absolute.
        let _ = t.update(
            Message::Stream(
                key,
                StreamOutput::Event(position_event("BTCUSDT", PositionSide::Both, -5)),
            ),
            1,
        );
        let rt = &t.accounts[&m.id];
        assert_eq!(rt.positions.len(), 1);
        assert_eq!(rt.positions[0].qty, Decimal::from_int(5));
        // Fills, even many of them, change no position.
        for id in ["t1", "t2", "t3"] {
            let _ = t.update(Message::Stream(key, StreamOutput::Event(fill(Some(id)))), 2);
        }
        let rt = &t.accounts[&m.id];
        assert_eq!(rt.positions.len(), 1);
        assert_eq!(rt.positions[0].qty, Decimal::from_int(5));
        assert_eq!(rt.fills_seen, 3);
        // A flat event removes it.
        let _ = t.update(
            Message::Stream(
                key,
                StreamOutput::Event(position_event("BTCUSDT", PositionSide::Both, 0)),
            ),
            3,
        );
        assert!(t.accounts[&m.id].positions.is_empty());
    }

    #[test]
    fn a_fill_is_counted_once_per_trade_id() {
        let (mut t, m, key) = running();
        for _ in 0..3 {
            let _ = t.update(
                Message::Stream(key, StreamOutput::Event(fill(Some("same")))),
                0,
            );
        }
        assert_eq!(t.accounts[&m.id].fills_seen, 1);
        // Without an id nothing can be told apart: each one counts.
        for _ in 0..2 {
            let _ = t.update(Message::Stream(key, StreamOutput::Event(fill(None))), 0);
        }
        assert_eq!(t.accounts[&m.id].fills_seen, 3);
    }

    #[test]
    fn the_remembered_trade_ids_are_bounded() {
        let (mut t, m, _) = running();
        let rt = t.accounts.get_mut(&m.id).unwrap();
        for i in 0..(SEEN_TRADES_CAP + 10) {
            rt.note_fill("BTCUSDT", Some(&i.to_string()));
        }
        assert_eq!(rt.seen_trades.len(), SEEN_TRADES_CAP);
        assert_eq!(rt.seen_order.len(), SEEN_TRADES_CAP);
    }

    #[test]
    fn messages_print_no_credential_listen_key_or_client_id() {
        let (_, _, key) = running();
        let msgs = [
            Message::Stream(key, StreamOutput::Event(fill(Some("t1")))),
            Message::Stream(key, StreamOutput::State(StreamState::Connected)),
            Message::Stream(
                key,
                StreamOutput::InvalidateSession(InvalidReason::KeychainUnavailable),
            ),
            Message::Closed(Uuid::nil()),
            Message::EquityTick,
            Message::EquityPolled(Uuid::nil(), None),
        ];
        let text = format!("{msgs:?}");
        for secret in [SECRET_CID, "listenKey", "apiKey", "secret", "passphrase"] {
            assert!(!text.contains(secret), "leaked {secret}");
        }
    }

    #[test]
    fn order_updates_and_equity_reads_print_no_client_id() {
        let (_, _, key) = running();
        let update = PrivateEvent::Order(exchange::trade::stream::OrderUpdate {
            symbol: "BTCUSDT".into(),
            client_id: Some(SECRET_CID.into()),
            order_id: Some("987654".into()),
            category: exchange::trade::stream::OrderCategory::Plain,
            side: None,
            status: exchange::trade::stream::OrderStatus::New,
            price: None,
            qty: None,
            filled_qty: None,
            reduce_only: None,
            position_side: None,
            time_ms: None,
        });
        let obs = EquityObservation::rest(
            0,
            Decimal::from_int(1000),
            EquityBasis::BinanceTotalMarginBalance,
        );
        let msgs = [
            Message::Stream(key, StreamOutput::Event(Box::new(update))),
            Message::EquityPolled(Uuid::nil(), Some(obs)),
        ];
        let text = format!("{msgs:?}");
        assert!(!text.contains(SECRET_CID), "{text}");
    }

    // ── N1, the book of a flat account, the proxy alert ─────────────────

    #[test]
    fn testing_again_after_a_failed_delete_keeps_the_tracker_and_its_lock() {
        let (mut t, m, _) = running();
        t.apply_risk_settings(
            m.id,
            &AccountRiskSettings {
                daily_loss_limit: Some("100".into()),
                confirm_observations: 1,
                ..AccountRiskSettings::default()
            },
        );
        let basis = EquityBasis::BinanceTotalMarginBalance;
        let base = now_ms();
        {
            let rt = t.accounts.get_mut(&m.id).unwrap();
            for (at, eq) in [(base, 1000), (base + 1, 700)] {
                let _ = rt
                    .risk
                    .observe(EquityObservation::rest(at, Decimal::from_int(eq), basis));
            }
            assert!(rt.risk.is_tripped().unwrap().is_some());
        }
        // The delete failed after the stream was closed: the account is voided, not removed.
        let _ = t.on_tested(&m, false, signed());
        assert!(t.accounts.contains_key(&m.id));
        let _ = t.on_tested(&m, true, signed());
        let rt = &t.accounts[&m.id];
        assert!(
            rt.risk.is_tripped().unwrap().is_some(),
            "a new test must not clear the lock"
        );
        assert_eq!(rt.generation, 2, "one stream was made again");
    }

    fn connect(t: &mut TradingState, key: StreamKey) {
        let _ = t.update(
            Message::Stream(key, StreamOutput::State(StreamState::Connected)),
            0,
        );
    }

    #[test]
    fn a_flat_book_is_stamped_only_when_connected_trusted_and_flat() {
        let (mut t, m, key) = running();
        t.apply_risk_settings(
            m.id,
            &AccountRiskSettings {
                max_symbol_notional: Some("1000".into()),
                ..AccountRiskSettings::default()
            },
        );
        let view = RiskView {
            account: account_key(m.id),
            symbol: "BTCUSDT",
            exposure: Exposure::Adds {
                notional: Some(Decimal::from_int(10)),
            },
        };
        let risk = t.risk_for(m.id).unwrap();
        // Before the first snapshot nothing is trusted: a refresh does not make a book.
        connect(&mut t, key);
        t.refresh_flat_books(now_ms());
        assert_ne!(risk.check(&view), Ok(()), "no snapshot yet");
        // After a flat snapshot the book is stamped; later refreshes keep it fresh.
        let now = now_ms();
        t.accounts
            .get_mut(&m.id)
            .unwrap()
            .apply_snapshot(&[], &[], |r, n| r.set_positions(now, n));
        t.refresh_flat_books(now);
        assert_eq!(risk.check(&view), Ok(()));
        // A held position is never refreshed: it stays unknown.
        t.accounts
            .get_mut(&m.id)
            .unwrap()
            .apply_snapshot(&[], &[held("BTCUSDT", 1)], |r, n| r.set_positions(now, n));
        t.refresh_flat_books(now);
        assert_eq!(risk.check(&view), Err(RiskRefusal::PositionUnknown));
        // Not connected: no refresh either (flat again, but the stream is down).
        t.accounts
            .get_mut(&m.id)
            .unwrap()
            .apply_snapshot(&[], &[], |r, n| r.set_positions(0, n));
        let _ = t.update(
            Message::Stream(key, StreamOutput::State(StreamState::Reconnecting)),
            0,
        );
        t.refresh_flat_books(now);
        assert!(!t.accounts[&m.id].book_trusted);
    }

    #[test]
    fn a_proxy_change_after_the_test_is_a_standing_warning() {
        let (t, m, _) = running();
        let session = session_with(&m);
        assert!(!texts(&standing_alerts(&t, &session, &[])).contains("proxy setting changed"));
        t.set_proxy(ProxyState::Broken);
        let all = texts(&standing_alerts(&t, &session, &[]));
        assert!(all.contains("proxy setting changed"), "{all}");
        assert!(all.contains("test the account again"), "{all}");
    }

    #[test]
    fn a_later_send_never_erases_an_unresolved_result() {
        let (mut t, m, _) = running();
        t.begin_send(m.id, "BTCUSDT");
        assert_eq!(t.pair_state(m.id, "BTCUSDT"), Some(PairState::InFlight));
        t.finish_send(m.id, "BTCUSDT", true);
        assert_eq!(t.pair_state(m.id, "BTCUSDT"), Some(PairState::Unresolved));
        // A stop or a cancel on the same pair starts and ends well.
        t.begin_send(m.id, "BTCUSDT");
        assert_eq!(t.pair_state(m.id, "BTCUSDT"), Some(PairState::InFlight));
        t.finish_send(m.id, "BTCUSDT", false);
        assert_eq!(t.pair_state(m.id, "BTCUSDT"), Some(PairState::Unresolved));
        assert!(texts(&standing_alerts(&t, &session_with(&m), &[])).contains("is unknown"));
        // Another symbol is untouched, and a send that ends well leaves nothing behind.
        t.begin_send(m.id, "ETHUSDT");
        t.finish_send(m.id, "ETHUSDT", false);
        assert_eq!(t.pair_state(m.id, "ETHUSDT"), None);
    }

    // ── Proxy, equity ───────────────────────────────────────────────────

    #[test]
    fn a_changed_proxy_is_seen_by_streams_and_clients_already_built() {
        let (t, _, _) = running();
        let run = t.runs().remove(0);
        let snapshot = run.ctx.signed.proxy_state().clone();
        assert!(check_proxy_snapshot(&snapshot, &(run.ctx.current_proxy)()).is_ok());
        t.set_proxy(ProxyState::Broken);
        // The stream reads the live setting on every connect: it no longer matches the client
        // it was built with, so it refuses to connect (and a client refuses to sign).
        assert!(check_proxy_snapshot(&snapshot, &(run.ctx.current_proxy)()).is_err());
        assert_eq!((t.proxy_reader())(), ProxyState::Broken);
    }

    #[test]
    fn an_equity_read_is_refused_when_the_proxy_changed_and_never_reaches_the_keychain() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let m = meta("main", AccountVenue::Binance, AccountProduct::UsdM);
        let client = SignedHttp::build(ProxyState::Direct).unwrap();
        let read = rt.block_on(read_equity(client, m, Arc::new(|| ProxyState::Broken)));
        assert!(read.is_none());
    }

    #[test]
    fn equity_is_read_only_for_connected_accounts_and_often_enough() {
        let mut t = booted();
        let a = meta("a", AccountVenue::Okx, AccountProduct::UsdM);
        let b = meta("b", AccountVenue::Binance, AccountProduct::UsdM);
        let _ = t.on_tested(&a, true, signed());
        let _ = t.on_tested(&b, true, signed());
        // Nobody is connected: nothing to poll, no timer.
        assert_eq!(t.equity_poll_interval(), None);
        let _ = t.poll_equity();
        assert!(t.accounts.values().all(|rt| !rt.equity_inflight));
        // One connected account with the default 120 s age is read every 40 s.
        t.accounts.get_mut(&a.id).unwrap().state = Some(StreamState::Connected);
        assert_eq!(t.equity_poll_interval(), Some(Duration::from_secs(40)));
        let _ = t.poll_equity();
        assert!(t.accounts[&a.id].equity_inflight);
        assert!(!t.accounts[&b.id].equity_inflight);
        // The shortest allowed age (10 s) is still read more often than it goes stale.
        let short = AccountRiskSettings {
            max_equity_age_secs: 10,
            ..AccountRiskSettings::default()
        };
        t.accounts.get_mut(&b.id).unwrap().state = Some(StreamState::Connected);
        t.apply_risk_settings(b.id, &short);
        assert_eq!(t.equity_poll_interval(), Some(Duration::from_secs(5)));
        // A reading that came back clears the in-flight mark; a failure changes nothing else.
        let _ = t.update(Message::EquityPolled(a.id, None), 0);
        assert!(!t.accounts[&a.id].equity_inflight);
    }

    // ── Standing alerts ─────────────────────────────────────────────────

    fn with_limits(t: &mut TradingState, m: &AccountMeta) {
        t.apply_risk_settings(
            m.id,
            &AccountRiskSettings {
                max_order_notional: Some("100".into()),
                ..AccountRiskSettings::default()
            },
        );
    }

    #[test]
    fn alerts_for_the_startup_journal() {
        let mut t = booted();
        t.on_booted(BootReport {
            journal_unreadable: true,
            records: vec![],
        });
        assert!(
            texts(&standing_alerts(&t, &TradeSession::default(), &[]))
                .contains("could not be read: all new entries are blocked")
        );

        t.on_booted(BootReport {
            journal_unreadable: false,
            records: vec![
                RecordView::new(record(9, "BTCUSDT", SECRET_CID), None),
                RecordView::new(
                    record(9, "ETHUSDT", SECRET_CID),
                    Some(RestoreError::BadClientId),
                ),
            ],
        });
        let alerts = standing_alerts(&t, &TradeSession::default(), &[]);
        let all = texts(&alerts);
        assert!(all.contains("BTCUSDT) was restored"));
        assert!(all.contains("ETHUSDT) has an invalid id"));
        assert!(alerts.iter().any(|a| a.severity == Severity::Error));
        // The text never carries a client id (the report has none).
        assert!(!all.contains(SECRET_CID));
    }

    #[test]
    fn a_lost_pair_alert_only_offers_the_release_when_a_record_exists() {
        let (mut t, m, _) = running();
        t.pairs
            .insert((m.id, "BTCUSDT".into()), PairState::Unresolved);
        let all = texts(&standing_alerts(&t, &TradeSession::default(), &[]));
        assert!(all.contains("until the app restarts"), "{all}");
        assert!(!all.contains("release it by hand"), "{all}");

        let mut rec = RecordView::new(record(9, "BTCUSDT", SECRET_CID), None);
        rec.account = account_key(m.id);
        t.on_booted(BootReport {
            journal_unreadable: false,
            records: vec![rec],
        });
        let all = texts(&standing_alerts(&t, &TradeSession::default(), &[]));
        assert!(all.contains("release it by hand (Order ticket)"), "{all}");
    }

    #[test]
    fn alert_for_a_damaged_order_journal() {
        let mut t = booted();
        assert!(!texts(&standing_alerts(&t, &TradeSession::default(), &[])).contains("journal"));
        t.journal_failed = true;
        assert!(
            texts(&standing_alerts(&t, &TradeSession::default(), &[]))
                .contains("journal is damaged")
        );
    }

    #[test]
    fn alerts_for_each_stream_state() {
        let (mut t, m, key) = running();
        with_limits(&mut t, &m);
        let session = session_with(&m);
        let cases = [
            (
                StreamState::Reconnecting,
                Some("reconnecting"),
                Severity::Warning,
            ),
            (StreamState::Resyncing, Some("resyncing"), Severity::Warning),
            (
                StreamState::AuthFailed,
                Some("credentials were rejected"),
                Severity::Error,
            ),
            (
                StreamState::Unavailable,
                Some("unavailable"),
                Severity::Error,
            ),
            (StreamState::Connected, None, Severity::Info),
        ];
        for (state, expect, severity) in cases {
            let _ = t.update(Message::Stream(key, StreamOutput::State(state)), 0);
            let alerts = standing_alerts(&t, &session, &[]);
            let all = texts(&alerts);
            match expect {
                Some(word) => {
                    let hit = alerts.iter().find(|a| a.text.contains(word));
                    assert_eq!(hit.map(|a| a.severity), Some(severity), "{state:?}");
                }
                None => assert!(
                    !all.contains("private stream") && !all.contains("credentials"),
                    "{state:?}: {all}"
                ),
            }
        }
    }

    #[test]
    fn alert_for_a_risk_lock() {
        let (mut t, m, _) = running();
        t.apply_risk_settings(
            m.id,
            &AccountRiskSettings {
                daily_loss_limit: Some("100".into()),
                confirm_observations: 1,
                ..AccountRiskSettings::default()
            },
        );
        let basis = EquityBasis::BinanceTotalMarginBalance;
        let base = now_ms();
        let rt = t.accounts.get_mut(&m.id).unwrap();
        for (at, eq) in [(base, 1000), (base + 1, 700)] {
            let _ = rt
                .risk
                .observe(EquityObservation::rest(at, Decimal::from_int(eq), basis));
        }
        assert!(rt.risk.is_tripped().unwrap().is_some(), "the limit tripped");
        let all = texts(&standing_alerts(&t, &session_with(&m), &[]));
        assert!(all.contains("risk lock (daily_loss)"));
    }

    #[test]
    fn alert_for_an_unusable_risk_state() {
        let (mut t, m, _) = running();
        with_limits(&mut t, &m);
        let risk = t.accounts[&m.id].risk.clone();
        // A panic while the lock is held poisons it.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = risk.with_tracker(|_| panic!("poison the tracker"));
        }));
        let all = texts(&standing_alerts(&t, &session_with(&m), &[]));
        assert!(all.contains("risk state is unavailable"));
    }

    #[test]
    fn alert_when_no_risk_limit_is_set_and_none_once_one_is() {
        let (mut t, m, _) = running();
        let session = session_with(&m);
        assert!(texts(&standing_alerts(&t, &session, &[])).contains("main: no risk limits set"));
        with_limits(&mut t, &m);
        assert!(!texts(&standing_alerts(&t, &session, &[])).contains("no risk limits set"));
    }

    #[test]
    fn alert_for_spot_and_futures_of_one_environment() {
        let mut t = booted();
        let spot = meta("s", AccountVenue::Binance, AccountProduct::Spot);
        let fut = meta("f", AccountVenue::Binance, AccountProduct::UsdM);
        let _ = t.on_tested(&spot, true, signed());
        assert!(
            !texts(&standing_alerts(&t, &TradeSession::default(), &[]))
                .contains("share one exchange")
        );
        let _ = t.on_tested(&fut, true, signed());
        assert!(
            texts(&standing_alerts(&t, &TradeSession::default(), &[]))
                .contains("share one exchange")
        );
    }

    #[test]
    fn alerts_are_pure_state_and_empty_without_accounts_or_boot_trouble() {
        let t = booted();
        assert!(standing_alerts(&t, &TradeSession::default(), &[]).is_empty());
    }

    #[test]
    fn the_only_trade_calls_here_are_reads() {
        // The send paths are not used by this module (c2b-1 has no order path).
        let src = include_str!("trading.rs");
        // The tests module only: single `#[cfg(test)]` items come before it and stay scanned.
        let code = src.split("#[cfg(test)]\nmod tests").next().unwrap();
        for call in [
            ".submit(",
            ".cancel_all(",
            ".auto_close(",
            "submit_auto(",
            "submit_replace(",
            "replace_grant(",
            "for_replace(",
        ] {
            assert!(!code.contains(call), "{call} must not appear in trading.rs");
        }
        // `.flatten()` without arguments is `Stream::flatten`; an order flatten takes arguments.
        assert_eq!(
            code.matches(".flatten(").count(),
            code.matches(".flatten()").count(),
            "an order flatten call must not appear in trading.rs"
        );
    }

    // ── Unknown results (c2b-3) ─────────────────────────────────────────

    fn boot_with(records: Vec<UnknownRecord>) -> TradingState {
        let mut t = TradingState::new(ProxyState::Direct);
        t.on_booted(plan_boot(Ok(records), || {}, |_| Ok(())));
        t
    }

    fn id_of(m: &AccountMeta) -> AccountKey {
        account_key(m.id)
    }

    fn rec(m: &AccountMeta, symbol: &str, cid: &str) -> UnknownRecord {
        UnknownRecord {
            account: id_of(m),
            symbol: symbol.into(),
            client_id: cid.into(),
            kind: "x".into(),
            recorded_at_ms: 0,
        }
    }

    #[test]
    fn a_saved_record_is_pending_until_the_account_is_tested_and_is_looked_up_once() {
        let m = meta("a", AccountVenue::Binance, AccountProduct::UsdM);
        let r = rec(&m, "BTCUSDT", "eAbc123");
        let mut t = boot_with(vec![r.clone()]);
        assert_eq!(t.record_status(m.id, &r), RecordStatus::AwaitingTest);
        assert!(
            t.record_status(m.id, &r)
                .text()
                .contains("Pending verification")
        );
        // The test passed: the record is looked up, once.
        assert_eq!(t.begin_lookup(m.id), vec![r.clone()]);
        assert!(t.begin_lookup(m.id).is_empty());
        assert_eq!(t.record_status(m.id, &r), RecordStatus::Looking);
        // No answer: still unknown, still locked, and a later test looks again.
        t.lookup_finished(&r, LookupEnd::StillUnknown);
        assert_eq!(t.record_status(m.id, &r), RecordStatus::StillUnknown);
        assert!(t.is_record_locked(m.id, "BTCUSDT"));
        assert_eq!(t.begin_lookup(m.id).len(), 1);
        // An answer drops it from the locked list.
        t.lookup_finished(&r, LookupEnd::Found);
        assert_eq!(t.record_status(m.id, &r), RecordStatus::SessionUnknown);
        assert!(t.begin_lookup(m.id).is_empty());
    }

    /// A release before the account's test would skip the lookup that follows the test.
    #[test]
    fn a_record_awaiting_the_test_cannot_be_released_by_hand() {
        let m = meta("a", AccountVenue::Binance, AccountProduct::UsdM);
        let r = rec(&m, "BTCUSDT", "eAbc123");
        let mut t = boot_with(vec![r.clone()]);
        let st = t.record_status(m.id, &r);
        assert_eq!(st, RecordStatus::AwaitingTest);
        assert!(!st.may_release() && !st.may_discard());
        // After the test and a lookup with no answer the release is open.
        assert_eq!(t.begin_lookup(m.id).len(), 1);
        assert!(!t.record_status(m.id, &r).may_release());
        t.lookup_finished(&r, LookupEnd::StillUnknown);
        assert!(t.record_status(m.id, &r).may_release());
    }

    #[test]
    fn records_of_an_account_with_a_bulk_run_show_as_looking_and_cannot_be_released() {
        let m = meta("a", AccountVenue::Binance, AccountProduct::UsdM);
        let other = meta("b", AccountVenue::Binance, AccountProduct::UsdM);
        let r = rec(&m, "BTCUSDT", "eAbc123");
        let mut t = boot_with(vec![r.clone()]);
        t.begin_lookup(m.id);
        t.lookup_finished(&r, LookupEnd::StillUnknown);
        assert!(t.record_status(m.id, &r).may_release());
        t.begin_bulk(m.id);
        let st = t.record_status(m.id, &r);
        assert_eq!(st, RecordStatus::Looking);
        assert!(!st.may_release());
        // Another account is not affected, and an invalid id is still only discardable.
        let o = rec(&other, "BTCUSDT", "eAbc123");
        assert_ne!(t.record_status(other.id, &o), RecordStatus::Looking);
        let bad = rec(&m, "BTCUSDT", "not valid!");
        assert_eq!(t.record_status(m.id, &bad), RecordStatus::Invalid);
        t.finish_bulk(m.id);
        assert!(t.record_status(m.id, &r).may_release());
    }

    #[test]
    fn a_record_with_an_invalid_id_is_never_looked_up_and_can_only_be_discarded() {
        let m = meta("a", AccountVenue::Binance, AccountProduct::UsdM);
        let r = rec(&m, "BTCUSDT", "not valid!");
        let mut t = TradingState::new(ProxyState::Direct);
        t.on_booted(plan_boot(
            Ok(vec![r.clone()]),
            || {},
            |_| Err(RestoreError::BadClientId),
        ));
        assert!(t.begin_lookup(m.id).is_empty());
        let st = t.record_status(m.id, &r);
        assert_eq!(st, RecordStatus::Invalid);
        assert!(st.may_discard() && !st.may_release());
        // After the discard the record stays listed as blocked, not unlocked.
        t.record_discarded(m.id, "not valid!");
        // The block stays: the account still refuses new entries on every symbol.
        assert!(t.is_record_locked(m.id, "ETHUSDT"));
        assert!(t.begin_lookup(m.id).is_empty());
        assert_eq!(t.record_status(m.id, &r), RecordStatus::Invalid);
    }

    #[test]
    fn a_manual_release_clears_the_unresolved_pair_unless_another_record_names_it() {
        let m = meta("a", AccountVenue::Binance, AccountProduct::UsdM);
        let r = rec(&m, "BTCUSDT", "eAbc123");
        let mut t = boot_with(vec![r.clone()]);
        t.finish_send(m.id, "BTCUSDT", true);
        assert_eq!(t.pair_state(m.id, "BTCUSDT"), Some(PairState::Unresolved));
        // Another saved record still names the symbol: the pair stays.
        t.record_released(m.id, "BTCUSDT", "eAbc123", true);
        assert_eq!(t.pair_state(m.id, "BTCUSDT"), Some(PairState::Unresolved));
        t.record_released(m.id, "BTCUSDT", "eAbc123", false);
        assert_eq!(t.pair_state(m.id, "BTCUSDT"), None);
        assert!(!t.is_record_locked(m.id, "BTCUSDT"));
    }

    #[test]
    fn a_duplicate_alias_gets_the_venue_and_the_end_of_the_id_and_others_do_not() {
        let a = meta("Main", AccountVenue::Binance, AccountProduct::UsdM);
        let b = meta(" main ", AccountVenue::Okx, AccountProduct::UsdM);
        let c = meta("other", AccountVenue::Okx, AccountProduct::UsdM);
        let saved = vec![a.clone(), b.clone(), c.clone()];
        let shown = display_name(&saved, &a);
        assert!(
            shown.starts_with("Main [") && shown.contains("Binance"),
            "{shown}"
        );
        let id = a.id.simple().to_string();
        assert!(shown.contains(&id[id.len() - 4..]));
        assert_ne!(shown, display_name(&saved, &b));
        assert_eq!(display_name(&saved, &c), "other");
        // The alert and the confirmation name the account from the saved list.
        assert_eq!(name_of(&saved, id_of(&c)).as_deref(), Some("other"));
        assert_eq!(name_of(&[], id_of(&c)), None);
    }

    #[test]
    fn the_lists_are_stale_without_a_connected_stream_and_a_bulk_run_is_tracked() {
        let mut t = booted();
        let id = Uuid::new_v4();
        assert!(t.lists_stale(id));
        assert!(!t.bulk_running(id));
        t.begin_bulk(id);
        assert!(t.bulk_running(id));
        t.finish_bulk(id);
        assert!(!t.bulk_running(id));
    }

    // ── d1: protection status from the lists ────────────────────────────

    fn algo_update(category: OrderCategory) -> Box<PrivateEvent> {
        Box::new(PrivateEvent::Order(exchange::trade::stream::OrderUpdate {
            symbol: "BTCUSDT".into(),
            client_id: None,
            order_id: None,
            category,
            side: None,
            status: exchange::trade::stream::OrderStatus::Triggered,
            price: None,
            qty: None,
            filled_qty: None,
            reduce_only: None,
            position_side: None,
            time_ms: None,
        }))
    }

    fn long_stop() -> OpenOrder {
        use exchange::trade::{OpenKind, ProtectRole, Side};
        OpenOrder {
            symbol: "BTCUSDT".into(),
            id: "7".into(),
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

    fn connect_with_stop(t: &mut TradingState, m: &AccountMeta, key: StreamKey) {
        let _ = t.update(
            Message::Stream(key, StreamOutput::State(StreamState::Connected)),
            0,
        );
        t.accounts.get_mut(&m.id).unwrap().apply_snapshot(
            &[long_stop()],
            &[held("BTCUSDT", 2)],
            |r, n| r.set_positions(0, n),
        );
    }

    #[test]
    fn a_fill_that_arrives_twice_is_counted_once_and_one_without_ids_is_not_guessed() {
        use crate::auto_flow::tests::{entry_for, filled_of, spec};
        use exchange::trade::Side;
        use exchange::trade::stream::{Fill, PrivateEvent};
        let (mut t, m, key) = running();
        connect_with_stop(&mut t, &m, key);
        let (entry_id, init) = entry_for(m.id, spec(1, false));
        assert!(t.register_attach(
            entry_id.clone(),
            PendingAttach {
                account: m.id,
                symbol: "BTCUSDT".into(),
                plan: crate::protect_attach::AttachPlan {
                    kind: crate::protect_attach::AttachKind::AfterFill,
                    dir: Dir::Long,
                    limit: true,
                    spec: spec(1, false),
                    lines: Vec::new(),
                },
            },
            init,
        ));
        t.settle_attach(m.id, "BTCUSDT", &SendResult::Accepted { order_id: None });
        let fill = |trade: Option<&str>, client: Option<&str>| {
            PrivateEvent::Fill(Fill {
                symbol: "BTCUSDT".into(),
                client_id: client.map(str::to_string),
                order_id: None,
                trade_id: trade.map(str::to_string),
                side: Some(Side::Buy),
                qty: Decimal::parse("0.001").unwrap(),
                price: Decimal::parse("60000").unwrap(),
                fee: None,
                fee_asset: None,
                realized_pnl: None,
                time_ms: None,
            })
        };
        let send = |t: &mut TradingState, e: PrivateEvent| {
            let _ = t.update(Message::Stream(key, StreamOutput::Event(Box::new(e))), 0);
        };
        send(&mut t, fill(Some("t1"), Some(&entry_id)));
        send(&mut t, fill(Some("t1"), Some(&entry_id)));
        // No client id, or no trade id: not matched, not counted.
        send(&mut t, fill(Some("t2"), None));
        send(&mut t, fill(None, Some(&entry_id)));
        assert_eq!(
            filled_of(&t.auto, &entry_id),
            Some(Decimal::parse("0.001").unwrap())
        );
        let jobs = t.poll_auto(2_000, 2_000, &|_| true);
        assert_eq!(jobs.len(), 1, "one stop for the one counted fill");
        assert!(t.poll_auto(2_000, 2_000, &|_| true).is_empty());
    }

    #[test]
    fn holdings_name_the_directions_on_the_symbol_and_say_when_the_list_is_stale() {
        use crate::protect_attach::check_new_entry;
        use exchange::account::PositionMode;
        let (mut t, m, key) = running();
        // No stream state yet: the list cannot be trusted.
        assert!(t.holdings(m.id, "BTCUSDT").stale);
        connect_with_stop(&mut t, &m, key);
        let h = t.holdings(m.id, "btcusdt");
        assert!(!h.stale);
        assert_eq!(h.dirs.len(), 1);
        let held_dir = h.dirs[0];
        // Another symbol is not held.
        assert!(t.holdings(m.id, "ETHUSDT").dirs.is_empty());
        // N1: a one-way entry against the held direction is refused with these holdings, one
        // with it is not; a stale list refuses both.
        let other = match held_dir {
            Dir::Long => Dir::Short,
            Dir::Short => Dir::Long,
        };
        let one_way = Some(PositionMode::OneWay);
        let usdm = AccountProduct::UsdM;
        assert!(check_new_entry(usdm, one_way, other, &h).is_err());
        assert!(check_new_entry(usdm, one_way, held_dir, &h).is_ok());
        let _ = t.update(
            Message::Stream(key, StreamOutput::Event(algo_update(OrderCategory::Algo))),
            0,
        );
        let stale = t.holdings(m.id, "BTCUSDT");
        assert!(stale.stale);
        assert!(check_new_entry(usdm, one_way, held_dir, &stale).is_err());
    }

    #[test]
    fn a_finished_replace_never_leaves_the_lists_stale_so_retry_protection_stays_usable() {
        use crate::replace_flow::{CancelEnd, NewLeg, ReplaceResult};
        // A replace that changed nothing on the exchange (cancel refused or not sent), or whose
        // new order failed after the stream already read the cancel, sends no further event.
        // Nothing in the app may mark the lists then: no event would ever clear the mark.
        let (mut t, m, key) = running();
        connect_with_stop(&mut t, &m, key);
        let results = [
            ReplaceResult::CancelNotConfirmed(CancelEnd::NotSent),
            ReplaceResult::CancelNotConfirmed(CancelEnd::Rejected {
                code: "-2011".into(),
                msg: "Unknown order".into(),
            }),
            ReplaceResult::NewNotPlaced(NewLeg::NotPlacedPresumed),
            ReplaceResult::NewNotPlaced(NewLeg::Unresolved),
            ReplaceResult::Lost,
        ];
        let mut sticky = false;
        for result in results {
            let mut s = crate::replace_flow::tests::summary_of(result);
            s.account = m.id;
            t.begin_send(m.id, &s.symbol);
            let blocked = t.on_replace_result(&s);
            let _ = crate::replace_flow::handle(&s);
            let tag = s.result.tag();
            assert!(!t.lists_stale(m.id), "{tag}");
            assert!(
                !t.holdings(m.id, "BTCUSDT").stale,
                "{tag}: Retry needs a fresh list"
            );
            let expect = matches!(
                s.result,
                ReplaceResult::Lost | ReplaceResult::NewNotPlaced(NewLeg::Unresolved)
            );
            assert_eq!(blocked, expect, "{tag}");
            // An unknown result stays until restart; a later one that ended well never clears it.
            sticky |= expect;
            let ps = t.pair_state(m.id, "BTCUSDT");
            assert_eq!(ps == Some(PairState::Unresolved), sticky, "{tag}");
        }
        // The handler of the result does not mark the lists (the algo event does, in `update`).
        let src = include_str!("main.rs");
        let body = src
            .split("fn on_replace_sent(")
            .nth(1)
            .and_then(|r| r.split("fn on_order_sent(").next())
            .unwrap();
        assert!(
            !body.contains("orders_dirty"),
            "on_replace_sent must not mark the lists"
        );
    }

    #[test]
    fn an_algo_order_event_makes_the_lists_undetermined_until_the_next_snapshot() {
        let (mut t, m, key) = running();
        connect_with_stop(&mut t, &m, key);
        assert!(!t.lists_stale(m.id));
        assert_eq!(
            t.protection(m.id)
                .iter()
                .map(|p| p.protection)
                .collect::<Vec<_>>(),
            vec![exchange::trade::Protection::Stop]
        );
        // A plain order event leaves the lists as they were.
        let _ = t.update(
            Message::Stream(key, StreamOutput::Event(algo_update(OrderCategory::Plain))),
            0,
        );
        assert!(!t.lists_stale(m.id));
        // An algo event: nothing can be said about protection or orphans.
        let _ = t.update(
            Message::Stream(key, StreamOutput::Event(algo_update(OrderCategory::Algo))),
            0,
        );
        assert!(t.lists_stale(m.id));
        assert_eq!(
            t.protection(m.id)
                .iter()
                .map(|p| p.protection)
                .collect::<Vec<_>>(),
            vec![exchange::trade::Protection::Undetermined]
        );
        // The snapshot that follows the read clears it.
        t.accounts
            .get_mut(&m.id)
            .unwrap()
            .apply_snapshot(&[], &[held("BTCUSDT", 2)], |r, n| r.set_positions(0, n));
        assert!(!t.lists_stale(m.id));
        assert_eq!(
            t.protection(m.id)
                .iter()
                .map(|p| p.protection)
                .collect::<Vec<_>>(),
            vec![exchange::trade::Protection::Unprotected]
        );
    }

    #[test]
    fn a_snapshot_entry_price_is_held_and_an_event_can_replace_it() {
        let (mut t, m, key) = running();
        connect_with_stop(&mut t, &m, key);
        let rt = t.accounts.get_mut(&m.id).unwrap();
        let mut with_px = held("BTCUSDT", 2);
        with_px.entry_price = Some(Decimal::from_int(60_000));
        let mut zero_px = held("ETHUSDT", 1);
        zero_px.entry_price = Some(Decimal::ZERO);
        rt.apply_snapshot(&[], &[with_px, zero_px, held("SOLUSDT", 1)], |r, n| {
            r.set_positions(0, n)
        });
        let px_of = |rt: &AccountRuntime, s: &str| {
            rt.positions
                .iter()
                .find(|p| p.symbol == s)
                .unwrap()
                .entry_price
        };
        assert_eq!(px_of(rt, "BTCUSDT"), Some(Decimal::from_int(60_000)));
        assert_eq!(px_of(rt, "ETHUSDT"), None, "a zero price is none");
        assert_eq!(px_of(rt, "SOLUSDT"), None);
        let newer = Decimal::from_int(61_500);
        rt.apply_position_event(
            "BTCUSDT",
            PositionSide::Both,
            Decimal::from_int(3),
            Some(newer),
        );
        assert_eq!(px_of(rt, "BTCUSDT"), Some(newer));
        // The risk side still sees the same positions as before.
        assert_eq!(rt.exchange_positions().len(), 3);
    }

    /// A position event brings an entry price a snapshot may lack, and a flat event removes
    /// the position with it.
    #[test]
    fn a_position_event_brings_the_entry_price_a_snapshot_lacks() {
        let (mut t, m, key) = running();
        connect_with_stop(&mut t, &m, key);
        let rt = t.accounts.get_mut(&m.id).unwrap();
        assert!(rt.positions.iter().all(|p| p.entry_price.is_none()));
        let px = Decimal::from_int(61_000);
        rt.apply_position_event(
            "BTCUSDT",
            PositionSide::Both,
            Decimal::from_int(2),
            Some(px),
        );
        assert_eq!(rt.positions[0].entry_price, Some(px));
        rt.apply_position_event("BTCUSDT", PositionSide::Both, Decimal::ZERO, None);
        assert!(rt.positions.is_empty());
    }

    #[test]
    fn the_direction_of_a_one_way_position_survives_the_signed_size() {
        let (mut t, m, key) = running();
        connect_with_stop(&mut t, &m, key);
        let rt = t.accounts.get_mut(&m.id).unwrap();
        rt.apply_position_event("ETHUSDT", PositionSide::Both, Decimal::from_int(-3), None);
        rt.apply_position_event("SOLUSDT", PositionSide::Short, Decimal::from_int(1), None);
        let dir_of = |rt: &AccountRuntime, s: &str| {
            rt.positions.iter().find(|p| p.symbol == s).map(|p| p.dir)
        };
        assert_eq!(dir_of(rt, "ETHUSDT"), Some(Dir::Short));
        assert_eq!(dir_of(rt, "SOLUSDT"), Some(Dir::Short));
        assert_eq!(dir_of(rt, "BTCUSDT"), Some(Dir::Long));
        // A short is not protected by a sell stop; the sell stop of the long is.
        let by_symbol: Vec<_> = t
            .protection(m.id)
            .into_iter()
            .map(|p| (p.symbol, p.protection))
            .collect();
        assert!(by_symbol.contains(&(
            "ETHUSDT".to_string(),
            exchange::trade::Protection::Unprotected
        )));
        assert!(by_symbol.contains(&("BTCUSDT".to_string(), exchange::trade::Protection::Stop)));
    }

    #[test]
    fn protection_is_empty_for_an_unknown_account() {
        let t = booted();
        let id = Uuid::new_v4();
        assert!(t.protection(id).is_empty());
    }
    fn pending_for(id: Uuid, symbol: &str, limit: bool) -> PendingAttach {
        use crate::protect_attach::{AttachKind, AttachPlan};
        PendingAttach {
            account: id,
            symbol: symbol.into(),
            plan: AttachPlan {
                kind: AttachKind::AfterFill,
                dir: Dir::Long,
                limit,
                spec: exchange::trade::ProtectSpec::default(),
                lines: Vec::new(),
            },
        }
    }

    /// A listed but not yet sent protection is "pending", never "protected", even when a stop
    /// is on the list; once the entry is refused the pending state is gone (d2).
    #[test]
    fn a_pending_entry_protection_is_never_shown_as_protected() {
        let (mut t, m, key) = running();
        connect_with_stop(&mut t, &m, key);
        let status = |t: &TradingState| {
            t.protection(m.id)
                .iter()
                .map(|p| p.protection)
                .collect::<Vec<_>>()
        };
        assert_eq!(status(&t), vec![exchange::trade::Protection::Stop]);
        assert!(t.register_okx_attach("eAAAA".into(), pending_for(m.id, "BTCUSDT", true)));
        assert_eq!(status(&t), vec![exchange::trade::Protection::EntryPending]);
        assert!(t.has_resting_limit_attach());
        assert!(
            exchange::trade::Protection::EntryPending
                .label()
                .contains("pending")
        );
        // Also while the lists may be out of date.
        let _ = t.update(
            Message::Stream(key, StreamOutput::Event(algo_update(OrderCategory::Algo))),
            0,
        );
        assert_eq!(status(&t), vec![exchange::trade::Protection::EntryPending]);
        // The send was refused: nothing is pending any more.
        t.settle_attach(m.id, "BTCUSDT", &SendResult::Refused("gate".into()));
        assert!(!t.has_resting_limit_attach());
        assert_ne!(status(&t), vec![exchange::trade::Protection::EntryPending]);
        // Another symbol's pending entry does not touch this position.
        assert!(t.register_okx_attach("eBBBB".into(), pending_for(m.id, "ETHUSDT", false)));
        assert_ne!(status(&t), vec![exchange::trade::Protection::EntryPending]);
        // Deleting the account drops what was remembered for it.
        t.remove(m.id);
        assert!(!t.has_resting_limit_attach());
    }

    /// OKX partial fill: a position exists while the attached orders wait for the entry to be
    /// complete. The position says so in words (d4), over a stop that is already listed.
    #[test]
    fn a_partly_filled_okx_entry_says_protection_is_not_active_yet() {
        let (mut t, m, key) = running();
        connect_with_stop(&mut t, &m, key);
        assert!(t.register_okx_attach("eAAAA".into(), pending_for(m.id, "BTCUSDT", true)));
        let all = t.protection(m.id);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].protection, exchange::trade::Protection::EntryPending);
        assert!(
            all[0]
                .protection
                .label()
                .starts_with("Protection not active yet")
        );
    }

    /// An order the app sent by itself and could not confirm is an ordinary unknown record: the
    /// same status, the same wait for the account test and lookup, and the same release step
    /// (the typed phrase). Nothing in the status depends on who sent it (d4).
    #[test]
    fn an_unknown_result_of_an_automatic_order_follows_the_manual_release_flow() {
        let m = meta("a", AccountVenue::Binance, AccountProduct::UsdM);
        let mut r = rec(&m, "BTCUSDT", "sAuto123");
        r.kind = "stop_market".into();
        let mut t = boot_with(vec![r.clone()]);
        let st = t.record_status(m.id, &r);
        assert_eq!(st, RecordStatus::AwaitingTest);
        assert!(!st.may_release());
        assert!(t.is_record_locked(m.id, "BTCUSDT"));
        assert_eq!(t.begin_lookup(m.id).len(), 1);
        t.lookup_finished(&r, LookupEnd::StillUnknown);
        assert!(t.record_status(m.id, &r).may_release());
        assert!(t.is_record_locked(m.id, "BTCUSDT"));
    }

    #[test]
    fn the_same_trade_id_on_two_symbols_is_two_fills() {
        let (mut t, m, _) = running();
        let rt = t.accounts.get_mut(&m.id).unwrap();
        assert!(rt.note_fill("BTCUSDT", Some("7")));
        assert!(
            rt.note_fill("ETHUSDT", Some("7")),
            "another symbol, another fill"
        );
        assert!(
            !rt.note_fill("BTCUSDT", Some("7")),
            "a repeat on the same symbol"
        );
        assert!(!rt.note_fill("ETHUSDT", Some("7")));
        assert_eq!(rt.fills_seen, 2);
    }

    /// An entry registered the way the confirmation does, with the send settled.
    fn registered_entry(t: &mut TradingState, m: &AccountMeta) -> String {
        use crate::auto_flow::tests::{entry_for, spec};
        let (entry_id, init) = entry_for(m.id, spec(1, false));
        assert!(t.register_attach(
            entry_id.clone(),
            PendingAttach {
                account: m.id,
                symbol: "BTCUSDT".into(),
                plan: crate::protect_attach::AttachPlan {
                    kind: crate::protect_attach::AttachKind::AfterFill,
                    dir: Dir::Long,
                    limit: true,
                    spec: spec(1, false),
                    lines: Vec::new(),
                },
            },
            init,
        ));
        t.settle_attach(m.id, "BTCUSDT", &SendResult::Accepted { order_id: None });
        entry_id
    }

    #[test]
    fn a_second_entry_with_automatic_protection_waits_for_the_first_to_end() {
        let (mut t, m, key) = running();
        connect_with_stop(&mut t, &m, key);
        assert!(!t.auto_active(m.id, "BTCUSDT"));
        let entry_id = registered_entry(&mut t, &m);
        assert!(t.auto_active(m.id, "BTCUSDT"));
        assert!(t.auto_active(m.id, "btcusdt"));
        assert!(!t.auto_active(m.id, "ETHUSDT"));
        assert!(!t.auto_active(uuid::Uuid::from_u128(1), "BTCUSDT"));
        // The entry is cancelled with nothing filled: nothing is owed, the symbol is free.
        let update = |status| {
            PrivateEvent::Order(exchange::trade::stream::OrderUpdate {
                symbol: "BTCUSDT".into(),
                client_id: Some(entry_id.clone()),
                order_id: None,
                category: OrderCategory::Plain,
                side: None,
                status,
                price: None,
                qty: None,
                filled_qty: Some(Decimal::ZERO),
                reduce_only: None,
                position_side: None,
                time_ms: None,
            })
        };
        let _ = t.update(
            Message::Stream(
                key,
                StreamOutput::Event(Box::new(update(
                    exchange::trade::stream::OrderStatus::Canceled,
                ))),
            ),
            0,
        );
        assert!(!t.auto_active(m.id, "BTCUSDT"));
    }

    #[test]
    fn a_reconnect_that_finds_a_position_and_no_entry_order_is_noted_and_sends_nothing() {
        let (mut t, m, key) = running();
        connect_with_stop(&mut t, &m, key);
        let _entry_id = registered_entry(&mut t, &m);
        // The stream drops and comes back: the check runs at `Connected`, after the snapshot.
        let _ = t.update(
            Message::Stream(key, StreamOutput::State(StreamState::Reconnecting)),
            0,
        );
        assert!(!t.accounts[&m.id].book_trusted);
        t.accounts
            .get_mut(&m.id)
            .unwrap()
            .apply_snapshot(&[], &[held("BTCUSDT", 2)], |r, n| r.set_positions(0, n));
        assert!(
            t.auto_notes(m.id).is_empty(),
            "not before the state is Connected"
        );
        let _ = t.update(
            Message::Stream(key, StreamOutput::State(StreamState::Connected)),
            0,
        );
        let notes = t.auto_notes(m.id);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("connection was lost"));
        assert!(
            t.poll_auto(2_000, 2_000, &|_| true).is_empty(),
            "nothing is sent for it"
        );
        assert!(!t.auto_active(m.id, "BTCUSDT"));
    }

    fn entry_event(
        entry_id: &str,
        status: exchange::trade::stream::OrderStatus,
        category: OrderCategory,
    ) -> PrivateEvent {
        PrivateEvent::Order(exchange::trade::stream::OrderUpdate {
            symbol: "BTCUSDT".into(),
            client_id: Some(entry_id.to_string()),
            order_id: None,
            category,
            side: None,
            status,
            price: None,
            qty: None,
            filled_qty: None,
            reduce_only: None,
            position_side: None,
            time_ms: None,
        })
    }

    fn entry_fill(entry_id: &str, trade: &str) -> PrivateEvent {
        PrivateEvent::Fill(exchange::trade::stream::Fill {
            symbol: "BTCUSDT".into(),
            client_id: Some(entry_id.to_string()),
            order_id: None,
            trade_id: Some(trade.to_string()),
            side: Some(exchange::trade::Side::Buy),
            qty: Decimal::parse("0.001").unwrap(),
            price: Decimal::parse("60000").unwrap(),
            fee: None,
            fee_asset: None,
            realized_pnl: None,
            time_ms: None,
        })
    }

    /// A resync while an entry stands: `first` is what starts it, then a snapshot with a long
    /// position and no entry order, then the fill (before or after `Connected`). Returns the
    /// number of notes, whether a stop job came out, and whether the entry is still active.
    fn resync_with_fill(
        first: Option<PrivateEvent>,
        fill_after_connected: bool,
    ) -> (usize, bool, bool) {
        let (mut t, m, key) = running();
        connect_with_stop(&mut t, &m, key);
        // The first connect of a stream consumes the flag in the real flow (the snapshot comes
        // before `Connected`); this helper applies its snapshot after it.
        t.accounts.get_mut(&m.id).unwrap().recheck_auto = false;
        let entry_id = registered_entry(&mut t, &m);
        let send = |t: &mut TradingState, o: StreamOutput| {
            let _ = t.update(Message::Stream(key, o), 0);
        };
        let event = |e| StreamOutput::Event(Box::new(e));
        let state = |s| StreamOutput::State(s);
        match first {
            Some(e) => send(&mut t, event(e)),
            None => send(&mut t, state(StreamState::Reconnecting)),
        }
        send(&mut t, state(StreamState::Resyncing));
        t.accounts
            .get_mut(&m.id)
            .unwrap()
            .apply_snapshot(&[], &[held("BTCUSDT", 2)], |r, n| r.set_positions(0, n));
        if !fill_after_connected {
            send(&mut t, event(entry_fill(&entry_id, "t1")));
        }
        send(&mut t, state(StreamState::Connected));
        if fill_after_connected {
            send(&mut t, event(entry_fill(&entry_id, "t1")));
        }
        let notes = t.auto_notes(m.id).len();
        let jobs = t.poll_auto(2_000, 2_000, &|_| true);
        let stop = jobs.len() == 1 && format!("{:?}", jobs[0]).contains("Place(Stop)");
        assert!(stop || jobs.is_empty(), "{jobs:?}");
        (notes, stop, t.auto_active(m.id, "BTCUSDT"))
    }

    #[test]
    fn a_resync_caused_by_an_algo_event_is_not_a_reconnect() {
        use exchange::trade::stream::OrderStatus;
        let algo = entry_event("x", OrderStatus::New, OrderCategory::Algo);
        let (notes, stop, active) = resync_with_fill(Some(algo), false);
        assert_eq!(notes, 0, "no note: the fill was held back, not lost");
        assert!(stop, "the fill still gets its stop");
        assert!(active);
    }

    #[test]
    fn a_reconnect_notes_and_a_later_fill_still_gets_its_stop() {
        let (notes, stop, active) = resync_with_fill(None, true);
        assert_eq!(notes, 1);
        assert!(
            stop,
            "a fill of the entry seen after the note still gets its stop"
        );
        assert!(!active);
    }

    #[test]
    fn a_resync_required_event_is_treated_like_a_reconnect() {
        use exchange::trade::stream::ResyncReason;
        let ev = PrivateEvent::ResyncRequired(ResyncReason::MalformedFrame);
        let (notes, stop, active) = resync_with_fill(Some(ev), true);
        assert_eq!(notes, 1);
        assert!(stop);
        assert!(!active);
    }

    // ── d5: bare positions, strays, labels ──────────────────────────────

    fn connect_lists(
        t: &mut TradingState,
        m: &AccountMeta,
        key: StreamKey,
        orders: &[OpenOrder],
        positions: &[Position],
    ) {
        let _ = t.update(
            Message::Stream(key, StreamOutput::State(StreamState::Connected)),
            0,
        );
        t.accounts
            .get_mut(&m.id)
            .unwrap()
            .apply_snapshot(orders, positions, |r, n| r.set_positions(0, n));
    }

    fn algo_row(id: &str, role: exchange::trade::ProtectRole, client: Option<&str>) -> OpenOrder {
        let mut o = long_stop();
        o.id = id.into();
        o.role = Some(role);
        o.trigger_price = Some(Decimal::from_int(59000));
        o.client_id = client.map(str::to_string);
        o
    }

    const APP_ID: &str = "sAAAAAAAAAAAAAAAAAAAA";

    #[test]
    fn a_bare_position_is_confirmed_after_the_grace_time_and_told_once() {
        let (mut t, m, key) = running();
        connect_lists(&mut t, &m, key, &[], &[held("BTCUSDT", 2)]);
        t.watch_bare(0);
        assert!(t.bare_rows(m.id).is_empty(), "inside the grace time");
        assert!(t.take_bare_notices().is_empty());
        t.watch_bare(BARE_GRACE_MS);
        assert_eq!(t.bare_rows(m.id).len(), 1);
        assert_eq!(t.bare_rows(m.id)[0].symbol, "BTCUSDT");
        assert_eq!(t.take_bare_notices(), vec![m.id]);
        t.watch_bare(BARE_GRACE_MS * 2);
        assert_eq!(t.bare_rows(m.id).len(), 1, "the banner stays");
        assert!(t.take_bare_notices().is_empty(), "told once");
    }

    #[test]
    fn going_stale_says_nothing_and_does_not_forget_who_was_told() {
        let (mut t, m, key) = running();
        connect_lists(&mut t, &m, key, &[], &[held("BTCUSDT", 2)]);
        t.watch_bare(0);
        t.watch_bare(BARE_GRACE_MS);
        assert_eq!(t.take_bare_notices(), vec![m.id]);
        // An algo-order event makes the lists stale: no verdict, no banner, no notice.
        t.accounts.get_mut(&m.id).unwrap().orders_dirty = true;
        assert!(t.lists_stale(m.id));
        t.watch_bare(BARE_GRACE_MS * 2);
        assert!(t.bare_rows(m.id).is_empty());
        assert!(t.take_bare_notices().is_empty());
        // Fresh again with no stop: going stale did not restart the clock, the row is back at
        // once and nobody is told twice.
        connect_lists(&mut t, &m, key, &[], &[held("BTCUSDT", 2)]);
        t.watch_bare(BARE_GRACE_MS * 3);
        assert_eq!(t.bare_rows(m.id).len(), 1);
        assert!(t.take_bare_notices().is_empty());
        // A fresh verdict with a stop resets it; bare again later is told again.
        connect_lists(
            &mut t,
            &m,
            key,
            &[algo_row("7", exchange::trade::ProtectRole::Stop, None)],
            &[held("BTCUSDT", 2)],
        );
        t.watch_bare(BARE_GRACE_MS * 5);
        assert!(t.bare_rows(m.id).is_empty());
        connect_lists(&mut t, &m, key, &[], &[held("BTCUSDT", 2)]);
        t.watch_bare(BARE_GRACE_MS * 6);
        t.watch_bare(BARE_GRACE_MS * 7);
        assert_eq!(t.take_bare_notices(), vec![m.id]);
    }

    #[test]
    fn a_position_that_is_gone_resets_the_notice() {
        let (mut t, m, key) = running();
        connect_lists(&mut t, &m, key, &[], &[held("BTCUSDT", 2)]);
        t.watch_bare(0);
        t.watch_bare(BARE_GRACE_MS);
        assert_eq!(t.take_bare_notices().len(), 1);
        connect_lists(&mut t, &m, key, &[], &[]);
        t.watch_bare(BARE_GRACE_MS * 2);
        assert!(t.bare_rows(m.id).is_empty());
        connect_lists(&mut t, &m, key, &[], &[held("BTCUSDT", 1)]);
        t.watch_bare(BARE_GRACE_MS * 3);
        t.watch_bare(BARE_GRACE_MS * 4);
        assert_eq!(t.take_bare_notices().len(), 1);
    }

    #[test]
    fn a_take_profit_only_position_is_shown_with_lower_severity_and_not_notified() {
        let (mut t, m, key) = running();
        connect_lists(
            &mut t,
            &m,
            key,
            &[algo_row("8", exchange::trade::ProtectRole::Target, None)],
            &[held("BTCUSDT", 2)],
        );
        t.watch_bare(0);
        t.watch_bare(BARE_GRACE_MS);
        assert_eq!(t.bare_rows(m.id).len(), 1);
        assert!(t.bare_rows(m.id)[0].lower_severity());
        assert!(t.take_bare_notices().is_empty());
        assert!(
            t.bare_rows(m.id)[0]
                .text("main")
                .contains("take-profit orders but no stop-loss")
        );
    }

    #[test]
    fn a_pending_entry_does_not_hide_a_position_that_has_no_stop() {
        use crate::auto_flow::tests::{entry_for, spec};
        let (mut t, m, key) = running();
        connect_lists(&mut t, &m, key, &[], &[held("BTCUSDT", 2)]);
        let (entry_id, init) = entry_for(m.id, spec(1, false));
        assert!(t.register_attach(
            entry_id,
            PendingAttach {
                account: m.id,
                symbol: "BTCUSDT".into(),
                plan: crate::protect_attach::AttachPlan {
                    kind: crate::protect_attach::AttachKind::AfterFill,
                    dir: Dir::Long,
                    limit: false,
                    spec: spec(1, false),
                    lines: Vec::new(),
                },
            },
            init,
        ));
        t.settle_attach(m.id, "BTCUSDT", &SendResult::Accepted { order_id: None });
        // The overlay says "not active yet" on the position row, but the verdict comes from the
        // lists: after the grace time the position is bare and the user is told once.
        assert_eq!(
            t.protection(m.id)[0].protection,
            exchange::trade::Protection::EntryPending
        );
        t.watch_bare(0);
        assert!(t.bare_rows(m.id).is_empty(), "inside the grace time");
        t.watch_bare(BARE_GRACE_MS);
        assert_eq!(t.bare_rows(m.id).len(), 1);
        assert_eq!(t.take_bare_notices(), vec![m.id]);
    }

    #[test]
    fn a_stale_account_has_no_bare_rows_and_no_strays() {
        let (mut t, m, _key) = running();
        t.watch_bare(0);
        t.watch_bare(BARE_GRACE_MS * 10);
        assert!(t.lists_stale(m.id));
        assert!(t.bare_rows(m.id).is_empty());
        assert!(
            t.stray_orders(m.id).is_none(),
            "cannot tell is not none-found"
        );
        assert!(t.protective_lines(m.id, "BTCUSDT", Dir::Long).is_empty());
    }

    fn stray_table(orders: &[OpenOrder], positions: &[Position], okx: bool) -> Vec<StrayOrder> {
        stray_orders(positions, orders, false, okx, &|c| {
            c == "tOWNEDOWNEDOWNEDOWNE"
        })
        .expect("fresh lists")
    }

    #[test]
    fn strays_are_orphans_and_unowned_orders_of_the_apps_shape() {
        use exchange::trade::ProtectRole::{Stop, Target};
        let long = [held("BTCUSDT", 2)];
        // One-way: a stop with a position is no stray; a take-profit for a symbol that is not
        // held is an orphan.
        let mut other = algo_row("9", Target, None);
        other.symbol = "ETHUSDT".into();
        let got = stray_table(&[algo_row("7", Stop, None), other], &long, false);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "9");
        assert!(!got[0].residual && got[0].can_cancel);
        assert!(got[0].text.contains("no position to protect"));
        // Unowned, app-shaped, and the whole-position stop of a position still held: it is only
        // told about (no cancel), as it may be the very stop that protects the position.
        let got = stray_table(
            &[
                algo_row("1", Stop, Some(APP_ID)),
                algo_row("2", Stop, Some("tOWNEDOWNEDOWNEDOWNE")),
                algo_row("3", Stop, Some("web_1")),
            ],
            &long,
            false,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "1");
        assert!(got[0].residual && got[0].guards_position);
        assert!(
            !got[0].can_cancel,
            "the stop of a held position is not offered for cancel"
        );
        assert!(
            got[0]
                .text
                .contains("stop-loss of your long BTCUSDT position")
        );
        assert!(got[0].text.contains("keep it unless you replace it"));
        // Unowned, app-shaped take-profit on a held symbol: still cancellable, with the warning.
        let got = stray_table(&[algo_row("4", Target, Some(APP_ID))], &long, false);
        assert_eq!(got.len(), 1);
        assert!(got[0].residual && got[0].can_cancel && !got[0].guards_position);
        assert!(
            got[0]
                .text
                .starts_with("BTCUSDT: this looks like an automatic order")
        );
        assert!(
            got[0]
                .text
                .ends_with("Cancelling it changes the protection of the BTCUSDT position.")
        );
        // The same order with no position on the symbol has no such warning.
        let mut eth = algo_row("5", Target, Some(APP_ID));
        eth.symbol = "ETHUSDT".into();
        let got = stray_table(&[eth], &long, false);
        assert!(!got[0].text.contains("changes the protection"));
        // Real text has no run of blanks (a continued string literal once left 20 of them).
        assert!(!got[0].text.contains("  "));
        // An order that is both an orphan and app-shaped is listed once, as an orphan.
        let got = stray_table(&[algo_row("1", Stop, Some(APP_ID))], &[], false);
        assert_eq!(got.len(), 1);
        assert!(!got[0].residual);
        // OKX attached orders have the app's ids too and are not judged.
        assert!(stray_table(&[algo_row("1", Stop, Some(APP_ID))], &long, true).is_empty());
        // An OKX orphan whose algo type is unknown cannot be cancelled from here.
        let got = stray_table(&[algo_row("1", Stop, None)], &[], true);
        assert!(!got[0].can_cancel);
        // Stale: nothing is concluded.
        assert!(
            stray_orders(
                &long,
                &[algo_row("1", Stop, Some(APP_ID))],
                true,
                false,
                &|_| false
            )
            .is_none()
        );
    }

    #[test]
    fn a_stop_that_guards_a_held_position_is_not_a_stray_to_cancel() {
        use exchange::trade::ProtectRole::Stop;
        let (mut t, m, key) = running();
        connect_lists(
            &mut t,
            &m,
            key,
            &[algo_row("1", Stop, Some(APP_ID))],
            &[held("BTCUSDT", 2)],
        );
        let list = t.stray_orders(m.id).expect("fresh lists");
        assert_eq!(list.len(), 1);
        assert!(!list[0].can_cancel && !list[0].text.contains("  "));
        assert!(!t.is_stray(m.id, "1"), "judged again at click and confirm");
    }

    #[test]
    fn hedge_strays_follow_the_leg() {
        use exchange::trade::ProtectRole::Stop;
        let mut short_held = held("BTCUSDT", 1);
        short_held.dir = Dir::Short;
        short_held.hedge_side = true;
        let mut long_stop_hedge = algo_row("7", Stop, None);
        long_stop_hedge.position_side = Some(Dir::Long);
        // Only the short leg is held: the long leg's stop is an orphan.
        let got = stray_table(&[long_stop_hedge.clone()], &[short_held.clone()], false);
        assert_eq!(got.len(), 1);
        // Both legs held: not an orphan.
        let mut long_held = held("BTCUSDT", 1);
        long_held.hedge_side = true;
        assert!(stray_table(&[long_stop_hedge], &[long_held, short_held], false).is_empty());
    }

    #[test]
    fn an_app_shaped_order_the_session_sent_is_not_a_stray() {
        let (mut t, m, key) = running();
        let orders = [algo_row(
            "1",
            exchange::trade::ProtectRole::Target,
            Some(APP_ID),
        )];
        connect_lists(&mut t, &m, key, &orders, &[held("BTCUSDT", 2)]);
        assert_eq!(t.stray_orders(m.id).unwrap().len(), 1);
        assert!(t.is_stray(m.id, "1"));
        t.note_sent_ids(&[APP_ID]);
        assert!(t.stray_orders(m.id).unwrap().is_empty());
        assert!(!t.is_stray(m.id, "1"));
    }

    #[test]
    fn a_stray_cancel_is_judged_again_from_its_mark() {
        let (mut t, m, key) = running();
        assert!(!t.has_stray_mark(m.id, "1"));
        t.mark_stray_cancel(m.id, "1");
        assert!(t.has_stray_mark(m.id, "1"));
        let _ = key;
        assert!(t.take_stray_mark(m.id, "1"));
        assert!(!t.has_stray_mark(m.id, "1"));
        // A closed confirmation leaves no mark behind (S5).
        t.mark_stray_cancel(m.id, "1");
        t.clear_stray_marks();
        assert!(!t.has_stray_mark(m.id, "1"));
    }

    #[test]
    fn protective_orders_are_listed_per_position_only_on_fresh_lists() {
        use exchange::trade::ProtectRole::{Stop, Target, Trailing};
        let (mut t, m, key) = running();
        let mut trailing = algo_row("3", Trailing, None);
        trailing.trigger_price = None;
        trailing.close_position = false;
        trailing.reduce_only = true;
        trailing.qty = Some(Decimal::from_int(1));
        connect_lists(
            &mut t,
            &m,
            key,
            &[
                algo_row("1", Stop, None),
                algo_row("2", Target, None),
                trailing,
            ],
            &[held("BTCUSDT", 2)],
        );
        let lines = t.protective_lines(m.id, "BTCUSDT", Dir::Long);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].starts_with("Stop-loss: triggers at 59000"));
        assert!(lines[1].starts_with("Take-profit: triggers at 59000"));
        assert!(lines[2].contains("not counted as a stop-loss"));
        assert!(t.protective_lines(m.id, "BTCUSDT", Dir::Short).is_empty());
        t.accounts.get_mut(&m.id).unwrap().orders_dirty = true;
        assert!(t.protective_lines(m.id, "BTCUSDT", Dir::Long).is_empty());
    }

    #[test]
    fn the_bare_banner_names_the_account_symbol_and_side() {
        use exchange::trade::Protection::{PartialStop, TrailingOnly, Unprotected};
        let row = |protection| BareRow {
            symbol: "BTCUSDT".into(),
            dir: Dir::Short,
            protection,
        };
        assert_eq!(
            row(Unprotected).text("main"),
            "main: the short position on BTCUSDT has no stop-loss."
        );
        assert!(
            row(PartialStop)
                .text("a")
                .contains("may not cover the whole position")
        );
        assert!(
            row(TrailingOnly)
                .text("a")
                .contains("may not be active yet")
        );
        assert!(!row(Unprotected).lower_severity());
    }

    #[test]
    fn okx_isolated_margin_is_a_mismatch_but_an_unknown_mode_is_not() {
        use exchange::trade::MarginMode;
        let mut t = booted();
        let m = meta("okx", AccountVenue::Okx, AccountProduct::UsdM);
        let _ = t.on_tested(&m, true, signed());
        let key = t.accounts[&m.id].current_key().expect("a running stream");
        let mut pos = held("BTC-USDT-SWAP", 2);
        connect_lists(&mut t, &m, key, &[], std::slice::from_ref(&pos));
        assert!(
            !t.margin_conflict(m.id, "BTC-USDT-SWAP"),
            "unknown is not a mismatch"
        );
        pos.margin = Some(MarginMode::Cross);
        connect_lists(&mut t, &m, key, &[], std::slice::from_ref(&pos));
        assert!(!t.margin_conflict(m.id, "BTC-USDT-SWAP"));
        pos.margin = Some(MarginMode::Isolated);
        connect_lists(&mut t, &m, key, &[], std::slice::from_ref(&pos));
        assert!(t.margin_conflict(m.id, "BTC-USDT-SWAP"));
        assert!(!t.margin_conflict(m.id, "ETH-USDT-SWAP"));
        // A position event keeps the margin mode the snapshot read.
        t.accounts.get_mut(&m.id).unwrap().apply_position_event(
            "BTC-USDT-SWAP",
            PositionSide::Both,
            Decimal::from_int(3),
            None,
        );
        assert!(t.margin_conflict(m.id, "BTC-USDT-SWAP"));
    }

    #[test]
    fn a_reconnect_matches_the_open_entry_by_client_id_when_the_row_has_one() {
        use crate::auto_flow::tests::{entry_for, spec};
        use exchange::trade::{OpenKind, Side};
        let build = |client: Option<&str>| {
            let (mut t, m, key) = running();
            let (entry_id, init) = entry_for(m.id, spec(1, false));
            assert!(t.register_attach(
                entry_id.clone(),
                PendingAttach {
                    account: m.id,
                    symbol: "BTCUSDT".into(),
                    plan: crate::protect_attach::AttachPlan {
                        kind: crate::protect_attach::AttachKind::AfterFill,
                        dir: Dir::Long,
                        limit: true,
                        spec: spec(1, false),
                        lines: Vec::new(),
                    },
                },
                init,
            ));
            t.settle_attach(m.id, "BTCUSDT", &SendResult::Accepted { order_id: None });
            let mut open = long_stop();
            open.kind = OpenKind::Plain;
            open.role = None;
            open.close_position = false;
            open.side = Some(Side::Buy);
            open.client_id = match client {
                Some("same") => Some(entry_id),
                Some(other) => Some(other.to_string()),
                None => None,
            };
            connect_lists(&mut t, &m, key, &[open], &[]);
            let mut auto = std::mem::take(&mut t.auto);
            TradingState::check_after_reconnect(&t.accounts[&m.id], &mut auto, m.id);
            let told = !auto.notes_for(m.id).is_empty();
            t.auto = auto;
            told
        };
        assert!(!build(Some("same")), "the entry is still open");
        assert!(
            !build(None),
            "a row with no client id keeps the symbol and side match"
        );
        assert!(
            build(Some("eOTHEROTHEROTHEROTHE")),
            "another order is not this entry"
        );
    }
}
