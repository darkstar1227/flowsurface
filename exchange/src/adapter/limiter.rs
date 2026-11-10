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
        if let Ok(current_time) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        {
            let now = Instant::now();
            let period_seconds = self.refill_rate.as_secs();
            let seconds_in_current_period = current_time.as_secs() % period_seconds;

            let elapsed = now.duration_since(self.last_refill);
            if elapsed >= self.refill_rate || seconds_in_current_period < 1 {
                self.available_tokens = self.max_tokens;
                self.last_refill = now;
            }
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

    fn prepare_with_header_data(
        &self,
        weight: usize,
    ) -> (Option<Duration>, Option<DynamicLimitReason>) {
        let available = self.max_weight.saturating_sub(self.current_used_weight);

        if available >= weight {
            return (None, None);
        }

        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();

        let period_seconds = self.refill_rate.as_secs();
        let seconds_in_period = current_time.as_secs() % period_seconds;
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

/// Rate state of one Binance host, shared by public market-data fetches and signed
/// requests so both draw from the same IP weight.
pub struct BinanceHostState {
    pub(crate) bucket: DynamicBucket,
    /// A 429 on a signed request pauses signed requests only; public fetches keep their
    /// existing behaviour.
    pub(crate) signed_pause_until: Option<Instant>,
    pub(crate) order_counts: OrderCounts,
    /// `(per 10 s, per 1 m)` order limits, if known for this host.
    pub(crate) order_limits: Option<(usize, usize)>,
}

pub type SharedBinanceHost = Arc<Mutex<BinanceHostState>>;

static BINANCE_HOSTS: LazyLock<Mutex<HashMap<String, SharedBinanceHost>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// IP ban (HTTP 418) end, for every Binance host. While set, nothing is sent to Binance.
static BINANCE_BAN_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// USDⓈ-M order limits `(per 10 s, per 1 m)`. Not verified against the docs yet: a3 must
/// replace them with the values from `exchangeInfo` `rateLimits` before orders are enabled.
pub const FUTURES_ORDER_LIMITS: (usize, usize) = (300, 1200);

/// Ban length used when the 418 response carries no usable `Retry-After`.
const DEFAULT_BAN: Duration = Duration::from_secs(120);

/// Remaining Binance IP-ban time, if any.
pub fn binance_ban_remaining(now: Instant) -> Option<Duration> {
    let mut guard = lock(&BINANCE_BAN_UNTIL);
    match *guard {
        Some(until) if until > now => Some(until - now),
        Some(_) => {
            *guard = None;
            None
        }
        None => None,
    }
}

/// Start (or extend) the global Binance pause. Never shortens an existing ban.
pub fn binance_set_ban(now: Instant, retry_after: Option<Duration>) {
    let until = pause_until(now, retry_after.unwrap_or(DEFAULT_BAN));
    let mut guard = lock(&BINANCE_BAN_UNTIL);
    if guard.is_none_or(|cur| until > cur) {
        *guard = Some(until);
    }
}

#[cfg(test)]
pub(crate) fn binance_clear_ban_for_tests() {
    *lock(&BINANCE_BAN_UNTIL) = None;
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
        config: DynamicRateLimiterConfig,
        order_limits: Option<(usize, usize)>,
    ) -> Self {
        let keep_ratio = (1.0 - config.limiter_buffer_pct).clamp(0.0, 1.0);
        let effective_limit = (config.max_weight as f32 * keep_ratio) as usize;
        Self {
            bucket: DynamicBucket::new(effective_limit, config.refill_rate),
            signed_pause_until: None,
            order_counts: OrderCounts::default(),
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
    let order_limits = matches!(host, "fapi.binance.com" | "testnet.binancefuture.com")
        .then_some(FUTURES_ORDER_LIMITS);
    let mut map = lock(&BINANCE_HOSTS);
    Arc::clone(
        map.entry(host.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(BinanceHostState::new(config, order_limits)))),
    )
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
        // An IP ban (418) pauses every Binance request, public fetches included.
        if let Some(remaining) = binance_ban_remaining(Instant::now()) {
            return Some(remaining);
        }
        let (wait_time, _reason) = lock(&self.shared).bucket.prepare_request(weight);
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
        binance_ban_remaining(Instant::now())
    }

    fn should_exit_on_response(&self, response: &reqwest::Response) -> bool {
        let status = response.status();
        if Some(status) == self.extra_exit_status && status == reqwest::StatusCode::IM_A_TEAPOT {
            binance_set_ban(Instant::now(), retry_after(response));
        }
        status == self.exit_status || Some(status) == self.extra_exit_status
    }
}
