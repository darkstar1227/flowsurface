use crate::account::{AccountEnv, AccountKey};
use reqwest::Response;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Lock that survives a poisoned mutex: the limiter state is plain counters, so continuing
/// is safe and better than taking the whole adapter down.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub trait RateLimiter: Send + Sync {
    /// Prepare for a request with given weight. Returns wait time if needed
    fn prepare_request(&mut self, weight: usize) -> Option<Duration>;

    /// Update the limiter with response data (e.g., rate limit headers)
    fn update_from_response(&mut self, response: &Response, weight: usize);

    /// Check if response indicates rate limiting and should exit
    fn should_exit_on_response(&self, response: &Response) -> bool;

    /// Time left on a ban that must not be waited out inside a fetch worker (e.g. a Binance
    /// IP ban). The caller returns an error instead of sleeping.
    fn ban_remaining(&self) -> Option<Duration> {
        None
    }
}

/// Seconds since the Unix epoch, used to align windows with the exchange's wall-clock minute.
#[cfg(not(test))]
fn wall_clock_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        // A clock before the epoch is treated as mid-period: no boundary refill (fail closed).
        .map_or(1, |d| d.as_secs())
}

// Tests never read the wall clock: a run that crosses a minute boundary would otherwise
// refill buckets mid-test. The default sits mid-minute; tests can move it explicitly.
#[cfg(test)]
thread_local! {
    static TEST_WALL_CLOCK_SECS: std::cell::Cell<u64> = const { std::cell::Cell::new(30) };
}

#[cfg(test)]
fn wall_clock_secs() -> u64 {
    TEST_WALL_CLOCK_SECS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn set_wall_clock_secs_for_tests(secs: u64) {
    TEST_WALL_CLOCK_SECS.with(|c| c.set(secs));
}

/// Limiter for a fixed window rate
pub struct FixedWindowBucket {
    max_tokens: usize,
    available_tokens: usize,
    last_refill: Instant,
    refill_rate: Duration,
}

impl FixedWindowBucket {
    pub fn new(max_tokens: usize, refill_rate: Duration) -> Self {
        Self {
            max_tokens,
            available_tokens: max_tokens,
            last_refill: Instant::now(),
            refill_rate,
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let period_seconds = self.refill_rate.as_secs();
        let seconds_in_current_period = wall_clock_secs() % period_seconds;

        let elapsed = now.duration_since(self.last_refill);
        if elapsed >= self.refill_rate || seconds_in_current_period < 1 {
            self.available_tokens = self.max_tokens;
            self.last_refill = now;
        }
    }

    pub fn calculate_wait_time(&mut self, tokens: usize) -> Option<Duration> {
        self.refill();

        if self.available_tokens >= tokens {
            self.available_tokens -= tokens;
            return None;
        }

        let wait_time = self
            .refill_rate
            .saturating_sub(Instant::now().duration_since(self.last_refill));
        Some(wait_time)
    }

    fn refill_and_used(&mut self) -> usize {
        self.refill();
        self.max_tokens - self.available_tokens
    }

    pub fn consume_tokens(&mut self, tokens: usize) {
        self.refill();
        self.available_tokens -= tokens.min(self.available_tokens);
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DynamicLimitReason {
    HeaderRate,
    FixedWindowRate,
}

/// Limiter that can be used when source reports the rate-limit usage
///
/// Can fallback to fixed window bucket
pub struct DynamicBucket {
    max_weight: usize,
    current_used_weight: usize,
    last_updated: Instant,
    refill_rate: Duration,
    fallback_bucket: FixedWindowBucket,
}

impl DynamicBucket {
    pub fn new(max_weight: usize, refill_rate: Duration) -> Self {
        Self {
            max_weight,
            current_used_weight: 0,
            last_updated: Instant::now(),
            refill_rate,
            fallback_bucket: FixedWindowBucket::new(max_weight, refill_rate),
        }
    }

    /// Best estimate of the weight used in the current window: the exchange-reported value
    /// while it is fresh, otherwise what the fallback bucket has handed out.
    pub fn used_estimate(&mut self) -> usize {
        if self.last_updated.elapsed() <= self.refill_rate && self.current_used_weight > 0 {
            self.current_used_weight
        } else {
            self.fallback_bucket.refill_and_used()
        }
    }

    pub fn max_weight(&self) -> usize {
        self.max_weight
    }

    pub fn update_weight(&mut self, new_weight: usize) {
        if new_weight > 0 {
            self.current_used_weight = new_weight;
            self.last_updated = Instant::now();
        }
    }

    pub fn prepare_request(
        &mut self,
        weight: usize,
    ) -> (Option<Duration>, Option<DynamicLimitReason>) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_updated);

        if elapsed <= self.refill_rate && self.current_used_weight > 0 {
            self.prepare_with_header_data(weight)
        } else {
            self.prepare_with_fallback(weight)
        }
    }

    /// Time until the exchange's current weight window ends.
    pub fn wait_for_refill(&self) -> Duration {
        let period_seconds = self.refill_rate.as_secs().max(1);
        let seconds_in_period = wall_clock_secs() % period_seconds;
        Duration::from_secs(period_seconds - seconds_in_period)
            .saturating_add(Duration::from_millis(500))
    }

    fn prepare_with_header_data(
        &self,
        weight: usize,
    ) -> (Option<Duration>, Option<DynamicLimitReason>) {
        let available = self.max_weight.saturating_sub(self.current_used_weight);

        if available >= weight {
            return (None, None);
        }

        let period_seconds = self.refill_rate.as_secs();
        let seconds_in_period = wall_clock_secs() % period_seconds;
        let wait_time = Duration::from_secs(period_seconds - seconds_in_period)
            .saturating_add(Duration::from_millis(500));

        (Some(wait_time), Some(DynamicLimitReason::HeaderRate))
    }

    fn prepare_with_fallback(
        &mut self,
        weight: usize,
    ) -> (Option<Duration>, Option<DynamicLimitReason>) {
        match self.fallback_bucket.calculate_wait_time(weight) {
            None => (None, None),
            Some(wait_time) => (Some(wait_time), Some(DynamicLimitReason::FixedWindowRate)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FixedWindowRateLimiterConfig {
    pub limit: usize,
    pub refill_rate: Duration,
    pub limiter_buffer_pct: f32,
    pub exit_status: reqwest::StatusCode,
}

impl FixedWindowRateLimiterConfig {
    pub fn new(
        limit: usize,
        refill_rate: Duration,
        limiter_buffer_pct: f32,
        exit_status: reqwest::StatusCode,
    ) -> Self {
        Self {
            limit,
            refill_rate,
            limiter_buffer_pct,
            exit_status,
        }
    }
}

pub struct FixedWindowRateLimiter {
    bucket: FixedWindowBucket,
    exit_status: reqwest::StatusCode,
}

impl FixedWindowRateLimiter {
    pub fn new(config: FixedWindowRateLimiterConfig) -> Self {
        let keep_ratio = (1.0 - config.limiter_buffer_pct).clamp(0.0, 1.0);
        let effective_limit = (config.limit as f32 * keep_ratio) as usize;

        Self {
            bucket: FixedWindowBucket::new(effective_limit, config.refill_rate),
            exit_status: config.exit_status,
        }
    }
}

impl RateLimiter for FixedWindowRateLimiter {
    fn prepare_request(&mut self, weight: usize) -> Option<Duration> {
        self.bucket.calculate_wait_time(weight)
    }

    fn update_from_response(&mut self, _response: &reqwest::Response, weight: usize) {
        self.bucket.consume_tokens(weight);
    }

    fn should_exit_on_response(&self, response: &reqwest::Response) -> bool {
        response.status() == self.exit_status
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DynamicRateLimiterConfig {
    pub max_weight: usize,
    pub refill_rate: Duration,
    pub limiter_buffer_pct: f32,
    pub used_weight_header: &'static str,
    pub exit_status: reqwest::StatusCode,
    pub extra_exit_status: Option<reqwest::StatusCode>,
}

impl DynamicRateLimiterConfig {
    pub fn new(
        max_weight: usize,
        refill_rate: Duration,
        limiter_buffer_pct: f32,
        used_weight_header: &'static str,
        exit_status: reqwest::StatusCode,
        extra_exit_status: Option<reqwest::StatusCode>,
    ) -> Self {
        Self {
            max_weight,
            refill_rate,
            limiter_buffer_pct,
            used_weight_header,
            exit_status,
            extra_exit_status,
        }
    }
}

/// Counters of the exchange-reported order rate (`X-MBX-ORDER-COUNT-*`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OrderCounts {
    pub per_10s: usize,
    pub per_1m: usize,
}

/// Order counters as of `at`: the exchange reports them per account, and the 10 s / 1 m
/// windows have passed after that long, so a stale reading counts as zero.
#[derive(Debug, Clone, Copy)]
struct CountsReading {
    counts: OrderCounts,
    at: Instant,
}

/// Rate state of one Binance host, shared by public market-data fetches and signed
/// requests so both draw from the same IP weight.
pub struct BinanceHostState {
    pub(crate) bucket: DynamicBucket,
    /// Which environment the host belongs to; an IP ban (418) is tracked per environment.
    pub(crate) env: AccountEnv,
    /// A 429 on a signed request pauses signed requests only; public fetches keep their
    /// existing behaviour.
    pub(crate) signed_pause_until: Option<Instant>,
    /// A 429 on a public fetch pauses new entries and queries, not protective requests.
    pub(crate) public_pause_until: Option<Instant>,
    /// Exchange-reported order counts, one reading per account (`None` = requests that
    /// carry no account, such as the connection test).
    order_counts: HashMap<Option<AccountKey>, CountsReading>,
    /// `(per 10 s, per 1 m)` order limits from `exchangeInfo` `rateLimits`. `None` until
    /// the rules have been fetched: new entries are refused meanwhile.
    pub(crate) order_limits: Option<OrderLimits>,
}

impl BinanceHostState {
    /// Order counts of `account` at `now`; windows that have fully elapsed read as zero.
    pub(crate) fn counts_for(&self, account: Option<AccountKey>, now: Instant) -> OrderCounts {
        let Some(r) = self.order_counts.get(&account) else {
            return OrderCounts::default();
        };
        let age = now.saturating_duration_since(r.at);
        OrderCounts {
            per_10s: if age >= Duration::from_secs(10) {
                0
            } else {
                r.counts.per_10s
            },
            per_1m: if age >= Duration::from_secs(60) {
                0
            } else {
                r.counts.per_1m
            },
        }
    }

    /// Store a reading; a header that is missing keeps the account's current value.
    pub(crate) fn set_counts(
        &mut self,
        account: Option<AccountKey>,
        per_10s: Option<usize>,
        per_1m: Option<usize>,
        now: Instant,
    ) {
        let mut counts = self.counts_for(account, now);
        if let Some(n) = per_10s {
            counts.per_10s = n;
        }
        if let Some(n) = per_1m {
            counts.per_1m = n;
        }
        self.order_counts
            .insert(account, CountsReading { counts, at: now });
    }

    /// Entries and queries are held back while a public 429 pause is running.
    pub(crate) fn public_pause_remaining(&mut self, now: Instant) -> Option<Duration> {
        match self.public_pause_until {
            Some(until) if until > now => Some(until - now),
            Some(_) => {
                self.public_pause_until = None;
                None
            }
            None => None,
        }
    }

    pub(crate) fn set_public_pause(&mut self, now: Instant, duration: Duration) {
        let until = pause_until(now, duration);
        if self.public_pause_until.is_none_or(|cur| until > cur) {
            self.public_pause_until = Some(until);
        }
    }
}

/// Order-count limits from `exchangeInfo`; a window the exchange does not list is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderLimits {
    pub per_10s: Option<usize>,
    pub per_1m: Option<usize>,
}

impl OrderLimits {
    #[cfg(test)]
    pub(crate) fn both(per_10s: usize, per_1m: usize) -> Self {
        Self {
            per_10s: Some(per_10s),
            per_1m: Some(per_1m),
        }
    }
}

pub type SharedBinanceHost = Arc<Mutex<BinanceHostState>>;

static BINANCE_HOSTS: LazyLock<Mutex<HashMap<String, SharedBinanceHost>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// IP ban (HTTP 418) end, one per environment (Live, Testnet): a ban on the testnet must not
/// stop Live requests and the other way round. While set, nothing is sent to that
/// environment's Binance hosts.
struct BanTable(Mutex<[Option<Instant>; 2]>);

impl BanTable {
    const fn new() -> Self {
        Self(Mutex::new([None, None]))
    }

    fn remaining(&self, env: AccountEnv, now: Instant) -> Option<Duration> {
        let mut guard = lock(&self.0);
        let slot = &mut guard[env_slot(env)];
        match *slot {
            Some(until) if until > now => Some(until - now),
            Some(_) => {
                *slot = None;
                None
            }
            None => None,
        }
    }

    /// Start (or extend) the ban. Never shortens an existing one.
    fn set(&self, env: AccountEnv, now: Instant, retry_after: Option<Duration>) {
        let until = pause_until(now, retry_after.unwrap_or(DEFAULT_BAN));
        let mut guard = lock(&self.0);
        let slot = &mut guard[env_slot(env)];
        if slot.is_none_or(|cur| until > cur) {
            *slot = Some(until);
        }
    }

    #[cfg(test)]
    fn clear(&self) {
        *lock(&self.0) = [None, None];
    }
}

#[cfg(not(test))]
static BINANCE_BAN: BanTable = BanTable::new();

// Under test the ban is per thread: every test runs on its own thread, so a 418 simulated by
// one test cannot stop the requests of another one running at the same time. The logic is
// the same `BanTable` the release build shares across threads.
#[cfg(test)]
thread_local! {
    static BINANCE_BAN_TEST: BanTable = const { BanTable::new() };
}

fn with_ban<T>(f: impl FnOnce(&BanTable) -> T) -> T {
    #[cfg(not(test))]
    {
        f(&BINANCE_BAN)
    }
    #[cfg(test)]
    {
        BINANCE_BAN_TEST.with(f)
    }
}

fn env_slot(env: AccountEnv) -> usize {
    match env {
        AccountEnv::Live => 0,
        AccountEnv::Testnet => 1,
    }
}

/// The environment a Binance REST host belongs to.
pub fn binance_env_of_host(host: &str) -> AccountEnv {
    match host {
        "testnet.binance.vision" | "testnet.binancefuture.com" => AccountEnv::Testnet,
        _ => AccountEnv::Live,
    }
}

/// Ban length used when the 418 response carries no usable `Retry-After`.
const DEFAULT_BAN: Duration = Duration::from_secs(120);

/// Remaining Binance IP-ban time of `env`, if any.
pub fn binance_ban_remaining(env: AccountEnv, now: Instant) -> Option<Duration> {
    with_ban(|t| t.remaining(env, now))
}

/// Start (or extend) the Binance pause of `env`. Never shortens an existing ban.
pub fn binance_set_ban(env: AccountEnv, now: Instant, retry_after: Option<Duration>) {
    with_ban(|t| t.set(env, now, retry_after));
}

#[cfg(test)]
pub(crate) fn binance_clear_ban_for_tests() {
    with_ban(BanTable::clear);
}

fn retry_after(response: &Response) -> Option<Duration> {
    response
        .headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Longest pause taken from a server-supplied `Retry-After`.
pub const MAX_PAUSE: Duration = Duration::from_secs(3600);

/// `now + duration`, with `duration` capped at [`MAX_PAUSE`]. Never panics on overflow.
pub(crate) fn pause_until(now: Instant, duration: Duration) -> Instant {
    let capped = duration.min(MAX_PAUSE);
    now.checked_add(capped).unwrap_or(now)
}

impl BinanceHostState {
    pub(crate) fn new(
        env: AccountEnv,
        config: DynamicRateLimiterConfig,
        order_limits: Option<OrderLimits>,
    ) -> Self {
        let keep_ratio = (1.0 - config.limiter_buffer_pct).clamp(0.0, 1.0);
        let effective_limit = (config.max_weight as f32 * keep_ratio) as usize;
        Self {
            bucket: DynamicBucket::new(effective_limit, config.refill_rate),
            env,
            signed_pause_until: None,
            public_pause_until: None,
            order_counts: HashMap::new(),
            order_limits,
        }
    }
}

/// The limiter config for a Binance `host`, identical to the one the public fetch hubs use.
pub fn default_binance_config(host: &str) -> DynamicRateLimiterConfig {
    use super::hub::binance::{
        LIMITER_BUFFER_PCT, PERPS_LIMIT, REFILL_RATE, SPOT_LIMIT, USED_WEIGHT_HEADER,
    };
    let max_weight = match host {
        "api.binance.com" | "testnet.binance.vision" => SPOT_LIMIT,
        _ => PERPS_LIMIT,
    };
    DynamicRateLimiterConfig::new(
        max_weight,
        REFILL_RATE,
        LIMITER_BUFFER_PCT,
        USED_WEIGHT_HEADER,
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        Some(reqwest::StatusCode::IM_A_TEAPOT),
    )
}

/// The shared state for `host`, created with `config` on first use. Later callers get the
/// same instance whatever config they pass.
pub fn binance_host_state(host: &str, config: DynamicRateLimiterConfig) -> SharedBinanceHost {
    let env = binance_env_of_host(host);
    let mut map = lock(&BINANCE_HOSTS);
    Arc::clone(map.entry(host.to_string()).or_insert_with(|| {
        // The order limits stay unknown until `exchangeInfo` has been read (`rateLimits`).
        Arc::new(Mutex::new(BinanceHostState::new(env, config, None)))
    }))
}

pub struct HeaderDynamicRateLimiter {
    shared: SharedBinanceHost,
    used_weight_header: &'static str,
    exit_status: reqwest::StatusCode,
    extra_exit_status: Option<reqwest::StatusCode>,
}

impl HeaderDynamicRateLimiter {
    /// A limiter drawing from `shared`, so other users of the same host (signed requests)
    /// consume the same weight.
    pub fn new_shared(config: DynamicRateLimiterConfig, shared: SharedBinanceHost) -> Self {
        Self {
            shared,
            used_weight_header: config.used_weight_header,
            exit_status: config.exit_status,
            extra_exit_status: config.extra_exit_status,
        }
    }
}

impl RateLimiter for HeaderDynamicRateLimiter {
    fn prepare_request(&mut self, weight: usize) -> Option<Duration> {
        let now = Instant::now();
        let mut state = lock(&self.shared);
        // An IP ban (418) pauses every request to that environment, public fetches included.
        if let Some(remaining) = binance_ban_remaining(state.env, now) {
            return Some(remaining);
        }
        // A public 429 also holds back signed entries and queries (not protective requests).
        if let Some(remaining) = state.public_pause_remaining(now) {
            return Some(remaining);
        }
        // Public market data stops at the Query share, so it cannot use up the part of the
        // budget kept for protective requests (condition 55).
        let cap = crate::rate_limit::Priority::Query.cap(state.bucket.max_weight());
        if state.bucket.used_estimate() + weight > cap {
            return Some(state.bucket.wait_for_refill());
        }
        let (wait_time, _reason) = state.bucket.prepare_request(weight);
        wait_time
    }

    fn update_from_response(&mut self, response: &reqwest::Response, _weight: usize) {
        if let Some(header_value) = response
            .headers()
            .get(self.used_weight_header)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
        {
            lock(&self.shared).bucket.update_weight(header_value);
        }
    }

    fn ban_remaining(&self) -> Option<Duration> {
        let env = lock(&self.shared).env;
        binance_ban_remaining(env, Instant::now())
    }

    fn should_exit_on_response(&self, response: &reqwest::Response) -> bool {
        let status = response.status();
        let mut state = lock(&self.shared);
        if Some(status) == self.extra_exit_status && status == reqwest::StatusCode::IM_A_TEAPOT {
            binance_set_ban(state.env, Instant::now(), retry_after(response));
        } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            state.set_public_pause(
                Instant::now(),
                retry_after(response).unwrap_or(Duration::from_secs(10)),
            );
        }
        status == self.exit_status || Some(status) == self.extra_exit_status
    }
}

#[cfg(test)]
mod ban_tests {
    use super::*;

    /// The table the release build shares across threads: a ban set on one thread is seen on
    /// another, it is per environment, and it is never shortened.
    #[test]
    fn a_ban_is_shared_across_threads_per_env_and_never_shortened() {
        let table = BanTable::new();
        let now = Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| table.set(AccountEnv::Testnet, now, Some(Duration::from_secs(30))));
        });
        assert!(table.remaining(AccountEnv::Testnet, now).is_some());
        assert_eq!(table.remaining(AccountEnv::Live, now), None);
        // a shorter ban does not shorten the existing one
        table.set(AccountEnv::Testnet, now, Some(Duration::from_secs(5)));
        let left = table.remaining(AccountEnv::Testnet, now).unwrap();
        assert!(left > Duration::from_secs(20), "{left:?}");
        // it ends by itself
        assert_eq!(
            table.remaining(AccountEnv::Testnet, now + Duration::from_secs(31)),
            None
        );
        table.set(AccountEnv::Live, now, None);
        assert!(table.remaining(AccountEnv::Live, now).is_some());
        table.clear();
        assert_eq!(table.remaining(AccountEnv::Live, now), None);
    }
}
