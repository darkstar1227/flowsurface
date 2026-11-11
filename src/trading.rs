//! Trading state of the GUI (M5 c2b-1): startup, private streams, account state, standing
//! alerts. **No order is sent from here**: nothing in this module calls `submit`, `cancel_all`,
//! `flatten` or `auto_close`, and the only requests it makes are signed reads (equity).
//!
//! Everything is held in memory. Nothing of it is written to `saved-state.json` (condition
//! 78); the risk settings that are saved live in `AccountMeta::risk`.
//!
//! The account's private stream is created only for an account whose connection test passed in
//! this session, and it is kept in [`TradingState`] (not derived from the session) so that a
//! terminal state such as `AuthFailed` is not lost when the session is voided.

use crate::widget::{BannerKind, banner};
use data::config::exchange_accounts::{AccountMeta, load_credentials};
use data::config::risk::AccountRiskSettings;
use data::config::trade_gate::TradeSession;
use data::config::unknown_store::UnknownStore;
use exchange::account::{
    AccountEnv, AccountKey, AccountProduct, AccountVenue, ProxyState, check_proxy_snapshot,
};
use exchange::signed_http::SignedHttp;
use exchange::trade::stream::{
    CredentialsUnavailable, PositionSide, PrivateEvent, StreamCloser, StreamContext, StreamKey,
    StreamOutput, StreamState, run,
};
use exchange::trade::{
    Decimal, EquityBasis, EquityFreshness, EquityObservation, OpenOrder, Position, RestoreError,
    SharedRisk, StreamEquityFeed, TradeClient, UnknownRecord, block_all_entries, lock_from_record,
};
use iced::futures::{Stream, StreamExt, stream};
use iced::{Element, Subscription, Task};
use std::collections::{BTreeMap, HashSet, VecDeque};
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
/// and the boot report holds no client id.
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

/// A saved record as the user may see it: no client id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordView {
    pub account: AccountKey,
    pub symbol: String,
    /// `Some`: the record could not be turned into a lock by symbol. It is kept, and entries
    /// of its account stay blocked until restart.
    pub error: Option<RestoreError>,
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
                .map(|r| RecordView {
                    account: r.account,
                    symbol: r.symbol.clone(),
                    error: lock(r).err(),
                })
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
    /// Absolute size.
    pub qty: Decimal,
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
    positions: Vec<HeldPosition>,
    seen_trades: HashSet<String>,
    seen_order: VecDeque<String>,
    fills_seen: u64,
    /// The stream voided the session, or the last test failed.
    needs_retest: bool,
    risk_unavailable: bool,
    equity_inflight: bool,
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
    /// apart, so it counts every time.
    fn note_fill(&mut self, trade_id: Option<&str>) -> bool {
        if let Some(id) = trade_id {
            if !self.seen_trades.insert(id.to_string()) {
                return false;
            }
            self.seen_order.push_back(id.to_string());
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
                        exchange::trade::Dir::Long => PositionSide::Long,
                        exchange::trade::Dir::Short => PositionSide::Short,
                    }
                } else {
                    PositionSide::Both
                },
                qty: p.qty,
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
        self.set_positions_from_snapshot(positions);
        let notionals = position_notionals(&self.positions);
        let r = apply(&self.risk, notionals);
        self.note_poisoned(r);
    }

    /// A position event: the only thing besides a snapshot that changes the held positions (a
    /// fill never adds to them).
    fn apply_position_event(&mut self, symbol: &str, side: PositionSide, qty: Decimal) {
        self.positions
            .retain(|p| !(p.symbol == symbol && p.side == side));
        if qty.is_zero() {
            return;
        }
        // One-way size is signed on the wire; only the absolute size is held.
        let abs = qty.checked_abs().unwrap_or(qty);
        self.positions.push(HeldPosition {
            symbol: symbol.to_string(),
            side,
            qty: abs,
        });
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
    /// A send reported that its journal could not be written (c2b-2 sets it).
    journal_failed: bool,
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
            Message::EquityTick => self.poll_equity(),
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
            StreamOutput::State(state) => rt.state = Some(state),
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
            StreamOutput::Event(event) => Self::apply_event(rt, &event, now_ms),
        }
    }

    fn apply_event(rt: &mut AccountRuntime, event: &PrivateEvent, now_ms: i64) {
        if let Some(obs) = rt.feed.on_event(event, now_ms) {
            let r = rt.risk.observe(obs);
            rt.note_poisoned(r);
        }
        match event {
            PrivateEvent::Fill(fill) => {
                // A record of what happened; it never changes the held positions.
                rt.note_fill(fill.trade_id.as_deref());
            }
            PrivateEvent::Position(update) => {
                rt.apply_position_event(&update.symbol, update.position_side, update.qty);
                let notionals = position_notionals(&rt.positions);
                let r = rt.risk.set_positions(now_ms, notionals);
                rt.note_poisoned(r);
            }
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
/// popouts. A pure function of the state, so a table of states can be tested.
pub fn standing_alerts(state: &TradingState, session: &TradeSession) -> Vec<Alert> {
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
            let who = state
                .accounts
                .values()
                .find(|rt| rt.account() == rec.account)
                .map_or("an account", |rt| rt.meta.alias.as_str());
            out.push(match rec.error {
                Some(_) => alert(
                    Severity::Error,
                    format!(
                        "A saved unresolved order of {who} ({}) has an invalid id and was kept: \
                         all new entries of this account are blocked until the app restarts.",
                        rec.symbol
                    ),
                ),
                None => alert(
                    Severity::Warning,
                    format!(
                        "An unresolved order of {who} ({}) was restored: this symbol stays \
                         locked for new entries.",
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

    for rt in state.accounts.values() {
        let who = rt.meta.alias.as_str();
        if rt.needs_retest || (session.get(rt.meta.id).is_none() && rt.state.is_some()) {
            out.push(alert(
                Severity::Warning,
                format!("{who}: not tested in this session. Trading is off until you test it."),
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
        let alerts = standing_alerts(&t, &session_with(&m));
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
        let alerts = standing_alerts(&t, &TradeSession::default());
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
            rt.note_fill(Some(&i.to_string()));
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
            texts(&standing_alerts(&t, &TradeSession::default()))
                .contains("could not be read: all new entries are blocked")
        );

        t.on_booted(BootReport {
            journal_unreadable: false,
            records: vec![
                RecordView {
                    account: AccountKey::from_bytes([9; 16]),
                    symbol: "BTCUSDT".into(),
                    error: None,
                },
                RecordView {
                    account: AccountKey::from_bytes([9; 16]),
                    symbol: "ETHUSDT".into(),
                    error: Some(RestoreError::BadClientId),
                },
            ],
        });
        let alerts = standing_alerts(&t, &TradeSession::default());
        let all = texts(&alerts);
        assert!(all.contains("BTCUSDT) was restored"));
        assert!(all.contains("ETHUSDT) has an invalid id"));
        assert!(alerts.iter().any(|a| a.severity == Severity::Error));
        // The text never carries a client id (the report has none).
        assert!(!all.contains(SECRET_CID));
    }

    #[test]
    fn alert_for_a_damaged_order_journal() {
        let mut t = booted();
        assert!(!texts(&standing_alerts(&t, &TradeSession::default())).contains("journal"));
        t.journal_failed = true;
        assert!(
            texts(&standing_alerts(&t, &TradeSession::default())).contains("journal is damaged")
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
            let alerts = standing_alerts(&t, &session);
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
        let all = texts(&standing_alerts(&t, &session_with(&m)));
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
        let all = texts(&standing_alerts(&t, &session_with(&m)));
        assert!(all.contains("risk state is unavailable"));
    }

    #[test]
    fn alert_when_no_risk_limit_is_set_and_none_once_one_is() {
        let (mut t, m, _) = running();
        let session = session_with(&m);
        assert!(texts(&standing_alerts(&t, &session)).contains("main: no risk limits set"));
        with_limits(&mut t, &m);
        assert!(!texts(&standing_alerts(&t, &session)).contains("no risk limits set"));
    }

    #[test]
    fn alert_for_spot_and_futures_of_one_environment() {
        let mut t = booted();
        let spot = meta("s", AccountVenue::Binance, AccountProduct::Spot);
        let fut = meta("f", AccountVenue::Binance, AccountProduct::UsdM);
        let _ = t.on_tested(&spot, true, signed());
        assert!(
            !texts(&standing_alerts(&t, &TradeSession::default())).contains("share one exchange")
        );
        let _ = t.on_tested(&fut, true, signed());
        assert!(
            texts(&standing_alerts(&t, &TradeSession::default())).contains("share one exchange")
        );
    }

    #[test]
    fn alerts_are_pure_state_and_empty_without_accounts_or_boot_trouble() {
        let t = booted();
        assert!(standing_alerts(&t, &TradeSession::default()).is_empty());
    }

    #[test]
    fn the_only_trade_calls_here_are_reads() {
        // The send paths are not used by this module (c2b-1 has no order path).
        let src = include_str!("trading.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        for call in [".submit(", ".cancel_all(", ".auto_close(", "submit_auto("] {
            assert!(!code.contains(call), "{call} must not appear in trading.rs");
        }
        // `.flatten()` without arguments is `Stream::flatten`; an order flatten takes arguments.
        assert_eq!(
            code.matches(".flatten(").count(),
            code.matches(".flatten()").count(),
            "an order flatten call must not appear in trading.rs"
        );
    }
}
