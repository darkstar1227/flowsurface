//! The I/O layer of a private stream: one supervisor task owns the pure [`Machine`], one
//! session task per connection does the connecting, and REST reads run as short tasks. The
//! listenKey never leaves its session task; the supervisor only learns "up", "events" and
//! "down" (conditions 45, 46, 49).

use super::binance::{
    SPOT_PRIVATE_STREAM_VERIFIED, SpotMessage, futures_stream_url, parse_futures, parse_spot,
    spot_subscribe_frame, spot_ws_api_url,
};
use super::events::{MAX_FRAME_BYTES, ParseError, PrivateEvent, ResyncReason};
use super::key_claims;
use super::machine::{Command, DownCause, InvalidReason, Machine, StreamState};
use super::okx::{
    OkxLeg, OkxMessage, is_clock_error, leg_channels, login_frame, okx_ws_timestamp, parse_okx,
    subscribe_frame, ws_url,
};
use super::secret::StreamKey;
use super::status;
use crate::account::{
    AccountError, AccountKey, AccountProduct, AccountVenue, ApiCredentials, ProxyState,
    check_proxy_snapshot,
};
use crate::adapter::ws::{HeartbeatPolicy, WsTransport};
use crate::rate_limit::{Denied, Priority, WindowBucket};
use crate::signed_http::SignedHttp;
use crate::trade::client::{Balance, OpenOrder, Position, TradeClient};
use crate::trade::locks;
use fastwebsockets::{Frame, Payload};
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::channel::oneshot;
use secrecy::ExposeSecret;
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

/// The Keychain item could not be loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialsUnavailable;

/// Loads this account's credentials from the Keychain. Called for every connect and every REST
/// read, so a secret is held only while it is used; it blocks, so it runs on a blocking thread.
pub type CredentialLoader =
    Arc<dyn Fn() -> Result<ApiCredentials, CredentialsUnavailable> + Send + Sync>;

/// Everything a private stream needs. Debug prints the identity only.
#[derive(Clone)]
pub struct StreamContext {
    pub key: StreamKey,
    /// The signed client and the proxy it was built with; REST and the WebSocket both follow it.
    pub signed: SignedHttp,
    pub current_proxy: Arc<dyn Fn() -> ProxyState + Send + Sync>,
    pub load_credentials: CredentialLoader,
}

impl std::fmt::Debug for StreamContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamContext")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// What the REST read found. Segments c, d and e take their baseline from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub orders: Vec<OpenOrder>,
    pub positions: Vec<Position>,
    pub balances: Vec<Balance>,
    /// Which snapshot of the stream this is. The consumer passes it to
    /// [`EquityTracker::snapshot_applied`](crate::trade::EquityTracker::snapshot_applied)
    /// when it has applied the snapshot; until then the account's entries are refused.
    pub(crate) epoch: u64,
}

impl Snapshot {
    pub fn sync_epoch(&self) -> u64 {
        self.epoch
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamOutput {
    /// The state changed (also sent once at the start).
    State(StreamState),
    Event(Box<PrivateEvent>),
    /// A REST read finished and the stream is synchronised.
    Snapshot(Box<Snapshot>),
    /// The account's trade session verdict is void (condition 57).
    InvalidateSession(InvalidReason),
}

/// The stream of outputs. Dropping it stops every task of the stream (connections, REST reads,
/// keep-alive); it does **not** close the listenKey, which expires by itself 60 minutes after
/// its last extension. Closing it is [`PrivateStream::shutdown`], for an account that is
/// deleted or disabled.
///
/// Event order: while a REST read runs, the events that arrive are held back. When the read
/// is accepted, [`StreamOutput::Snapshot`] comes first and the held events follow in the order
/// they arrived, so the consumer applies the snapshot and then the later events on top of it.
/// An event that arrived before the read started is sent at once and is older than the
/// snapshot, which replaces it.
pub struct PrivateStream {
    rx: UnboundedReceiver<StreamOutput>,
    task: JoinHandle<()>,
    ctl: UnboundedSender<Msg>,
    unread: Arc<AtomicUsize>,
    /// The entry this stream owns in the status table (see `status`).
    account: AccountKey,
    status_token: u64,
}

impl PrivateStream {
    /// Stop the stream and close the account's listenKey (futures only), then return. The
    /// DELETE is signed with the credentials in the Keychain, so a caller that is deleting the
    /// account must **await this first and only then delete the Keychain item**; the other way
    /// round, the credentials cannot be loaded and the key is left to expire. A stream that
    /// has already stopped (authentication failed, spot) closes nothing.
    pub async fn shutdown(self) {
        let (ack, done) = oneshot::channel();
        if self.ctl.unbounded_send(Msg::Shutdown(ack)).is_ok() {
            let _ = done.await;
        }
    }
}

impl futures::Stream for PrivateStream {
    type Item = StreamOutput;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<StreamOutput>> {
        let polled = self.rx.poll_next_unpin(cx);
        if let Poll::Ready(Some(_)) = &polled {
            // `saturating`: the counter only ever lags, it must not wrap.
            let _ = self
                .unread
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    Some(n.saturating_sub(1))
                });
        }
        polled
    }
}

impl Drop for PrivateStream {
    fn drop(&mut self) {
        self.task.abort();
        // Here, not in the task: an aborted task may still run once more. Removing the entry
        // also fences off any write it makes after this (tokens are checked), so a stream
        // that is gone never leaves the account looking connected.
        status::retire(self.account, self.status_token);
    }
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Shared by the tasks of one stream. Dropping it closes nothing: the account has a single
/// listenKey, so a stream that is replaced (settings edited, generation bumped) must not end
/// the key its successor has just fetched. Only [`PrivateStream::shutdown`] closes it.
struct Shared {
    ctx: StreamContext,
    key_created: AtomicBool,
    clock_offset_ms: AtomicI64,
    /// `Msg::LegEvents` sent by connections and not yet taken by the supervisor.
    pending_legs: AtomicUsize,
    /// The token that lets this stream write its state to the status table; `None` when the
    /// stream is not registered (tests of the pieces).
    status_token: Option<u64>,
    /// This stream's claim on its API key (futures only), held until the stream ends so that no
    /// other app account can open a stream on the same exchange account.
    key_claim: Mutex<Option<key_claims::KeyClaim>>,
}

impl Shared {
    fn new(ctx: StreamContext) -> Self {
        Shared {
            ctx,
            key_created: AtomicBool::new(false),
            clock_offset_ms: AtomicI64::new(0),
            pending_legs: AtomicUsize::new(0),
            status_token: None,
            key_claim: Mutex::new(None),
        }
    }
}

enum Msg {
    LegUp {
        leg: usize,
        epoch: u64,
    },
    LegEvents {
        leg: usize,
        epoch: u64,
        events: Vec<PrivateEvent>,
    },
    LegDown {
        leg: usize,
        epoch: u64,
        cause: DownCause,
    },
    /// The REST read is about to start: events from here on may be newer than its answers.
    ResyncStarted,
    ResyncDone(Result<Snapshot, DownCause>),
    KeepAliveDone(bool),
    /// Close the stream and the listenKey, then answer.
    Shutdown(oneshot::Sender<()>),
}

/// Most `LegEvents` messages that may wait for the supervisor. Past it the connection ends
/// and everything is read again, so a flood cannot grow memory.
const MAX_PENDING_LEG_MSGS: usize = 1_024;
/// Most outputs (and held events) that may wait for the consumer. Past it the events are
/// dropped, the connection ends and everything is read again.
const MAX_UNREAD_OUTPUTS: usize = 4_096;
/// Frames the reader task may queue for the connection loop (a full queue ends the connection).
const READER_CAPACITY: usize = 256;
/// Binance sends a ping every 3 minutes: 3.5 minutes of silence means the socket is dead.
/// (Waiting 10 minutes left a half-open connection looking `Connected` for that long.)
const BINANCE_FUTURES_SILENCE: Duration = Duration::from_secs(210);

/// Start the private stream of one account. Must be called inside a tokio runtime.
pub fn run(ctx: StreamContext) -> PrivateStream {
    let (out_tx, rx) = unbounded();
    let (msg_tx, msg_rx) = unbounded();
    let unread = Arc::new(AtomicUsize::new(0));
    let account = ctx.key.account;
    let status_token = status::register(account);
    let mut shared = Shared::new(ctx);
    shared.status_token = Some(status_token);
    let task = tokio::spawn(supervise(
        Arc::new(shared),
        msg_tx.clone(),
        msg_rx,
        out_tx,
        unread.clone(),
    ));
    PrivateStream {
        rx,
        task,
        ctl: msg_tx,
        unread,
        account,
        status_token,
    }
}

fn leg_count(key: &StreamKey) -> usize {
    match key.venue {
        AccountVenue::Okx => OkxLeg::ALL.len(),
        AccountVenue::Binance => 1,
    }
}

fn uses_listen_key(key: &StreamKey) -> bool {
    key.venue == AccountVenue::Binance && key.product == AccountProduct::UsdM
}

/// Wall-clock jump compared with the monotonic clock between two ticks. A gap far longer than
/// the tick, or a wall clock that ran ahead of the monotonic one, means the machine slept.
pub(crate) fn detect_wake(wall_delta_ms: i64, mono_delta: Duration, threshold: Duration) -> bool {
    let t = i64::try_from(threshold.as_millis()).unwrap_or(i64::MAX);
    let m = i64::try_from(mono_delta.as_millis()).unwrap_or(i64::MAX);
    mono_delta > threshold || wall_delta_ms.saturating_sub(m) > t
}

const TICK: Duration = Duration::from_secs(5);
const WAKE_GAP: Duration = Duration::from_secs(30);

struct Supervisor {
    shared: Arc<Shared>,
    machine: Machine,
    msg_tx: UnboundedSender<Msg>,
    out: UnboundedSender<StreamOutput>,
    /// Outputs sent and not yet read by the consumer.
    unread: Arc<AtomicUsize>,
    epochs: Vec<u64>,
    sessions: Vec<Option<AbortOnDrop>>,
    /// The REST read, the keep-alive and the clock measurement: separate tasks that sign
    /// requests, so they end with the stream.
    resync_task: Option<AbortOnDrop>,
    keepalive_task: Option<AbortOnDrop>,
    clock_task: Option<AbortOnDrop>,
    /// A REST read is running: events are held until it ends (see [`PrivateStream`]).
    reading: bool,
    held: Vec<PrivateEvent>,
    /// The machine asked for the listenKey to be closed (account deleted or disabled).
    delete_key: bool,
    start: Instant,
    stopped: bool,
}

/// Marks the account as not connected when `supervise` ends for any reason, including a panic
/// that unwinds the task (review N2). Without it the status table would keep saying `Connected`
/// until the owner dropped the stream. Only the stream's own token can write.
struct ReconnectingOnExit {
    account: AccountKey,
    token: Option<u64>,
}

impl Drop for ReconnectingOnExit {
    fn drop(&mut self) {
        // Not `publish`: an `AuthFailed` or `Unavailable` stream ended on purpose, and that
        // reason must stay readable (review N6).
        if let Some(token) = self.token {
            status::demote(self.account, token);
        }
    }
}

async fn supervise(
    shared: Arc<Shared>,
    msg_tx: UnboundedSender<Msg>,
    mut msg_rx: UnboundedReceiver<Msg>,
    out: UnboundedSender<StreamOutput>,
    unread: Arc<AtomicUsize>,
) {
    let _guard = ReconnectingOnExit {
        account: shared.ctx.key.account,
        token: shared.status_token,
    };
    let legs = leg_count(&shared.ctx.key);
    let (machine, first) = Machine::start(legs, uses_listen_key(&shared.ctx.key));
    let mut sup = Supervisor::new(shared, machine, msg_tx, out, unread);
    let mut shown = sup.machine.state();
    sup.send(StreamOutput::State(shown));
    sup.apply(first);

    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_wall = chrono::Utc::now().timestamp_millis();
    let mut last_mono = Instant::now();

    while !sup.stopped || shown != sup.machine.state() {
        let cmds = tokio::select! {
            msg = msg_rx.next() => match msg {
                Some(Msg::Shutdown(ack)) => {
                    sup.shut_down().await;
                    sup.send(StreamOutput::State(sup.machine.state()));
                    let _ = ack.send(());
                    break;
                }
                Some(m) => sup.on_msg(m),
                None => break,
            },
            _ = tick.tick() => {
                let (wall, mono) = (chrono::Utc::now().timestamp_millis(), Instant::now());
                let woke = detect_wake(wall - last_wall, mono - last_mono, WAKE_GAP);
                last_wall = wall;
                last_mono = mono;
                sup.machine.observe(sup.start.elapsed());
                if woke { sup.machine.wake() } else { sup.machine.tick(sup.start.elapsed()) }
            }
        };
        sup.apply(cmds);
        let now_state = sup.machine.state();
        if now_state != shown {
            shown = now_state;
            if !sup.send(StreamOutput::State(shown)) {
                break;
            }
        }
        if sup.stopped && shown == sup.machine.state() {
            break;
        }
    }
}

impl Supervisor {
    fn new(
        shared: Arc<Shared>,
        machine: Machine,
        msg_tx: UnboundedSender<Msg>,
        out: UnboundedSender<StreamOutput>,
        unread: Arc<AtomicUsize>,
    ) -> Self {
        let legs = leg_count(&shared.ctx.key);
        Supervisor {
            shared,
            machine,
            msg_tx,
            out,
            unread,
            epochs: vec![0; legs],
            sessions: (0..legs).map(|_| None).collect(),
            resync_task: None,
            keepalive_task: None,
            clock_task: None,
            reading: false,
            held: Vec::new(),
            delete_key: false,
            start: Instant::now(),
            stopped: false,
        }
    }

    /// `false` when the consumer is gone.
    fn send(&self, output: StreamOutput) -> bool {
        // The one place a state leaves the supervisor, so the table and the consumer cannot
        // disagree about what was reported.
        if let (StreamOutput::State(state), Some(token)) = (&output, self.shared.status_token) {
            status::publish(self.shared.ctx.key.account, token, *state);
        }
        self.unread.fetch_add(1, Ordering::Relaxed);
        self.out.unbounded_send(output).is_ok()
    }

    /// Outputs the consumer has not read plus the events held for a snapshot.
    fn backlog(&self) -> usize {
        self.unread.load(Ordering::Relaxed) + self.held.len()
    }

    fn on_msg(&mut self, msg: Msg) -> Vec<Command> {
        self.machine.observe(self.start.elapsed());
        match msg {
            Msg::LegUp { leg, epoch } if self.current(leg, epoch) => {
                self.machine.leg_up(leg, self.start.elapsed())
            }
            Msg::LegDown { leg, epoch, cause } if self.current(leg, epoch) => {
                self.sessions[leg] = None;
                self.machine.leg_down(leg, cause)
            }
            Msg::LegEvents { leg, epoch, events } => {
                let _ = self.shared.pending_legs.fetch_update(
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                    |n| Some(n.saturating_sub(1)),
                );
                if !self.current(leg, epoch) {
                    return Vec::new();
                }
                if self.backlog() >= MAX_UNREAD_OUTPUTS {
                    // The consumer is not keeping up. The events are dropped, so the
                    // connection ends and the REST read that follows replaces them.
                    log::warn!("private stream: output backlog is full, reconnecting");
                    self.sessions[leg] = None;
                    return self.machine.leg_down(leg, DownCause::Network);
                }
                let mut cmds = Vec::new();
                for event in events {
                    if let PrivateEvent::Order(o) = &event
                        && let Some(id) = &o.client_id
                    {
                        // N2-3: an order event with our client id settles an Unknown send.
                        locks::resolve_client_id(self.shared.ctx.key.account, &o.symbol, id);
                    }
                    cmds.extend(self.machine.on_event(&event));
                    if self.reading {
                        self.held.push(event);
                    } else {
                        self.send(StreamOutput::Event(Box::new(event)));
                    }
                }
                cmds
            }
            Msg::ResyncStarted => {
                self.reading = true;
                Vec::new()
            }
            Msg::ResyncDone(result) => {
                self.reading = false;
                let held = std::mem::take(&mut self.held);
                let cmds = match result {
                    Ok(mut snapshot) => {
                        let cmds = self.machine.resync_done(true);
                        if self.machine.state() == StreamState::Connected {
                            // The state turns `Connected` with (or before) this send, but the
                            // consumer applies the snapshot later: the epoch makes entries wait
                            // for that (review N1).
                            if let Some(token) = self.shared.status_token {
                                snapshot.epoch =
                                    status::begin_sync(self.shared.ctx.key.account, token);
                            }
                            // The snapshot first, then what arrived while it was read.
                            self.send(StreamOutput::Snapshot(Box::new(snapshot)));
                        }
                        cmds
                    }
                    Err(cause) => {
                        let mut cmds = Vec::new();
                        match cause {
                            DownCause::Auth | DownCause::Keychain => {
                                // Void the verdict (or stop) first, then release the flag.
                                cmds.extend(self.machine.leg_down(0, cause));
                            }
                            DownCause::ClockSkew => cmds.push(Command::SyncClock),
                            _ => {}
                        }
                        cmds.extend(self.machine.resync_done(false));
                        cmds
                    }
                };
                // With no snapshot to follow, these are still real events: send them as they
                // are; the next read replaces whatever they changed.
                for event in held {
                    self.send(StreamOutput::Event(Box::new(event)));
                }
                cmds
            }
            Msg::KeepAliveDone(ok) => self.machine.keepalive_done(ok, self.start.elapsed()),
            // A message from a connection that was replaced since.
            _ => Vec::new(),
        }
    }

    /// The account was deleted or disabled: stop everything, then close the listenKey and wait
    /// for the answer (the caller deletes the Keychain item only after this returns).
    async fn shut_down(&mut self) {
        let cmds = self.machine.shutdown();
        self.apply(cmds);
        if self.delete_key && self.shared.key_created.load(Ordering::Relaxed) {
            let offset = self.shared.clock_offset_ms.load(Ordering::Relaxed);
            let close = async {
                if let Ok(client) = make_client(&self.shared.ctx, offset).await {
                    let _ = client.listen_key_close().await;
                }
            };
            let _ = tokio::time::timeout(Duration::from_secs(15), close).await;
        }
    }

    fn current(&self, leg: usize, epoch: u64) -> bool {
        self.epochs.get(leg) == Some(&epoch)
    }

    fn apply(&mut self, cmds: Vec<Command>) {
        for cmd in cmds {
            match cmd {
                Command::Connect { leg, after } => {
                    self.epochs[leg] += 1;
                    let epoch = self.epochs[leg];
                    let shared = self.shared.clone();
                    let tx = self.msg_tx.clone();
                    // Replacing the handle aborts the old session.
                    self.sessions[leg] = Some(AbortOnDrop(tokio::spawn(async move {
                        tokio::time::sleep(after).await;
                        let cause = run_leg(&shared, leg, epoch, &tx).await;
                        let _ = tx.unbounded_send(Msg::LegDown { leg, epoch, cause });
                    })));
                }
                Command::Resync { after } => {
                    let shared = self.shared.clone();
                    let tx = self.msg_tx.clone();
                    self.resync_task = Some(AbortOnDrop(tokio::spawn(async move {
                        tokio::time::sleep(after).await;
                        let _ = tx.unbounded_send(Msg::ResyncStarted);
                        let result = read_snapshot(&shared).await;
                        let _ = tx.unbounded_send(Msg::ResyncDone(result));
                    })));
                }
                Command::KeepAlive => {
                    let shared = self.shared.clone();
                    let tx = self.msg_tx.clone();
                    self.keepalive_task = Some(AbortOnDrop(tokio::spawn(async move {
                        let offset = shared.clock_offset_ms.load(Ordering::Relaxed);
                        let ok = match make_client(&shared.ctx, offset).await {
                            Ok(c) => c.listen_key_keepalive().await.is_ok(),
                            Err(_) => false,
                        };
                        let _ = tx.unbounded_send(Msg::KeepAliveDone(ok));
                    })));
                }
                Command::SyncClock => {
                    let shared = self.shared.clone();
                    self.clock_task = Some(AbortOnDrop(tokio::spawn(async move {
                        let offset = shared.clock_offset_ms.load(Ordering::Relaxed);
                        if let Ok(c) = make_client(&shared.ctx, offset).await
                            && let Ok(new) = c.sync_clock().await
                        {
                            shared.clock_offset_ms.store(new, Ordering::Relaxed);
                        }
                    })));
                }
                Command::InvalidateSession(reason) => {
                    self.send(StreamOutput::InvalidateSession(reason));
                }
                Command::Stop => {
                    self.stopped = true;
                    for s in &mut self.sessions {
                        *s = None;
                    }
                    self.resync_task = None;
                    self.keepalive_task = None;
                    self.clock_task = None;
                }
                // Done by `shut_down`, which waits for it; nothing closes a key on its own.
                Command::DeleteKey => self.delete_key = true,
            }
        }
    }
}

// ── Clients and REST ────────────────────────────────────────────────────

async fn load_credentials(ctx: &StreamContext) -> Result<ApiCredentials, DownCause> {
    let load = ctx.load_credentials.clone();
    match tokio::task::spawn_blocking(move || load()).await {
        Ok(Ok(creds)) => Ok(creds),
        _ => Err(DownCause::Keychain),
    }
}

/// A fresh client with credentials loaded for this call only.
async fn make_client(ctx: &StreamContext, clock_offset_ms: i64) -> Result<TradeClient, DownCause> {
    check_proxy_snapshot(ctx.signed.proxy_state(), &(ctx.current_proxy)())
        .map_err(|_| DownCause::Proxy)?;
    let creds = load_credentials(ctx).await?;
    let client = TradeClient::new(
        ctx.signed.clone(),
        ctx.key.venue,
        ctx.key.env,
        ctx.key.product,
        ctx.key.account,
        creds,
        ctx.current_proxy.clone(),
    );
    client.set_clock_offset_ms(clock_offset_ms);
    Ok(client)
}

/// How a failed REST call is treated by the machine.
pub(crate) fn classify_rest(e: &AccountError) -> DownCause {
    match e {
        AccountError::Exchange { code, .. }
            if matches!(code.as_str(), "-2014" | "-2015" | "-1022" | "-2008") =>
        {
            DownCause::Auth
        }
        AccountError::Http(401 | 403) | AccountError::InvalidCredentials => DownCause::Auth,
        AccountError::ClockSkew => DownCause::ClockSkew,
        AccountError::ProxyChanged | AccountError::ProxyBroken => DownCause::Proxy,
        _ => DownCause::Network,
    }
}

fn classify_read(e: &crate::trade::client::ReadError) -> DownCause {
    match e {
        crate::trade::client::ReadError::Transport(a) => classify_rest(a),
        _ => DownCause::Network,
    }
}

async fn read_snapshot(shared: &Shared) -> Result<Snapshot, DownCause> {
    let offset = shared.clock_offset_ms.load(Ordering::Relaxed);
    let client = make_client(&shared.ctx, offset).await?;
    let orders = client.open_orders().await.map_err(|e| classify_read(&e))?;
    let positions = client.positions().await.map_err(|e| classify_read(&e))?;
    let balances = client.balances().await.map_err(|e| classify_read(&e))?;
    Ok(Snapshot {
        orders,
        positions,
        balances,
        epoch: 0,
    })
}

// ── Connection limiter ──────────────────────────────────────────────────

/// OKX allows 3 connection requests per second per IP; the same cap is applied to every host.
const CONNECTS_PER_SECOND: usize = 3;

static CONNECT_LIMITS: LazyLock<Mutex<HashMap<String, WindowBucket>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `Ok` when a connection may start now, otherwise how long to wait.
pub(crate) fn try_slot(bucket: &mut WindowBucket, now: Instant) -> Result<(), Duration> {
    match bucket.acquire(now, 1, Priority::Protective) {
        Ok(()) => Ok(()),
        Err(Denied::Wait(d)) => Err(d),
        Err(_) => Err(Duration::from_secs(1)),
    }
}

async fn connect_slot(host: &str) {
    loop {
        let wait = {
            let mut map = CONNECT_LIMITS.lock().unwrap_or_else(|p| p.into_inner());
            let bucket = map
                .entry(host.to_string())
                .or_insert_with(|| WindowBucket::new(CONNECTS_PER_SECOND, Duration::from_secs(1)));
            try_slot(bucket, Instant::now())
        };
        match wait {
            Ok(()) => return,
            Err(d) => tokio::time::sleep(d.max(Duration::from_millis(50))).await,
        }
    }
}

// ── One connection ──────────────────────────────────────────────────────

/// What one login answer means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginStep {
    Wait,
    Done,
    Fail(DownCause),
}

/// Judge one frame received while logging in. Logs the error code only (cleaned); the frame
/// itself, which may echo credentials, is never logged (condition 46).
pub(crate) fn login_step(frame: &[u8]) -> LoginStep {
    match parse_okx(frame) {
        Ok(OkxMessage::LoginOk) => LoginStep::Done,
        Ok(OkxMessage::Error { code }) => {
            log::warn!("okx login refused (code {code})");
            if is_clock_error(&code) {
                LoginStep::Fail(DownCause::ClockSkew)
            } else {
                LoginStep::Fail(DownCause::Auth)
            }
        }
        Ok(OkxMessage::Notice) => LoginStep::Fail(DownCause::Network),
        Ok(_) => LoginStep::Wait,
        Err(_) => LoginStep::Fail(DownCause::Network),
    }
}

/// The whole handshake of one connection (OKX login plus subscribe, Binance spot subscribe)
/// must finish within this, however many frames the server sends meanwhile.
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(15);
/// One handshake read waits at most this long.
const HANDSHAKE_READ: Duration = Duration::from_secs(10);
/// Events a server may push before the last subscription is confirmed. They are kept (a
/// dropped event would leave the state after the snapshot incomplete), so the count is bounded.
const MAX_EARLY_EVENTS: usize = 512;

/// How long one handshake read may wait, or `None` once the deadline has passed.
pub(crate) fn remaining(deadline: Instant, now: Instant) -> Option<Duration> {
    let left = deadline.checked_duration_since(now)?;
    (!left.is_zero()).then(|| left.min(HANDSHAKE_READ))
}

/// One handshake frame, read within what is left of the deadline. A passed deadline is a
/// network failure (reconnect with backoff), never an authentication failure.
async fn read_handshake(t: &mut WsTransport, deadline: Instant) -> Result<Vec<u8>, DownCause> {
    let wait = remaining(deadline, Instant::now()).ok_or(DownCause::Network)?;
    t.read_text(wait).await.map_err(|_| DownCause::Network)
}

/// How far an OKX leg's subscriptions have got. The leg is up only when every channel it asked
/// for is confirmed: an unconfirmed channel would leave the stream looking healthy and blind.
pub(crate) struct SubscribeProgress {
    expected: &'static [&'static str],
    confirmed: HashSet<String>,
    /// Events that arrived before the last confirmation; they are passed on once the leg is up.
    pub(crate) early: Vec<PrivateEvent>,
}

impl SubscribeProgress {
    pub(crate) fn new(leg: OkxLeg, product: AccountProduct) -> Self {
        Self {
            expected: leg_channels(leg, product),
            confirmed: HashSet::new(),
            early: Vec::new(),
        }
    }

    pub(crate) fn complete(&self) -> bool {
        self.expected.iter().all(|c| self.confirmed.contains(*c))
    }

    /// Take one frame. `Some(cause)`: the leg must end. A refused subscription is not retried
    /// (the same request would be refused again and every retry logs in again, against the
    /// login limit): the stream becomes unavailable, which is fail-closed. A wrong clock is
    /// the exception, as it mends.
    pub(crate) fn feed(&mut self, frame: &[u8]) -> Option<DownCause> {
        match parse_okx(frame) {
            Ok(OkxMessage::Subscribed { channel }) => {
                // A name we did not ask for confirms nothing.
                if self.expected.contains(&channel.as_str()) {
                    self.confirmed.insert(channel);
                }
                None
            }
            Ok(OkxMessage::Error { code }) => {
                log::warn!("okx subscribe refused (code {code})");
                Some(if is_clock_error(&code) {
                    DownCause::ClockSkew
                } else {
                    DownCause::Unavailable
                })
            }
            Ok(OkxMessage::Notice) => Some(DownCause::Expired),
            Ok(OkxMessage::Events(events)) => {
                if self.early.len().saturating_add(events.len()) > MAX_EARLY_EVENTS {
                    return Some(DownCause::Network);
                }
                self.early.extend(events);
                None
            }
            Ok(_) => None,
            Err(_) => Some(DownCause::Network),
        }
    }
}

fn coarse(e: &ParseError) -> &'static str {
    match e {
        ParseError::TooLarge => "too large",
        ParseError::NotJson => "not json",
        ParseError::Shape => "unexpected shape",
    }
}

async fn run_leg(shared: &Shared, leg: usize, epoch: u64, tx: &UnboundedSender<Msg>) -> DownCause {
    let ctx = &shared.ctx;
    let key = ctx.key;
    let spot = key.venue == AccountVenue::Binance && key.product == AccountProduct::Spot;
    if spot && !SPOT_PRIVATE_STREAM_VERIFIED {
        return DownCause::Unavailable;
    }
    if let Err(_e) = check_proxy_snapshot(ctx.signed.proxy_state(), &(ctx.current_proxy)()) {
        return DownCause::Proxy;
    }
    let proxy = match ctx.signed.proxy_state() {
        ProxyState::Direct => None,
        ProxyState::Proxied(p) => Some(p.clone()),
        ProxyState::Broken => return DownCause::Proxy,
    };
    let creds = match load_credentials(ctx).await {
        Ok(c) => c,
        Err(c) => return c,
    };
    let offset = shared.clock_offset_ms.load(Ordering::Relaxed);
    let okx_leg = OkxLeg::ALL.get(leg).copied().unwrap_or(OkxLeg::Private);

    // The URL of this connection (and the listenKey inside it, for futures).
    let url = match key.venue {
        AccountVenue::Okx => ws_url(key.env, key.product, okx_leg),
        AccountVenue::Binance if spot => spot_ws_api_url(key.env),
        AccountVenue::Binance => {
            // One stream per exchange account: the same key under another app account would
            // share this listenKey and split the locks and limits.
            {
                let mut held = shared.key_claim.lock().unwrap_or_else(|p| p.into_inner());
                // Release a claim on an old key first (the Keychain item was replaced).
                *held = None;
                match key_claims::claim(
                    key.account,
                    shared.status_token.unwrap_or(0),
                    creds.key().expose_secret(),
                ) {
                    Some(c) => *held = Some(c),
                    None => {
                        log::warn!(
                            "private stream: this API key is already in use by another account"
                        );
                        return DownCause::Unavailable;
                    }
                }
            }
            let client = TradeClient::new(
                ctx.signed.clone(),
                key.venue,
                key.env,
                key.product,
                key.account,
                ApiCredentials::new(
                    creds.key().expose_secret().to_string(),
                    creds.secret().expose_secret().to_string(),
                    None,
                ),
                ctx.current_proxy.clone(),
            );
            client.set_clock_offset_ms(offset);
            match client.listen_key_create().await {
                Ok(listen_key) => {
                    shared.key_created.store(true, Ordering::Relaxed);
                    futures_stream_url(key.env, &listen_key)
                }
                Err(e) => return classify_rest(&e),
            }
        }
    };
    let url = match url {
        Ok(u) => u,
        Err(_) => return DownCause::Unavailable,
    };
    let host = url.host_str().unwrap_or_default().to_string();

    connect_slot(&host).await;
    let mut transport = match WsTransport::establish(&host, url.as_str(), proxy.as_ref()).await {
        Ok(t) => t,
        Err(_) => {
            log::warn!("private stream: could not connect to {host}");
            return DownCause::Network;
        }
    };
    transport.limit_frame_size(MAX_FRAME_BYTES);
    let deadline = Instant::now() + HANDSHAKE_DEADLINE;

    let now_ms = chrono::Utc::now().timestamp_millis() + offset;
    let mut early: Vec<PrivateEvent> = Vec::new();
    let policy = match key.venue {
        AccountVenue::Okx => {
            let ts = okx_ws_timestamp(now_ms);
            let Some(frame) = login_frame(&creds, &ts) else {
                return DownCause::Auth;
            };
            drop(creds);
            if write_text(&mut transport, frame).await.is_err() {
                return DownCause::Network;
            }
            loop {
                match read_handshake(&mut transport, deadline).await {
                    Ok(bytes) => match login_step(&bytes) {
                        LoginStep::Wait => {}
                        LoginStep::Done => break,
                        LoginStep::Fail(c) => return c,
                    },
                    Err(c) => return c,
                }
            }
            if write_text(&mut transport, subscribe_frame(okx_leg, key.product))
                .await
                .is_err()
            {
                return DownCause::Network;
            }
            let mut progress = SubscribeProgress::new(okx_leg, key.product);
            while !progress.complete() {
                match read_handshake(&mut transport, deadline).await {
                    Ok(bytes) => {
                        if let Some(cause) = progress.feed(&bytes) {
                            return cause;
                        }
                    }
                    Err(c) => return c,
                }
            }
            early = progress.early;
            HeartbeatPolicy::ping_after_idle_text(
                b"ping",
                Duration::from_secs(20),
                Duration::from_secs(10),
            )
        }
        AccountVenue::Binance if spot => {
            let frame = spot_subscribe_frame(
                "1",
                creds.key().expose_secret(),
                creds.secret().expose_secret(),
                now_ms,
            );
            drop(creds);
            if write_text(&mut transport, frame).await.is_err() {
                return DownCause::Network;
            }
            loop {
                match read_handshake(&mut transport, deadline).await {
                    Ok(bytes) => match parse_spot(&bytes) {
                        Ok(SpotMessage::Subscribed) => break,
                        Ok(m @ SpotMessage::Refused { .. }) => {
                            if m.is_auth_failure() {
                                return DownCause::Auth;
                            }
                            if matches!(&m, SpotMessage::Refused { code, .. } if code == "-1021") {
                                return DownCause::ClockSkew;
                            }
                            // Unknown refusal: fail closed.
                            return DownCause::Unavailable;
                        }
                        Ok(SpotMessage::Events(_)) => {}
                        Err(_) => return DownCause::Network,
                    },
                    Err(c) => return c,
                }
            }
            HeartbeatPolicy::server_driven(Duration::from_secs(90))
        }
        AccountVenue::Binance => {
            drop(creds);
            HeartbeatPolicy::server_driven(BINANCE_FUTURES_SILENCE)
        }
    };

    let (mut rx, reader) = transport.spawn_reader(policy, READER_CAPACITY);
    let _reader = AbortOnDrop(reader);
    let _ = tx.unbounded_send(Msg::LegUp { leg, epoch });
    if !early.is_empty() && !send_events(shared, tx, leg, epoch, early) {
        return DownCause::Network;
    }

    loop {
        let bytes = match rx.next().await {
            Some(Ok(b)) => b,
            _ => return DownCause::Network,
        };
        if bytes.is_empty() {
            continue;
        }
        let parsed = match key.venue {
            AccountVenue::Okx => match parse_okx(&bytes) {
                Ok(OkxMessage::Events(e)) => Ok(e),
                Ok(OkxMessage::Notice) => return DownCause::Expired,
                Ok(OkxMessage::Error { code }) => {
                    log::warn!("okx stream error (code {code})");
                    return DownCause::Network;
                }
                Ok(_) => Ok(Vec::new()),
                Err(e) => Err(e),
            },
            AccountVenue::Binance if spot => match parse_spot(&bytes) {
                Ok(SpotMessage::Events(e)) => Ok(e),
                Ok(SpotMessage::Refused { .. }) => return DownCause::Network,
                Ok(SpotMessage::Subscribed) => Ok(Vec::new()),
                Err(e) => Err(e),
            },
            AccountVenue::Binance => parse_futures(&bytes),
        };
        let events = match parsed {
            Ok(e) => e,
            Err(e) => {
                log::warn!("private stream: dropped a frame ({})", coarse(&e));
                vec![PrivateEvent::ResyncRequired(ResyncReason::MalformedFrame)]
            }
        };
        if !events.is_empty() && !send_events(shared, tx, leg, epoch, events) {
            return DownCause::Network;
        }
    }
}

/// Hand events to the supervisor. `false` when it is gone or too many messages are waiting
/// (the connection then ends, and everything is read again after it is back).
fn send_events(
    shared: &Shared,
    tx: &UnboundedSender<Msg>,
    leg: usize,
    epoch: u64,
    events: Vec<PrivateEvent>,
) -> bool {
    if shared.pending_legs.fetch_add(1, Ordering::Relaxed) >= MAX_PENDING_LEG_MSGS {
        shared.pending_legs.fetch_sub(1, Ordering::Relaxed);
        log::warn!("private stream: too many unread messages, reconnecting");
        return false;
    }
    if tx
        .unbounded_send(Msg::LegEvents { leg, epoch, events })
        .is_err()
    {
        shared.pending_legs.fetch_sub(1, Ordering::Relaxed);
        return false;
    }
    true
}

async fn write_text(t: &mut WsTransport, frame: String) -> Result<(), ()> {
    t.write_frame(Frame::text(Payload::Owned(frame.into_bytes())))
        .await
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::tests::{LOGGED, capture_logs};
    use crate::account::{AccountEnv, AccountKey};
    use crate::trade::stream::events::{OrderCategory, OrderStatus, OrderUpdate};

    fn key(venue: AccountVenue, product: AccountProduct) -> StreamKey {
        StreamKey {
            account: AccountKey::from_bytes([7; 16]),
            venue,
            env: AccountEnv::Testnet,
            product,
            generation: 3,
        }
    }

    fn context(venue: AccountVenue, product: AccountProduct) -> StreamContext {
        StreamContext {
            key: key(venue, product),
            signed: SignedHttp::for_test(reqwest::Client::new(), ProxyState::Direct),
            current_proxy: Arc::new(|| ProxyState::Direct),
            load_credentials: Arc::new(|| Err(CredentialsUnavailable)),
        }
    }

    fn supervisor_with(ctx: StreamContext) -> (Supervisor, UnboundedReceiver<StreamOutput>) {
        let k = ctx.key;
        let (machine, _) = Machine::start(leg_count(&k), uses_listen_key(&k));
        let (msg_tx, _msg_rx) = unbounded();
        let (out, rx) = unbounded();
        let mut sup = Supervisor::new(
            Arc::new(Shared::new(ctx)),
            machine,
            msg_tx,
            out,
            Arc::new(AtomicUsize::new(0)),
        );
        sup.epochs = vec![1; leg_count(&k)];
        (sup, rx)
    }

    fn supervisor(venue: AccountVenue, product: AccountProduct) -> Supervisor {
        supervisor_with(context(venue, product)).0
    }

    #[test]
    fn at_most_three_connections_per_second_even_when_many_ask() {
        let base = Instant::now();
        let mut bucket = WindowBucket::new(CONNECTS_PER_SECOND, Duration::from_secs(1));
        let granted = (0..20)
            .filter(|_| try_slot(&mut bucket, base).is_ok())
            .count();
        assert_eq!(granted, 3);
        let wait = try_slot(&mut bucket, base).unwrap_err();
        assert!(wait <= Duration::from_secs(1));
        // After the window the next three are let through.
        let later = base + Duration::from_millis(1100);
        let granted = (0..20)
            .filter(|_| try_slot(&mut bucket, later).is_ok())
            .count();
        assert_eq!(granted, 3);
    }

    #[test]
    fn rest_failures_are_classified() {
        let ex = |c: &str| AccountError::Exchange {
            code: c.into(),
            msg: String::new(),
        };
        for c in ["-2014", "-2015", "-1022", "-2008"] {
            assert_eq!(classify_rest(&ex(c)), DownCause::Auth, "{c}");
        }
        assert_eq!(classify_rest(&AccountError::Http(401)), DownCause::Auth);
        assert_eq!(classify_rest(&AccountError::Http(403)), DownCause::Auth);
        assert_eq!(
            classify_rest(&AccountError::InvalidCredentials),
            DownCause::Auth
        );
        assert_eq!(
            classify_rest(&AccountError::ClockSkew),
            DownCause::ClockSkew
        );
        assert_eq!(classify_rest(&AccountError::ProxyChanged), DownCause::Proxy);
        assert_eq!(classify_rest(&AccountError::ProxyBroken), DownCause::Proxy);
        for e in [
            AccountError::Timeout,
            AccountError::Connect,
            AccountError::Network,
            AccountError::Http(500),
            AccountError::RateLimited(429),
            AccountError::BadResponse,
            ex("-1003"),
        ] {
            assert_eq!(classify_rest(&e), DownCause::Network, "{e:?}");
        }
    }

    #[test]
    fn a_long_gap_or_a_running_ahead_wall_clock_is_a_wake() {
        let gap = Duration::from_secs(30);
        // A normal tick.
        assert!(!detect_wake(5_000, Duration::from_secs(5), gap));
        // The monotonic clock saw the gap too (platforms that count sleep).
        assert!(detect_wake(600_000, Duration::from_secs(600), gap));
        // Only the wall clock moved (platforms whose monotonic clock stops in sleep).
        assert!(detect_wake(600_000, Duration::from_secs(5), gap));
        // A small wall-clock correction is not a wake.
        assert!(!detect_wake(9_000, Duration::from_secs(5), gap));
        // The wall clock stepping back is not a wake.
        assert!(!detect_wake(-60_000, Duration::from_secs(5), gap));
    }

    #[test]
    fn login_steps_follow_the_answer() {
        assert_eq!(
            login_step(br#"{"event":"login","code":"0","msg":""}"#),
            LoginStep::Done
        );
        assert_eq!(
            login_step(br#"{"event":"error","code":"60004","msg":"x"}"#),
            LoginStep::Fail(DownCause::ClockSkew)
        );
        assert_eq!(
            login_step(br#"{"event":"error","code":"60009","msg":"x"}"#),
            LoginStep::Fail(DownCause::Auth)
        );
        assert_eq!(login_step(b"not json"), LoginStep::Fail(DownCause::Network));
        assert_eq!(login_step(br#"{"event":"subscribe"}"#), LoginStep::Wait);
    }

    #[test]
    fn a_refused_login_logs_no_credential() {
        capture_logs();
        let frame = br#"{"event":"error","code":"60009","msg":"Login failed apiKey=FAKEAPIKEY passphrase=FAKEPASS sign=FAKESIGN"}"#;
        assert_eq!(login_step(frame), LoginStep::Fail(DownCause::Auth));
        let logs = LOGGED.lock().unwrap_or_else(|e| e.into_inner()).join("\n");
        assert!(logs.contains("60009"));
        for secret in ["FAKEAPIKEY", "FAKEPASS", "FAKESIGN"] {
            assert!(!logs.contains(secret), "{secret} reached a log");
        }
    }

    #[test]
    fn outputs_and_identity_carry_no_listen_key() {
        let k = key(AccountVenue::Binance, AccountProduct::UsdM);
        let text = format!("{k:?} {:?}", StreamOutput::State(StreamState::Connected));
        assert!(text.contains("generation"));
        assert!(!text.to_lowercase().contains("listenkey"));
        let ctx = supervisor(AccountVenue::Binance, AccountProduct::UsdM)
            .shared
            .ctx
            .clone();
        let text = format!("{ctx:?}");
        assert!(!text.contains("load_credentials") && !text.to_lowercase().contains("secret"));
    }

    #[test]
    fn an_order_event_with_our_client_id_releases_the_unknown_lock_once() {
        let mut sup = supervisor(AccountVenue::Binance, AccountProduct::UsdM);
        let account = sup.shared.ctx.key.account;
        locks::lock_unknown(account, "BTCUSDT", Some("cid-1"));
        assert!(locks::is_locked(account, "BTCUSDT"));
        let order = |id: Option<&str>| {
            PrivateEvent::Order(OrderUpdate {
                symbol: "BTCUSDT".into(),
                client_id: id.map(str::to_string),
                order_id: None,
                category: OrderCategory::Plain,
                side: None,
                status: OrderStatus::New,
                price: None,
                qty: None,
                filled_qty: None,
                reduce_only: None,
                position_side: None,
                time_ms: None,
            })
        };
        // Another id does not release it.
        sup.on_msg(Msg::LegEvents {
            leg: 0,
            epoch: 1,
            events: vec![order(Some("other"))],
        });
        assert!(locks::is_locked(account, "BTCUSDT"));
        // A stale connection's events are dropped.
        sup.on_msg(Msg::LegEvents {
            leg: 0,
            epoch: 0,
            events: vec![order(Some("cid-1"))],
        });
        assert!(locks::is_locked(account, "BTCUSDT"));
        sup.on_msg(Msg::LegEvents {
            leg: 0,
            epoch: 1,
            events: vec![order(Some("cid-1"))],
        });
        assert!(!locks::is_locked(account, "BTCUSDT"));
        // A repeat does not release anything else.
        locks::lock_unknown(account, "BTCUSDT", Some("cid-2"));
        sup.on_msg(Msg::LegEvents {
            leg: 0,
            epoch: 1,
            events: vec![order(Some("cid-1"))],
        });
        assert!(locks::is_locked(account, "BTCUSDT"));
        sup.on_msg(Msg::LegEvents {
            leg: 0,
            epoch: 1,
            events: vec![order(Some("cid-2"))],
        });
        assert!(!locks::is_locked(account, "BTCUSDT"));
    }

    #[test]
    fn keychain_and_auth_failures_void_the_session_and_auth_stops() {
        let mut sup = supervisor(AccountVenue::Okx, AccountProduct::UsdM);
        let cmds = sup.on_msg(Msg::LegDown {
            leg: 0,
            epoch: 1,
            cause: DownCause::Keychain,
        });
        assert!(cmds.contains(&Command::InvalidateSession(
            InvalidReason::KeychainUnavailable
        )));
        let cmds = sup.on_msg(Msg::LegDown {
            leg: 1,
            epoch: 1,
            cause: DownCause::Auth,
        });
        assert!(cmds.contains(&Command::InvalidateSession(InvalidReason::StreamAuthFailed)));
        assert!(cmds.contains(&Command::Stop));
        assert_eq!(sup.machine.state(), StreamState::AuthFailed);
    }

    #[test]
    fn spot_is_unavailable_until_verified() {
        const { assert!(!SPOT_PRIVATE_STREAM_VERIFIED) };
        let shared = Shared::new(context(AccountVenue::Binance, AccountProduct::Spot));
        let (tx, _rx) = unbounded();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let cause = rt.block_on(run_leg(&shared, 0, 1, &tx));
        assert_eq!(cause, DownCause::Unavailable);
    }

    // ── Resync ordering, listenKey lifetime, task lifetime, subscriptions ────

    const T: Duration = Duration::from_secs(100);

    fn drain(rx: &mut UnboundedReceiver<StreamOutput>) -> Vec<StreamOutput> {
        let mut all = Vec::new();
        while let Ok(o) = rx.try_recv() {
            all.push(o);
        }
        all
    }

    fn account_event(symbol: &str) -> PrivateEvent {
        PrivateEvent::Order(OrderUpdate {
            symbol: symbol.into(),
            client_id: None,
            order_id: None,
            category: OrderCategory::Plain,
            side: None,
            status: OrderStatus::Filled,
            price: None,
            qty: None,
            filled_qty: None,
            reduce_only: None,
            position_side: None,
            time_ms: None,
        })
    }

    fn empty_snapshot() -> Snapshot {
        Snapshot {
            orders: Vec::new(),
            positions: Vec::new(),
            balances: Vec::new(),
            epoch: 0,
        }
    }

    fn events_msg(symbol: &str) -> Msg {
        Msg::LegEvents {
            leg: 0,
            epoch: 1,
            events: vec![account_event(symbol)],
        }
    }

    fn is_event(o: &StreamOutput, symbol: &str) -> bool {
        matches!(o, StreamOutput::Event(e) if **e == account_event(symbol))
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[test]
    fn an_event_that_fills_after_the_orders_read_is_applied_after_the_snapshot() {
        let (mut sup, mut rx) =
            supervisor_with(context(AccountVenue::Binance, AccountProduct::UsdM));
        sup.machine.leg_up(0, T);
        // Before the read starts: older than the snapshot, sent at once.
        sup.on_msg(events_msg("BEFORE"));
        sup.on_msg(Msg::ResyncStarted);
        // The order fills after the openOrders answer, before the snapshot is out.
        sup.on_msg(events_msg("DURING1"));
        sup.on_msg(events_msg("DURING2"));
        let now = drain(&mut rx);
        assert_eq!(now.len(), 1, "{now:?}");
        assert!(is_event(&now[0], "BEFORE"));

        sup.on_msg(Msg::ResyncDone(Ok(empty_snapshot())));
        assert_eq!(sup.machine.state(), StreamState::Connected);
        let out = drain(&mut rx);
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(matches!(&out[0], StreamOutput::Snapshot(_)));
        assert!(is_event(&out[1], "DURING1"));
        assert!(is_event(&out[2], "DURING2"));
        // Back to sending at once.
        sup.on_msg(events_msg("AFTER"));
        assert!(is_event(&drain(&mut rx)[0], "AFTER"));
    }

    #[test]
    fn held_events_are_still_sent_when_no_snapshot_follows() {
        // A failed read: no snapshot, the events come out in order.
        let (mut sup, mut rx) =
            supervisor_with(context(AccountVenue::Binance, AccountProduct::UsdM));
        sup.machine.leg_up(0, T);
        sup.on_msg(Msg::ResyncStarted);
        sup.on_msg(events_msg("A"));
        sup.on_msg(Msg::ResyncDone(Err(DownCause::Network)));
        let out = drain(&mut rx);
        assert_eq!(out.len(), 1);
        assert!(is_event(&out[0], "A"));
        assert_ne!(sup.machine.state(), StreamState::Connected);

        // A read that was accepted but must repeat (a resync request came in meanwhile).
        let (mut sup, mut rx) =
            supervisor_with(context(AccountVenue::Binance, AccountProduct::UsdM));
        sup.machine.leg_up(0, T);
        sup.on_msg(Msg::ResyncStarted);
        sup.on_msg(Msg::LegEvents {
            leg: 0,
            epoch: 1,
            events: vec![PrivateEvent::ResyncRequired(ResyncReason::MalformedFrame)],
        });
        let cmds = sup.on_msg(Msg::ResyncDone(Ok(empty_snapshot())));
        assert!(cmds.iter().any(|c| matches!(c, Command::Resync { .. })));
        let out = drain(&mut rx);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(matches!(&out[0], StreamOutput::Event(_)));
        assert_eq!(sup.machine.state(), StreamState::Resyncing);
    }

    #[test]
    fn a_consumer_that_falls_behind_ends_the_connection_instead_of_growing() {
        let (mut sup, mut rx) =
            supervisor_with(context(AccountVenue::Binance, AccountProduct::UsdM));
        sup.machine.leg_up(0, T);
        sup.machine.resync_done(true);
        sup.shared.pending_legs.store(1, Ordering::Relaxed);
        sup.unread.store(MAX_UNREAD_OUTPUTS, Ordering::Relaxed);
        let cmds = sup.on_msg(events_msg("LOST"));
        assert!(matches!(cmds.as_slice(), [Command::Connect { leg: 0, .. }]));
        assert!(drain(&mut rx).is_empty());
        assert_eq!(sup.machine.state(), StreamState::Reconnecting);
        // The pending-message count was released.
        assert_eq!(sup.shared.pending_legs.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn too_many_unread_messages_end_the_connection() {
        let shared = Shared::new(context(AccountVenue::Binance, AccountProduct::UsdM));
        let (tx, _rx) = unbounded();
        let ev = || vec![account_event("X")];
        for _ in 0..MAX_PENDING_LEG_MSGS {
            assert!(send_events(&shared, &tx, 0, 1, ev()));
        }
        assert!(!send_events(&shared, &tx, 0, 1, ev()));
        assert_eq!(
            shared.pending_legs.load(Ordering::Relaxed),
            MAX_PENDING_LEG_MSGS
        );
    }

    fn counting(count: &Arc<AtomicUsize>, block: Duration) -> StreamContext {
        let mut ctx = context(AccountVenue::Binance, AccountProduct::UsdM);
        let count = count.clone();
        ctx.load_credentials = Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(block);
            Err(CredentialsUnavailable)
        });
        ctx
    }

    fn unique_ctx(byte: u8) -> StreamContext {
        let mut ctx = context(AccountVenue::Binance, AccountProduct::UsdM);
        ctx.key.account = AccountKey::from_bytes([byte; 16]);
        ctx
    }

    #[test]
    fn a_dropped_stream_leaves_the_account_refusing_entries() {
        runtime().block_on(async {
            let ctx = unique_ctx(0xB1);
            let account = ctx.key.account;
            let stream = run(ctx);
            let token = stream.status_token;
            status::publish(account, token, StreamState::Connected);
            assert_eq!(status::stream_state(account), StreamState::Connected);
            drop(stream);
            // Nothing is registered any more, which reads as "not connected".
            assert_eq!(status::stream_state(account), StreamState::Reconnecting);
            status::publish(account, token, StreamState::Connected);
            assert_eq!(status::stream_state(account), StreamState::Reconnecting);
        });
    }

    #[test]
    fn a_panic_in_the_supervisor_leaves_the_account_refusing_entries() {
        let account = AccountKey::from_bytes([0xB3; 16]);
        let token = status::register(account);
        status::publish(account, token, StreamState::Connected);
        let crashed = std::panic::catch_unwind(|| {
            let _guard = ReconnectingOnExit {
                account,
                token: Some(token),
            };
            panic!("supervisor failed");
        });
        assert!(crashed.is_err());
        assert_eq!(status::stream_state(account), StreamState::Reconnecting);
        // A newer stream of the same account is not touched by an older guard.
        let newer = status::register(account);
        status::publish(account, newer, StreamState::Connected);
        drop(ReconnectingOnExit {
            account,
            token: Some(token),
        });
        assert_eq!(status::stream_state(account), StreamState::Connected);
        status::retire(account, newer);
    }

    #[test]
    fn exiting_after_a_failed_login_keeps_auth_failed_in_the_registry() {
        let account = AccountKey::from_bytes([0xB4; 16]);
        let token = status::register(account);
        status::publish(account, token, StreamState::AuthFailed);
        drop(ReconnectingOnExit {
            account,
            token: Some(token),
        });
        assert_eq!(status::stream_state(account), StreamState::AuthFailed);
        status::publish(account, token, StreamState::Unavailable);
        drop(ReconnectingOnExit {
            account,
            token: Some(token),
        });
        assert_eq!(status::stream_state(account), StreamState::Unavailable);
        status::retire(account, token);
    }

    #[test]
    fn the_state_survives_a_client_rebuilt_for_every_send() {
        runtime().block_on(async {
            let ctx = unique_ctx(0xB2);
            let account = ctx.key.account;
            let (mut sup, _rx) = supervisor_with(ctx);
            let token = status::register(account);
            let mut shared = Shared::new(sup.shared.ctx.clone());
            shared.status_token = Some(token);
            sup.shared = Arc::new(shared);
            assert!(sup.send(StreamOutput::State(StreamState::Connected)));
            // Whoever asks, a fresh client included, reads the same registry entry.
            for _ in 0..3 {
                assert_eq!(
                    status::stream_state_for(account, AccountVenue::Binance, AccountProduct::UsdM),
                    StreamState::Connected
                );
            }
            status::retire(account, token);
        });
    }

    #[test]
    fn dropping_a_stream_does_not_close_the_listen_key() {
        let loads = Arc::new(AtomicUsize::new(0));
        runtime().block_on(async {
            let shared = Shared::new(counting(&loads, Duration::ZERO));
            shared.key_created.store(true, Ordering::Relaxed);
            drop(shared);
            // A DELETE would load the credentials on a blocking thread right away.
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        assert_eq!(loads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn only_shutdown_closes_the_listen_key_and_it_waits_for_the_answer() {
        let loads = Arc::new(AtomicUsize::new(0));
        let rt = runtime();
        // A key was made: shutdown loads the credentials once, before it returns.
        let (mut sup, _rx) = supervisor_with(counting(&loads, Duration::ZERO));
        sup.shared.key_created.store(true, Ordering::Relaxed);
        rt.block_on(sup.shut_down());
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(sup.stopped);
        assert_eq!(sup.machine.state(), StreamState::Unavailable);
        // No key was made by this stream: nothing to close.
        let (mut sup, _rx) = supervisor_with(counting(&loads, Duration::ZERO));
        rt.block_on(sup.shut_down());
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        // OKX has no listenKey.
        let mut ctx = context(AccountVenue::Okx, AccountProduct::UsdM);
        let n = loads.clone();
        ctx.load_credentials = Arc::new(move || {
            n.fetch_add(1, Ordering::SeqCst);
            Err(CredentialsUnavailable)
        });
        let (mut sup, _rx) = supervisor_with(ctx);
        sup.shared.key_created.store(true, Ordering::Relaxed);
        rt.block_on(sup.shut_down());
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_stream_handle_shuts_down_through_the_supervisor() {
        let loads = Arc::new(AtomicUsize::new(0));
        runtime().block_on(async {
            let ctx = counting(&loads, Duration::ZERO);
            // The connection task cannot connect (no credentials) and only retries.
            let stream = run(ctx);
            tokio::time::timeout(Duration::from_secs(10), stream.shutdown())
                .await
                .expect("shutdown answers");
        });
    }

    #[test]
    fn the_read_the_keep_alive_and_the_clock_task_end_with_the_stream() {
        let loads = Arc::new(AtomicUsize::new(0));
        runtime().block_on(async {
            let (mut sup, _rx) = supervisor_with(counting(&loads, Duration::from_millis(300)));
            let shared = sup.shared.clone();
            sup.apply(vec![
                Command::Resync {
                    after: Duration::from_secs(3600),
                },
                Command::KeepAlive,
                Command::SyncClock,
            ]);
            tokio::task::yield_now().await;
            assert!(Arc::strong_count(&shared) > 1);
            drop(sup);
            for _ in 0..50 {
                if Arc::strong_count(&shared) == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(Arc::strong_count(&shared), 1, "a task outlived the stream");
        });
    }

    #[test]
    fn binance_futures_waits_about_three_and_a_half_minutes_in_silence() {
        const { assert!(BINANCE_FUTURES_SILENCE.as_secs() >= 200) };
        const { assert!(BINANCE_FUTURES_SILENCE.as_secs() <= 240) };
    }

    #[test]
    fn an_okx_leg_is_up_only_when_every_channel_is_confirmed() {
        let sub = |c: &str| format!(r#"{{"event":"subscribe","arg":{{"channel":"{c}"}}}}"#);
        let mut p = SubscribeProgress::new(OkxLeg::Private, AccountProduct::UsdM);
        assert!(!p.complete());
        assert_eq!(p.feed(sub("orders").as_bytes()), None);
        // The same channel twice is not two channels.
        assert_eq!(p.feed(sub("orders").as_bytes()), None);
        assert_eq!(p.feed(sub("positions").as_bytes()), None);
        assert!(!p.complete());
        // A push in between is kept, not lost.
        let push = br#"{"arg":{"channel":"positions"},"data":[]}"#;
        assert_eq!(p.feed(push), None);
        assert_eq!(p.feed(sub("account").as_bytes()), None);
        assert!(p.complete());

        let mut b = SubscribeProgress::new(OkxLeg::Business, AccountProduct::UsdM);
        b.feed(sub("orders-algo").as_bytes());
        assert!(!b.complete());
        b.feed(sub("algo-advance").as_bytes());
        assert!(b.complete());
    }

    #[test]
    fn a_refused_subscription_stops_the_stream_instead_of_logging_in_again_and_again() {
        let mut p = SubscribeProgress::new(OkxLeg::Private, AccountProduct::UsdM);
        assert_eq!(
            p.feed(
                br#"{"event":"error","code":"60018","msg":"Wrong URL or channel doesn't exist"}"#
            ),
            Some(DownCause::Unavailable)
        );
        // A wrong clock mends: measure and retry with a backoff.
        assert_eq!(
            p.feed(br#"{"event":"error","code":"60006","msg":"x"}"#),
            Some(DownCause::ClockSkew)
        );
        assert_eq!(
            p.feed(br#"{"event":"notice","code":"64008","msg":"x"}"#),
            Some(DownCause::Expired)
        );
        assert_eq!(p.feed(b"not json"), Some(DownCause::Network));
        // The machine does not reconnect after `Unavailable`.
        let mut m = Machine::start(2, false).0;
        assert_eq!(m.leg_down(0, DownCause::Unavailable), vec![Command::Stop]);
    }

    fn position_push(n: usize) -> String {
        let item =
            r#"{"instId":"BTC-USDT-SWAP","posSide":"net","pos":"1","avgPx":"1","mgnMode":"cross"}"#;
        let items = vec![item; n].join(",");
        format!(r#"{{"arg":{{"channel":"positions"}},"data":[{items}]}}"#)
    }

    #[test]
    fn early_events_are_bounded_and_the_limit_itself_is_allowed() {
        let mut p = SubscribeProgress::new(OkxLeg::Private, AccountProduct::UsdM);
        assert_eq!(p.feed(position_push(MAX_EARLY_EVENTS).as_bytes()), None);
        assert_eq!(p.early.len(), MAX_EARLY_EVENTS);
        // One more event is over the limit; nothing is truncated or added.
        assert_eq!(
            p.feed(position_push(1).as_bytes()),
            Some(DownCause::Network)
        );
        assert_eq!(p.early.len(), MAX_EARLY_EVENTS);
        // One frame that alone exceeds the limit ends the leg too.
        let mut q = SubscribeProgress::new(OkxLeg::Private, AccountProduct::UsdM);
        assert_eq!(
            q.feed(position_push(MAX_EARLY_EVENTS + 1).as_bytes()),
            Some(DownCause::Network)
        );
        assert!(q.early.is_empty());
    }

    #[test]
    fn a_channel_we_did_not_ask_for_confirms_nothing() {
        let sub = |c: &str| format!(r#"{{"event":"subscribe","arg":{{"channel":"{c}"}}}}"#);
        let mut p = SubscribeProgress::new(OkxLeg::Private, AccountProduct::UsdM);
        for c in ["tickers", "books", "orders-algo", "x"] {
            assert_eq!(p.feed(sub(c).as_bytes()), None);
        }
        assert!(p.confirmed.is_empty());
        assert!(!p.complete());
    }

    #[test]
    fn a_handshake_read_never_waits_past_the_deadline() {
        let t0 = Instant::now();
        let deadline = t0 + HANDSHAKE_DEADLINE;
        assert_eq!(remaining(deadline, t0), Some(HANDSHAKE_READ));
        assert_eq!(
            remaining(deadline, t0 + Duration::from_secs(12)),
            Some(Duration::from_secs(3))
        );
        assert_eq!(remaining(deadline, deadline), None);
        assert_eq!(remaining(deadline, deadline + Duration::from_secs(1)), None);
    }
}
