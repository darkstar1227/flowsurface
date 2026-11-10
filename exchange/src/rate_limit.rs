//! Rate limiting for signed requests (shared with public Binance market data).
//!
//! - **Binance** counts IP weight per host. The bucket is the same instance the public
//!   market-data fetches use (`adapter::limiter::binance_host_state`), so signed and public
//!   requests consume one budget. An HTTP 418 (IP ban) pauses *every* Binance request,
//!   public fetches included, until `Retry-After` has passed.
//! - **OKX** counts per endpoint (and per `instId` for orders), by user id, so it needs no
//!   sharing with market data.
//!
//! Priorities: protective requests (cancel, close, protective orders) may use the whole
//! budget, queries up to 90 %, new entries only up to 80 %: the last 20 % is reserved so a
//! cancel or a protective order can always be sent. Nothing here sleeps or retries: a
//! denied request is reported to the caller, and order requests are never retried.

use crate::account::{AccountKey, ResponseMeta};
use crate::adapter::limiter::{
    BinanceHostState, OrderLimits, binance_ban_remaining, binance_env_of_host, binance_host_state,
    binance_set_ban, default_binance_config, lock, pause_until,
};
use std::collections::{HashMap, VecDeque};
#[cfg(not(test))]
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Share of every budget kept for protective requests (user decision).
pub const ENTRY_RESERVE_PCT: usize = 20;
/// Queries stop at this share of the budget so they cannot starve protective requests.
const QUERY_CAP_PCT: usize = 90;
/// Pause when a 429 carries no `Retry-After`.
const DEFAULT_429_PAUSE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Cancel, close, protective (stop-loss / take-profit) orders.
    Protective,
    /// Read-only requests.
    Query,
    /// New entry orders.
    Entry,
}

impl Priority {
    pub(crate) fn cap(self, limit: usize) -> usize {
        match self {
            Priority::Protective => limit,
            Priority::Query => limit * QUERY_CAP_PCT / 100,
            Priority::Entry => limit * (100 - ENTRY_RESERVE_PCT) / 100,
        }
    }
}

/// Why a request was not allowed to go out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    /// Budget exhausted; may be tried again after this long.
    Wait(Duration),
    /// Only the reserved share is left, which entries may not use.
    Reserved,
    /// Binance IP ban (HTTP 418) in effect for this long.
    IpBanned(Duration),
    /// The venue's order limits are not known yet (fetch the trading rules first).
    LimitsUnknown,
}

// ── Sliding-window bucket (OKX) ─────────────────────────────────────────

/// `limit` units per `window`, counted over a sliding window.
#[derive(Debug)]
pub struct WindowBucket {
    limit: usize,
    window: Duration,
    used: VecDeque<(Instant, usize)>,
    paused_until: Option<Instant>,
}

impl WindowBucket {
    pub fn new(limit: usize, window: Duration) -> Self {
        Self {
            limit,
            window,
            used: VecDeque::new(),
            paused_until: None,
        }
    }

    fn prune(&mut self, now: Instant) {
        while let Some(&(t, _)) = self.used.front() {
            if now.duration_since(t) >= self.window {
                self.used.pop_front();
            } else {
                break;
            }
        }
    }

    fn total(&self) -> usize {
        self.used.iter().map(|&(_, c)| c).sum()
    }

    /// Reserve `cost` units for a request of `priority`.
    pub fn acquire(&mut self, now: Instant, cost: usize, priority: Priority) -> Result<(), Denied> {
        if let Some(until) = self.paused_until {
            if until > now {
                return Err(Denied::Wait(until - now));
            }
            self.paused_until = None;
        }
        self.prune(now);
        let used = self.total();
        if used + cost <= priority.cap(self.limit) {
            self.used.push_back((now, cost));
            return Ok(());
        }
        if priority != Priority::Protective && used + cost <= self.limit {
            return Err(Denied::Reserved);
        }
        // Wait until enough of the oldest entries have left the window.
        let mut freed = 0;
        let need = (used + cost).saturating_sub(self.limit);
        let mut wait = self.window;
        for &(t, c) in &self.used {
            freed += c;
            if freed >= need {
                wait = self.window.saturating_sub(now.duration_since(t));
                break;
            }
        }
        Err(Denied::Wait(wait))
    }

    /// Pause this bucket (HTTP 429).
    pub fn pause(&mut self, now: Instant, duration: Duration) {
        let until = pause_until(now, duration);
        if self.paused_until.is_none_or(|cur| until > cur) {
            self.paused_until = Some(until);
        }
    }
}

// ── OKX ─────────────────────────────────────────────────────────────────

/// OKX endpoint groups with their own limits. Orders are counted per `instId`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OkxEndpoint {
    /// Place / cancel / amend order: 60 per 2 s per instrument.
    Order(String),
    /// `order-algo`: 20 per 2 s.
    OrderAlgo,
    /// `cancel-algos`: 20 per 2 s.
    CancelAlgos,
    /// `orders-algo-pending`: 20 per 2 s.
    OrdersAlgoPending,
    /// `positions`: 10 per 2 s.
    Positions,
    /// `balance`: 10 per 2 s.
    Balance,
    /// `orders-pending`: 60 per 2 s.
    OrdersPending,
    /// `public/instruments`: 20 per 2 s.
    Instruments,
    /// `account/config`: kept at 5 per 2 s, a conservative value that is not verified.
    AccountConfig,
}

const OKX_WINDOW: Duration = Duration::from_secs(2);

impl OkxEndpoint {
    fn limit(&self) -> usize {
        match self {
            OkxEndpoint::Order(_) | OkxEndpoint::OrdersPending => 60,
            OkxEndpoint::OrderAlgo | OkxEndpoint::CancelAlgos | OkxEndpoint::OrdersAlgoPending => {
                20
            }
            OkxEndpoint::Instruments => 20,
            OkxEndpoint::Positions | OkxEndpoint::Balance => 10,
            OkxEndpoint::AccountConfig => 5,
        }
    }
}

#[derive(Debug, Default)]
pub struct OkxLimiter {
    buckets: HashMap<OkxEndpoint, WindowBucket>,
}

impl OkxLimiter {
    fn bucket(&mut self, endpoint: &OkxEndpoint) -> &mut WindowBucket {
        self.buckets
            .entry(endpoint.clone())
            .or_insert_with(|| WindowBucket::new(endpoint.limit(), OKX_WINDOW))
    }

    pub fn acquire(
        &mut self,
        endpoint: &OkxEndpoint,
        priority: Priority,
        now: Instant,
    ) -> Result<(), Denied> {
        self.bucket(endpoint).acquire(now, 1, priority)
    }

    pub fn on_response(&mut self, endpoint: &OkxEndpoint, meta: &ResponseMeta, now: Instant) {
        if meta.status == 429 {
            let pause = meta
                .retry_after_secs
                .map_or(DEFAULT_429_PAUSE, Duration::from_secs);
            self.bucket(endpoint).pause(now, pause);
        }
    }
}

#[cfg(not(test))]
static OKX: LazyLock<Mutex<OkxLimiter>> = LazyLock::new(|| Mutex::new(OkxLimiter::default()));

// Under test the OKX limiter is per thread: every test runs on its own thread, so a 429
// simulated by one test cannot pause the requests of another one (same idea as the Binance
// ban in `adapter::limiter`).
#[cfg(test)]
thread_local! {
    static OKX_TEST: std::cell::RefCell<OkxLimiter> = std::cell::RefCell::new(OkxLimiter::default());
}

/// Forget this thread's OKX limiter state, for tests that run several scenarios in a row
/// (a 429 in one pauses the instrument's bucket for the next).
#[cfg(test)]
pub(crate) fn okx_reset_for_tests() {
    with_okx(|l| *l = OkxLimiter::default());
}

fn with_okx<T>(f: impl FnOnce(&mut OkxLimiter) -> T) -> T {
    #[cfg(not(test))]
    {
        f(&mut lock(&OKX))
    }
    #[cfg(test)]
    {
        OKX_TEST.with(|l| f(&mut l.borrow_mut()))
    }
}

// ── Binance ─────────────────────────────────────────────────────────────

/// Check a signed Binance request against `state`. The global IP ban is checked by the
/// caller ([`acquire`]) so this stays testable with a local state.
pub fn acquire_binance(
    state: &mut BinanceHostState,
    account: Option<AccountKey>,
    weight: usize,
    priority: Priority,
    now: Instant,
) -> Result<(), Denied> {
    if let Some(until) = state.signed_pause_until {
        if until > now {
            return Err(Denied::Wait(until - now));
        }
        state.signed_pause_until = None;
    }
    // A public 429 holds back entries and queries; protective requests may still go.
    if priority != Priority::Protective
        && let Some(wait) = state.public_pause_remaining(now)
    {
        return Err(Denied::Wait(wait));
    }
    // New entries stop short of the order-count limits too, keeping room for cancels. The
    // limits come from `exchangeInfo`; without them (rules not fetched yet) entries wait.
    if priority == Priority::Entry {
        let Some(limits) = state.order_limits else {
            return Err(Denied::LimitsUnknown);
        };
        let c = state.counts_for(account, now);
        // A window the exchange does not list (spot has no per-minute order limit) is not
        // limited here.
        if limits
            .per_10s
            .is_some_and(|l| c.per_10s + 1 > Priority::Entry.cap(l))
            || limits
                .per_1m
                .is_some_and(|l| c.per_1m + 1 > Priority::Entry.cap(l))
        {
            return Err(Denied::Reserved);
        }
    }
    let used = state.bucket.used_estimate();
    let max = state.bucket.max_weight();
    if used + weight > priority.cap(max) && priority != Priority::Protective {
        return if used + weight <= max {
            Err(Denied::Reserved)
        } else {
            Err(Denied::Wait(
                state
                    .bucket
                    .prepare_request(weight)
                    .0
                    .unwrap_or(Duration::from_secs(1)),
            ))
        };
    }
    match state.bucket.prepare_request(weight).0 {
        None => Ok(()),
        Some(wait) => Err(Denied::Wait(wait)),
    }
}

/// Apply response facts: weight and order counts, 429 (signed requests pause), 418 (global
/// ban).
pub fn record_binance(
    state: &mut BinanceHostState,
    account: Option<AccountKey>,
    meta: &ResponseMeta,
    now: Instant,
) {
    if let Some(w) = meta.used_weight_1m {
        state.bucket.update_weight(w);
    }
    if meta.order_count_10s.is_some() || meta.order_count_1m.is_some() {
        state.set_counts(account, meta.order_count_10s, meta.order_count_1m, now);
    }
    let retry = meta.retry_after_secs.map(Duration::from_secs);
    match meta.status {
        429 => {
            let until = pause_until(now, retry.unwrap_or(DEFAULT_429_PAUSE));
            if state.signed_pause_until.is_none_or(|cur| until > cur) {
                state.signed_pause_until = Some(until);
            }
        }
        418 => binance_set_ban(state.env, now, retry),
        _ => {}
    }
}

// ── Entry points used by the signed transport ───────────────────────────

/// Which budget a signed request draws from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitKey {
    /// Binance IP weight of `host`.
    Binance {
        host: String,
        weight: usize,
        /// Order counts are kept per account. `None` for requests without an account.
        account: Option<AccountKey>,
    },
    Okx(OkxEndpoint),
}

impl LimitKey {
    /// A Binance key for a request without an account (public or connection test).
    pub fn binance(host: &str, weight: usize) -> Self {
        LimitKey::Binance {
            host: host.to_string(),
            weight,
            account: None,
        }
    }
}

pub fn acquire(key: &LimitKey, priority: Priority, now: Instant) -> Result<(), Denied> {
    match key {
        LimitKey::Binance {
            host,
            weight,
            account,
        } => {
            if let Some(remaining) = binance_ban_remaining(binance_env_of_host(host), now) {
                return Err(Denied::IpBanned(remaining));
            }
            let shared = binance_host_state(host, default_binance_config(host));
            let mut state = lock(&shared);
            acquire_binance(&mut state, *account, *weight, priority, now)
        }
        LimitKey::Okx(endpoint) => with_okx(|l| l.acquire(endpoint, priority, now)),
    }
}

pub fn record(key: &LimitKey, meta: &ResponseMeta, now: Instant) {
    match key {
        LimitKey::Binance { host, account, .. } => {
            let shared = binance_host_state(host, default_binance_config(host));
            record_binance(&mut lock(&shared), *account, meta, now);
        }
        LimitKey::Okx(endpoint) => with_okx(|l| l.on_response(endpoint, meta, now)),
    }
}

/// Remaining Binance IP-ban time of `env`, for the UI.
pub fn binance_ban(env: crate::account::AccountEnv) -> Option<Duration> {
    binance_ban_remaining(env, Instant::now())
}

/// Read the `ORDERS` limits out of an `exchangeInfo` `rateLimits` array (spot and USDⓈ-M share
/// the shape) and store them for `host`. Returns `None` (nothing stored) when no window we use
/// is listed.
pub fn apply_exchange_info_limits(host: &str, info: &serde_json::Value) -> Option<OrderLimits> {
    let limits = parse_order_limits(info)?;
    let shared = binance_host_state(host, default_binance_config(host));
    lock(&shared).order_limits = Some(limits);
    Some(limits)
}

/// The 10-second and 1-minute windows of `rateLimits` entries of type `ORDERS`. USDⓈ-M lists
/// both; spot lists a 10-second and a daily window (docs copy `rest-api.md`), so each window is
/// optional and only "none at all" is `None`.
pub fn parse_order_limits(info: &serde_json::Value) -> Option<OrderLimits> {
    let mut ten = None;
    let mut minute = None;
    for e in info.get("rateLimits")?.as_array()? {
        if e.get("rateLimitType").and_then(|v| v.as_str()) != Some("ORDERS") {
            continue;
        }
        let n = e.get("intervalNum").and_then(|v| v.as_u64());
        let limit = e.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);
        match (e.get("interval").and_then(|v| v.as_str()), n) {
            (Some("SECOND"), Some(10)) => ten = limit,
            (Some("MINUTE"), Some(1)) => minute = limit,
            _ => {}
        }
    }
    if ten.is_none() && minute.is_none() {
        return None;
    }
    Some(OrderLimits {
        per_10s: ten,
        per_1m: minute,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::AccountEnv;
    use crate::adapter::limiter::{
        DynamicRateLimiterConfig, HeaderDynamicRateLimiter, MAX_PAUSE, RateLimiter,
        binance_clear_ban_for_tests,
    };

    // The global ban is process-wide, so tests touching it run one at a time.
    static BAN_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn cfg(max: usize) -> DynamicRateLimiterConfig {
        DynamicRateLimiterConfig::new(
            max,
            Duration::from_secs(60),
            0.0,
            "x-mbx-used-weight-1m",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            Some(reqwest::StatusCode::IM_A_TEAPOT),
        )
    }

    fn meta(status: u16) -> ResponseMeta {
        ResponseMeta {
            status,
            ..ResponseMeta::default()
        }
    }

    #[test]
    fn window_bucket_reserves_last_20_percent_for_protective() {
        let now = Instant::now();
        let mut b = WindowBucket::new(10, Duration::from_secs(2));
        // 80 % is the entry ceiling.
        for _ in 0..8 {
            assert_eq!(b.acquire(now, 1, Priority::Entry), Ok(()));
        }
        assert_eq!(b.acquire(now, 1, Priority::Entry), Err(Denied::Reserved));
        // Queries may go to 90 %, protective requests to 100 %.
        assert_eq!(b.acquire(now, 1, Priority::Query), Ok(()));
        assert_eq!(b.acquire(now, 1, Priority::Query), Err(Denied::Reserved));
        assert_eq!(b.acquire(now, 1, Priority::Protective), Ok(()));
        // Fully used: even protective requests have to wait.
        assert!(matches!(
            b.acquire(now, 1, Priority::Protective),
            Err(Denied::Wait(_))
        ));
        // The window slides: everything is free again afterwards.
        let later = now + Duration::from_secs(2);
        assert_eq!(b.acquire(later, 1, Priority::Entry), Ok(()));
    }

    #[test]
    fn okx_endpoints_are_counted_independently() {
        let now = Instant::now();
        let mut l = OkxLimiter::default();
        let btc = OkxEndpoint::Order("BTC-USDT-SWAP".into());
        let eth = OkxEndpoint::Order("ETH-USDT-SWAP".into());
        // Exhaust the algo bucket (20 per 2 s).
        for _ in 0..20 {
            assert_eq!(
                l.acquire(&OkxEndpoint::OrderAlgo, Priority::Protective, now),
                Ok(())
            );
        }
        assert!(
            l.acquire(&OkxEndpoint::OrderAlgo, Priority::Protective, now)
                .is_err()
        );
        // Other endpoint groups, and other instruments, are unaffected.
        assert_eq!(
            l.acquire(&OkxEndpoint::CancelAlgos, Priority::Protective, now),
            Ok(())
        );
        assert_eq!(
            l.acquire(&OkxEndpoint::Positions, Priority::Query, now),
            Ok(())
        );
        assert_eq!(l.acquire(&btc, Priority::Entry, now), Ok(()));
        for _ in 0..47 {
            assert_eq!(l.acquire(&btc, Priority::Entry, now), Ok(()));
        }
        // 48 = 80 % of 60: the 49th BTC entry is refused, ETH is untouched.
        assert_eq!(l.acquire(&btc, Priority::Entry, now), Err(Denied::Reserved));
        assert_eq!(l.acquire(&eth, Priority::Entry, now), Ok(()));
        // Limits per group.
        assert_eq!(OkxEndpoint::Positions.limit(), 10);
        assert_eq!(OkxEndpoint::Balance.limit(), 10);
        assert_eq!(OkxEndpoint::OrdersAlgoPending.limit(), 20);
        assert_eq!(btc.limit(), 60);
    }

    #[test]
    fn okx_429_pauses_only_that_bucket_and_is_not_retried() {
        let now = Instant::now();
        let mut l = OkxLimiter::default();
        l.on_response(
            &OkxEndpoint::OrderAlgo,
            &ResponseMeta {
                status: 429,
                retry_after_secs: Some(5),
                ..ResponseMeta::default()
            },
            now,
        );
        assert_eq!(
            l.acquire(&OkxEndpoint::OrderAlgo, Priority::Protective, now),
            Err(Denied::Wait(Duration::from_secs(5)))
        );
        assert_eq!(
            l.acquire(&OkxEndpoint::Balance, Priority::Query, now),
            Ok(())
        );
        // After the pause it works again.
        let later = now + Duration::from_secs(5);
        assert_eq!(
            l.acquire(&OkxEndpoint::OrderAlgo, Priority::Protective, later),
            Ok(())
        );
    }

    #[test]
    fn binance_signed_and_public_requests_share_one_budget() {
        let _g = BAN_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        binance_clear_ban_for_tests();
        let host = "shared-budget-test.example";
        let shared = binance_host_state(host, cfg(100));
        // The public path (HeaderDynamicRateLimiter) and the signed path hold the same state.
        let mut public = HeaderDynamicRateLimiter::new_shared(cfg(100), shared.clone());
        assert!(public.prepare_request(60).is_none());
        let now = Instant::now();
        // Public requests used 60 of 100 (fallback accounting); a signed entry of weight
        // 30 would pass 80 and is held back, a protective one still fits.
        {
            let mut st = lock(&shared);
            st.order_limits = Some(OrderLimits::both(300, 1200));
            assert_eq!(
                acquire_binance(&mut st, None, 30, Priority::Entry, now),
                Err(Denied::Reserved)
            );
            assert_eq!(
                acquire_binance(&mut st, None, 30, Priority::Protective, now),
                Ok(())
            );
        }
        // The signed request's weight now counts against public fetches as well.
        assert!(public.prepare_request(20).is_some());
    }

    #[test]
    fn binance_entry_is_refused_near_order_count_limit() {
        let mut st = BinanceHostState::new(
            AccountEnv::Live,
            cfg(1000),
            Some(OrderLimits::both(300, 1200)),
        );
        let now = Instant::now();
        assert_eq!(
            acquire_binance(&mut st, None, 1, Priority::Entry, now),
            Ok(())
        );
        record_binance(
            &mut st,
            None,
            &ResponseMeta {
                status: 200,
                order_count_10s: Some(240),
                order_count_1m: Some(10),
                ..ResponseMeta::default()
            },
            now,
        );
        assert_eq!(
            acquire_binance(&mut st, None, 1, Priority::Entry, now),
            Err(Denied::Reserved)
        );
        // Cancels are still allowed.
        assert_eq!(
            acquire_binance(&mut st, None, 1, Priority::Protective, now),
            Ok(())
        );
    }

    #[test]
    fn binance_429_pauses_signed_requests_only() {
        let _g = BAN_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        binance_clear_ban_for_tests();
        let shared = binance_host_state("pause-429-test.example", cfg(1000));
        let mut public = HeaderDynamicRateLimiter::new_shared(cfg(1000), shared.clone());
        let now = Instant::now();
        record_binance(
            &mut lock(&shared),
            None,
            &ResponseMeta {
                status: 429,
                retry_after_secs: Some(30),
                ..ResponseMeta::default()
            },
            now,
        );
        assert_eq!(
            acquire_binance(&mut lock(&shared), None, 1, Priority::Protective, now),
            Err(Denied::Wait(Duration::from_secs(30)))
        );
        // Public fetches keep their existing behaviour: no extra pause.
        assert!(public.prepare_request(1).is_none());
        assert!(binance_ban(AccountEnv::Live).is_none());
    }

    #[test]
    fn binance_418_pauses_every_request_including_public_fetches() {
        let _g = BAN_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        binance_clear_ban_for_tests();
        let shared = binance_host_state("ban-418-test.example", cfg(1000));
        let mut public = HeaderDynamicRateLimiter::new_shared(cfg(1000), shared.clone());
        let other_host = binance_host_state("ban-418-other.example", cfg(1000));
        let mut other_public = HeaderDynamicRateLimiter::new_shared(cfg(1000), other_host);
        let now = Instant::now();
        assert!(public.prepare_request(1).is_none());

        record_binance(
            &mut lock(&shared),
            None,
            &ResponseMeta {
                status: 418,
                retry_after_secs: Some(120),
                ..ResponseMeta::default()
            },
            now,
        );
        // Public fetches on every host wait out the ban; signed requests are denied.
        assert!(public.prepare_request(1).is_some());
        assert!(other_public.prepare_request(1).is_some());
        assert!(matches!(
            acquire(
                &LimitKey::Binance {
                    host: "ban-418-other.example".into(),
                    weight: 1,
                    account: None
                },
                Priority::Protective,
                Instant::now()
            ),
            Err(Denied::IpBanned(_))
        ));
        assert!(binance_ban(AccountEnv::Live).is_some());
        // A later 418 never shortens an existing ban.
        binance_set_ban(
            AccountEnv::Live,
            Instant::now(),
            Some(Duration::from_secs(1)),
        );
        assert!(binance_ban(AccountEnv::Live).unwrap() > Duration::from_secs(60));
        binance_clear_ban_for_tests();
        assert!(public.prepare_request(1).is_none());
    }

    /// Serve one canned HTTP response on localhost and fetch it.
    fn fetch_canned(status_line: &str, extra_header: &str) -> reqwest::Response {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let raw = format!(
            "HTTP/1.1 {status_line}\r\n{extra_header}Content-Length: 0\r\nConnection: close\r\n\r\n"
        );
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(raw.as_bytes());
            }
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(reqwest::Client::new().get(format!("http://{addr}/")).send())
            .unwrap()
    }

    #[test]
    fn public_418_sets_the_global_pause_via_should_exit_on_response() {
        let _g = BAN_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        binance_clear_ban_for_tests();
        let shared = binance_host_state("public-418-test.example", cfg(1000));
        let public = HeaderDynamicRateLimiter::new_shared(cfg(1000), shared);
        assert!(binance_ban(AccountEnv::Live).is_none());

        let response = fetch_canned("418 I'm a teapot", "Retry-After: 90\r\n");
        assert!(public.should_exit_on_response(&response));
        let remaining = binance_ban(AccountEnv::Live).expect("418 must set the global pause");
        assert!(remaining > Duration::from_secs(60) && remaining <= Duration::from_secs(90));
        // The public path reports it without sleeping, for the fetch worker to fail fast.
        assert!(public.ban_remaining().is_some());
        binance_clear_ban_for_tests();
        assert!(public.ban_remaining().is_none());
    }

    #[test]
    fn order_limits_come_from_exchange_info_rate_limits() {
        let info = serde_json::json!({"rateLimits": [
            {"rateLimitType": "REQUEST_WEIGHT", "interval": "MINUTE", "intervalNum": 1, "limit": 6000},
            {"rateLimitType": "ORDERS", "interval": "SECOND", "intervalNum": 10, "limit": 100},
            {"rateLimitType": "ORDERS", "interval": "MINUTE", "intervalNum": 1, "limit": 1200},
            {"rateLimitType": "ORDERS", "interval": "DAY", "intervalNum": 1, "limit": 200000}
        ]});
        assert_eq!(
            parse_order_limits(&info),
            Some(OrderLimits::both(100, 1200))
        );
        // spot: a 10-second and a daily window, no per-minute one
        let spot = serde_json::json!({"rateLimits": [
            {"rateLimitType": "ORDERS", "interval": "SECOND", "intervalNum": 10, "limit": 50},
            {"rateLimitType": "ORDERS", "interval": "DAY", "intervalNum": 1, "limit": 160000}
        ]});
        assert_eq!(
            parse_order_limits(&spot),
            Some(OrderLimits {
                per_10s: Some(50),
                per_1m: None
            })
        );
        // spot and futures use the same entries; a body without them gives nothing
        assert_eq!(parse_order_limits(&serde_json::json!({})), None);
    }

    #[test]
    fn entries_wait_until_the_order_limits_are_known() {
        let mut st = BinanceHostState::new(AccountEnv::Live, cfg(1000), None);
        let now = Instant::now();
        assert_eq!(
            acquire_binance(&mut st, None, 1, Priority::Entry, now),
            Err(Denied::LimitsUnknown)
        );
        // cancels and protective orders never wait for the limits
        assert_eq!(
            acquire_binance(&mut st, None, 1, Priority::Protective, now),
            Ok(())
        );
        st.order_limits = Some(OrderLimits::both(300, 1200));
        assert_eq!(
            acquire_binance(&mut st, None, 1, Priority::Entry, now),
            Ok(())
        );
    }

    #[test]
    fn order_counts_are_per_account_and_decay_to_zero() {
        let mut st = BinanceHostState::new(
            AccountEnv::Live,
            cfg(1000),
            Some(OrderLimits::both(100, 1200)),
        );
        let (a, b) = (
            AccountKey::from_bytes([1; 16]),
            AccountKey::from_bytes([2; 16]),
        );
        let now = Instant::now();
        let hot = ResponseMeta {
            status: 200,
            order_count_10s: Some(90),
            order_count_1m: Some(100),
            ..ResponseMeta::default()
        };
        record_binance(&mut st, Some(a), &hot, now);
        // account A is at its entry ceiling; account B is untouched
        assert_eq!(
            acquire_binance(&mut st, Some(a), 1, Priority::Entry, now),
            Err(Denied::Reserved)
        );
        assert_eq!(
            acquire_binance(&mut st, Some(b), 1, Priority::Entry, now),
            Ok(())
        );
        // the 10 s window has passed: that count reads zero again
        let later = now + Duration::from_secs(11);
        assert_eq!(st.counts_for(Some(a), later).per_10s, 0);
        assert_eq!(st.counts_for(Some(a), later).per_1m, 100);
        assert_eq!(
            acquire_binance(&mut st, Some(a), 1, Priority::Entry, later),
            Ok(())
        );
        let much_later = now + Duration::from_secs(61);
        assert_eq!(st.counts_for(Some(a), much_later).per_1m, 0);
    }

    #[test]
    fn public_429_pauses_entries_and_queries_but_not_protective() {
        let mut st = BinanceHostState::new(
            AccountEnv::Live,
            cfg(1000),
            Some(OrderLimits::both(300, 1200)),
        );
        let now = Instant::now();
        st.set_public_pause(now, Duration::from_secs(20));
        assert!(matches!(
            acquire_binance(&mut st, None, 1, Priority::Entry, now),
            Err(Denied::Wait(_))
        ));
        assert!(matches!(
            acquire_binance(&mut st, None, 1, Priority::Query, now),
            Err(Denied::Wait(_))
        ));
        assert_eq!(
            acquire_binance(&mut st, None, 1, Priority::Protective, now),
            Ok(())
        );
        let later = now + Duration::from_secs(21);
        assert_eq!(
            acquire_binance(&mut st, None, 1, Priority::Query, later),
            Ok(())
        );
    }

    #[test]
    fn minute_boundary_does_not_refill_when_the_clock_is_pinned() {
        use crate::adapter::limiter::{FixedWindowBucket, set_wall_clock_secs_for_tests};
        // 59 s into a minute: the bucket is drained, the clock then crosses the boundary
        // (and stays mid-minute again). Tests must not see a refill from the wall clock.
        set_wall_clock_secs_for_tests(59);
        let mut b = FixedWindowBucket::new(10, Duration::from_secs(60));
        assert!(b.calculate_wait_time(10).is_none());
        assert!(b.calculate_wait_time(1).is_some());
        set_wall_clock_secs_for_tests(60 + 30);
        assert!(b.calculate_wait_time(1).is_some());
        // A real boundary (second 0 of the minute) still refills, as in production.
        set_wall_clock_secs_for_tests(120);
        assert!(b.calculate_wait_time(1).is_none());
        set_wall_clock_secs_for_tests(30);
    }

    #[test]
    fn public_market_data_stops_at_the_query_share() {
        let _g = BAN_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        binance_clear_ban_for_tests();
        let shared = binance_host_state("public-cap-test.example", cfg(100));
        let mut public = HeaderDynamicRateLimiter::new_shared(cfg(100), shared);
        assert!(public.prepare_request(80).is_none());
        // 80 + 11 would pass 90 % of the budget, so it waits although 100 is not reached
        assert!(public.prepare_request(11).is_some());
        assert!(public.prepare_request(10).is_none());
    }

    #[test]
    fn a_418_on_testnet_does_not_stop_live() {
        let _g = BAN_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        binance_clear_ban_for_tests();
        let now = Instant::now();
        binance_set_ban(AccountEnv::Testnet, now, Some(Duration::from_secs(60)));
        assert!(binance_ban(AccountEnv::Testnet).is_some());
        assert!(binance_ban(AccountEnv::Live).is_none());
        binance_clear_ban_for_tests();
        binance_set_ban(AccountEnv::Live, now, Some(Duration::from_secs(60)));
        assert!(binance_ban(AccountEnv::Live).is_some());
        assert!(binance_ban(AccountEnv::Testnet).is_none());
        binance_clear_ban_for_tests();
    }

    #[test]
    fn huge_retry_after_is_capped_and_does_not_panic() {
        let _g = BAN_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        binance_clear_ban_for_tests();
        let now = Instant::now();
        for secs in [u64::MAX, i64::MAX as u64, u64::MAX / 2, 10_000_000] {
            let until = pause_until(now, Duration::from_secs(secs));
            assert_eq!(until, now + MAX_PAUSE, "{secs}");
        }
        assert_eq!(
            pause_until(now, Duration::from_secs(5)),
            now + Duration::from_secs(5)
        );

        // Every place a `Retry-After` is turned into a deadline.
        binance_set_ban(AccountEnv::Live, now, Some(Duration::from_secs(u64::MAX)));
        assert!(binance_ban(AccountEnv::Live).unwrap() <= MAX_PAUSE);
        binance_clear_ban_for_tests();
        let mut st = BinanceHostState::new(AccountEnv::Live, cfg(1000), None);
        record_binance(
            &mut st,
            None,
            &ResponseMeta {
                status: 429,
                retry_after_secs: Some(u64::MAX),
                ..ResponseMeta::default()
            },
            now,
        );
        assert!(st.signed_pause_until.unwrap() <= now + MAX_PAUSE);
        record_binance(
            &mut st,
            None,
            &ResponseMeta {
                status: 418,
                retry_after_secs: Some(u64::MAX),
                ..ResponseMeta::default()
            },
            now,
        );
        assert!(binance_ban(AccountEnv::Live).unwrap() <= MAX_PAUSE);
        let mut bucket = WindowBucket::new(10, Duration::from_secs(1));
        bucket.pause(now, Duration::from_secs(u64::MAX));
        binance_clear_ban_for_tests();
    }

    #[test]
    fn non_ban_responses_do_not_touch_the_ban() {
        let _g = BAN_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        binance_clear_ban_for_tests();
        let mut st = BinanceHostState::new(AccountEnv::Live, cfg(1000), None);
        record_binance(&mut st, None, &meta(200), Instant::now());
        record_binance(&mut st, None, &meta(500), Instant::now());
        assert!(binance_ban(AccountEnv::Live).is_none());
    }

    #[test]
    fn send_limited_denies_locally_without_sending() {
        let _g = BAN_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A 418 elsewhere paused all Binance traffic: the request is not even attempted.
        let key = LimitKey::Binance {
            host: "limited-send-test.example".into(),
            weight: 1,
            account: None,
        };
        binance_set_ban(
            AccountEnv::Live,
            Instant::now(),
            Some(std::time::Duration::from_secs(30)),
        );
        let url = reqwest::Url::parse("http://127.0.0.1:1/x").unwrap();
        let client = reqwest::Client::new();
        // A sync test: the guard on the process-wide ban must not live across an `.await`.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let err = rt
            .block_on(crate::account::send_limited(
                &client,
                &key,
                Priority::Protective,
                reqwest::Method::DELETE,
                url,
                reqwest::header::HeaderMap::new(),
                None,
            ))
            .unwrap_err();
        assert_eq!(err, crate::account::AccountError::LocalRateLimited);
        assert_eq!(err.delivery(), crate::account::Delivery::NotSent);
        binance_clear_ban_for_tests();
    }
}
