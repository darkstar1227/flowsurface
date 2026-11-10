//! The send layer (stage a3): signed placing, cancelling and reading for Binance and OKX.
//!
//! Everything that changes an order goes through [`TradeClient::submit`] or
//! [`TradeClient::submit_auto`] (the single gate, condition 72). Reads take builder requests
//! only. Nothing here retries a send: a refused, unsent or unknown request is reported, never
//! repeated (conditions 39, 60, 76).
//!
//! Tests point the client at a local fake server through a `#[cfg(test)]` seam; there is no
//! public, feature or environment switch that changes the host (the host whitelist in
//! `account::https_hosts` stays the only way out).

use super::client_id::ClientId;
use super::decimal::Decimal;
use super::gate::{
    ApprovedOrder, AutoIntent, ConfirmToken, OrderIntent, Prepared, RiskHook, SendGate, Spent,
    SubmitError, Target, approve,
};
use super::locks::{self, Locked};
use super::outcome::{Outcome, from_binance, from_binance_bulk, from_okx};
use super::queries;
use super::request::{BinanceRequest, Exposure, Lookup, LookupKind, Method, OkxRequest};
use super::rules::{FreshRules, RulesTable, TradeProduct, ValidSymbol};
use super::types::{Dir, MarginMode, TradeError};
use crate::account::{
    AccountEnv, AccountError, AccountKey, AccountProduct, AccountVenue, ApiCredentials, ProxyState,
    binance_hosts, check_proxy_snapshot, https_endpoint, okx_headers, okx_timestamp,
    send_limited_meta, sensitive,
};
use crate::rate_limit::{LimitKey, OkxEndpoint, Priority, apply_exchange_info_limits};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use secrecy::ExposeSecret;
use serde_json::Value;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock};

/// Binance `recvWindow` for orders (condition 76: at most 5000).
pub(super) const RECV_WINDOW_MS: u32 = 5000;

/// How long trading rules stay usable after they were fetched (conditions 62, F5). Fixed
/// here: a caller cannot stretch it.
pub const MAX_RULES_AGE_MS: i64 = 60_000;

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct Seam {
    /// `http://127.0.0.1:port`
    pub(crate) base: String,
    /// Stands in for the real host in the limiter, so tests do not share its state.
    pub(crate) limit_host: String,
}

/// An error from a read (never from a send: sends report an [`Outcome`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    Transport(AccountError),
    /// The response was not in the expected shape.
    Parse,
    Trade(TradeError),
}

impl From<AccountError> for ReadError {
    fn from(e: AccountError) -> Self {
        ReadError::Transport(e)
    }
}

impl From<TradeError> for ReadError {
    fn from(e: TradeError) -> Self {
        ReadError::Trade(e)
    }
}

/// What a send returned, plus the follow-up an `Unknown` outcome needs.
#[derive(Debug)]
pub struct Submitted {
    pub outcome: Outcome,
    /// Set when the outcome is `Unknown`: pass it to `TradeClient::resolve_unknown`.
    pub pending: Option<super::unknown::PendingUnknown>,
}

pub struct TradeClient {
    pub(super) http: reqwest::Client,
    pub(super) venue: AccountVenue,
    pub(super) env: AccountEnv,
    pub(super) product: AccountProduct,
    pub(super) account: AccountKey,
    creds: ApiCredentials,
    proxy_snapshot: ProxyState,
    current_proxy: Arc<dyn Fn() -> ProxyState + Send + Sync>,
    clock_offset_ms: AtomicI64,
    #[cfg(test)]
    pub(crate) seam: Option<Seam>,
    /// Test clock: when set, `now_ms` is this plus `test_elapsed_ms`, and waits advance it
    /// instead of sleeping.
    #[cfg(test)]
    pub(crate) test_now_ms: Option<i64>,
    #[cfg(test)]
    pub(crate) test_elapsed_ms: AtomicI64,
}

impl TradeClient {
    /// `http` must be the signed client (no redirects, https only); `proxy_snapshot` is what
    /// it was built with and `current_proxy` reads the setting now, compared before every
    /// request.
    pub fn new(
        http: reqwest::Client,
        venue: AccountVenue,
        env: AccountEnv,
        product: AccountProduct,
        account: AccountKey,
        creds: ApiCredentials,
        proxy_snapshot: ProxyState,
        current_proxy: Arc<dyn Fn() -> ProxyState + Send + Sync>,
    ) -> Self {
        Self {
            http,
            venue,
            env,
            product,
            account,
            creds,
            proxy_snapshot,
            current_proxy,
            clock_offset_ms: AtomicI64::new(0),
            #[cfg(test)]
            seam: None,
            #[cfg(test)]
            test_now_ms: None,
            #[cfg(test)]
            test_elapsed_ms: AtomicI64::new(0),
        }
    }

    pub fn account(&self) -> AccountKey {
        self.account
    }

    /// The client's own clock (local time plus the measured server offset). The gate, the
    /// rules age and the Unknown window all use it; callers cannot supply a time (F5).
    pub(super) fn now_ms(&self) -> i64 {
        #[cfg(test)]
        if let Some(base) = self.test_now_ms {
            return base + self.test_elapsed_ms.load(Ordering::Relaxed);
        }
        chrono::Utc::now().timestamp_millis() + self.clock_offset_ms.load(Ordering::Relaxed)
    }

    /// Monotonic milliseconds for waiting windows: immune to wall-clock jumps and to
    /// `sync_clock` changing the offset. Under test it follows the test clock.
    pub(crate) fn mono_ms(&self) -> i64 {
        #[cfg(test)]
        if let Some(base) = self.test_now_ms {
            return base + self.test_elapsed_ms.load(Ordering::Relaxed);
        }
        static START: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);
        i64::try_from(START.elapsed().as_millis()).unwrap_or(i64::MAX)
    }

    /// Wait `d`. Under test the clock moves instead, so windows of tens of seconds cost nothing.
    #[cfg(not(test))]
    pub(super) async fn wait(&self, d: std::time::Duration) {
        tokio::time::sleep(d).await;
    }

    #[cfg(test)]
    pub(super) async fn wait(&self, d: std::time::Duration) {
        let ms = i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
        self.test_elapsed_ms.fetch_add(ms, Ordering::Relaxed);
        tokio::task::yield_now().await;
    }

    /// Look a symbol up in `table`, proving the rules are fresh by this client's clock.
    pub fn fresh<'a>(
        &self,
        table: &'a RulesTable,
        symbol: &str,
    ) -> Result<FreshRules<'a>, TradeError> {
        table.fresh_lookup(symbol, self.now_ms(), MAX_RULES_AGE_MS)
    }

    fn target(&self) -> Target {
        Target {
            account: self.account,
            venue: self.venue,
            env: self.env,
            product: self.product,
        }
    }

    pub fn clock_offset_ms(&self) -> i64 {
        self.clock_offset_ms.load(Ordering::Relaxed)
    }

    // ── Clock (condition 76) ────────────────────────────────────────────

    /// Re-measure server time minus local time. Call at startup, every few minutes and
    /// after the machine wakes from sleep. A failure keeps the old offset.
    pub async fn sync_clock(&self) -> Result<i64, AccountError> {
        check_proxy_snapshot(&self.proxy_snapshot, &(self.current_proxy)())?;
        let local = chrono::Utc::now().timestamp_millis();
        let (host, path) = match self.venue {
            AccountVenue::Binance => {
                let (_, product_host, _) = binance_hosts(self.env, self.product);
                let path = match self.product {
                    AccountProduct::Spot => "/api/v3/time",
                    AccountProduct::UsdM => "/fapi/v1/time",
                };
                (product_host, path)
            }
            AccountVenue::Okx => ("www.okx.com", "/api/v5/public/time"),
        };
        let url = self.url(host, path)?;
        let key = match self.venue {
            AccountVenue::Binance => LimitKey::binance(&self.limit_host(host), 1),
            AccountVenue::Okx => LimitKey::Okx(OkxEndpoint::AccountConfig),
        };
        let (result, _) = send_limited_meta(
            &self.http,
            &key,
            Priority::Query,
            reqwest::Method::GET,
            url,
            HeaderMap::new(),
            None,
        )
        .await;
        let v = result?;
        let server = match self.venue {
            AccountVenue::Binance => v.get("serverTime").and_then(Value::as_i64),
            AccountVenue::Okx => v["data"][0]["ts"].as_str().and_then(|t| t.parse().ok()),
        }
        .ok_or(AccountError::BadResponse)?;
        let offset = server - local;
        self.clock_offset_ms.store(offset, Ordering::Relaxed);
        Ok(offset)
    }

    // ── Plumbing ────────────────────────────────────────────────────────

    fn limit_host(&self, host: &str) -> String {
        #[cfg(test)]
        if let Some(s) = &self.seam {
            return s.limit_host.clone();
        }
        host.to_string()
    }

    fn url(&self, host: &str, path_and_query: &str) -> Result<reqwest::Url, AccountError> {
        #[cfg(test)]
        if let Some(s) = &self.seam {
            return reqwest::Url::parse(&format!("{}{path_and_query}", s.base))
                .map_err(|_| AccountError::InvalidEndpoint);
        }
        https_endpoint(self.venue, self.env, self.product, host, path_and_query)
    }

    /// Sign and send one Binance request. The proxy snapshot is compared first.
    async fn call_binance(
        &self,
        req: &BinanceRequest,
        priority: Priority,
    ) -> (Result<Value, AccountError>, Option<u16>) {
        if let Err(e) = check_proxy_snapshot(&self.proxy_snapshot, &(self.current_proxy)()) {
            return (Err(e), None);
        }
        let (_, host, _) = binance_hosts(self.env, self.product);
        let signed = match req.signed(
            self.creds.secret().expose_secret(),
            RECV_WINDOW_MS,
            self.now_ms(),
        ) {
            Ok(s) => s,
            Err(_) => return (Err(AccountError::InvalidEndpoint), None),
        };
        let mut headers = HeaderMap::new();
        match sensitive(self.creds.key().expose_secret()) {
            Ok(v) => headers.insert("X-MBX-APIKEY", v),
            Err(e) => return (Err(e), None),
        };
        let (method, url, body) = match req.method() {
            Method::Post => {
                headers.insert(
                    CONTENT_TYPE,
                    HeaderValue::from_static("application/x-www-form-urlencoded"),
                );
                (
                    reqwest::Method::POST,
                    self.url(host, req.path()),
                    Some(signed.body),
                )
            }
            Method::Get => (
                reqwest::Method::GET,
                self.url(host, &format!("{}?{}", req.path(), signed.body)),
                None,
            ),
            Method::Delete => (
                reqwest::Method::DELETE,
                self.url(host, &format!("{}?{}", req.path(), signed.body)),
                None,
            ),
        };
        let url = match url {
            Ok(u) => u,
            Err(e) => return (Err(e), None),
        };
        let key = LimitKey::Binance {
            host: self.limit_host(host),
            weight: binance_weight(req.path(), req.symbol().is_some(), self.product),
            account: Some(self.account),
        };
        send_limited_meta(&self.http, &key, priority, method, url, headers, body).await
    }

    async fn call_okx(
        &self,
        req: &OkxRequest,
        priority: Priority,
    ) -> (Result<Value, AccountError>, Option<u16>) {
        if let Err(e) = check_proxy_snapshot(&self.proxy_snapshot, &(self.current_proxy)()) {
            return (Err(e), None);
        }
        let Some(passphrase) = self.creds.passphrase() else {
            return (Err(AccountError::InvalidCredentials), None);
        };
        let ts = okx_timestamp(
            chrono::DateTime::from_timestamp_millis(self.now_ms()).unwrap_or_default(),
        );
        let signed = match req.signed(self.creds.secret().expose_secret(), &ts) {
            Ok(s) => s,
            Err(_) => return (Err(AccountError::InvalidEndpoint), None),
        };
        let mut headers = match okx_headers(
            self.env,
            self.creds.key().expose_secret(),
            &signed.sign,
            &ts,
            passphrase.expose_secret(),
        ) {
            Ok(h) => h,
            Err(e) => return (Err(e), None),
        };
        let (method, body) = match req.method() {
            Method::Post => {
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                (reqwest::Method::POST, Some(signed.body))
            }
            Method::Get => (reqwest::Method::GET, None),
            Method::Delete => (reqwest::Method::DELETE, None),
        };
        let url = match self.url("www.okx.com", req.path()) {
            Ok(u) => u,
            Err(e) => return (Err(e), None),
        };
        let key = LimitKey::Okx(okx_endpoint(req));
        send_limited_meta(&self.http, &key, priority, method, url, headers, body).await
    }

    // ── The gate ────────────────────────────────────────────────────────

    /// Run the single gate and send. `token` is the user's confirmation, spent here.
    pub async fn submit<G: SendGate>(
        &self,
        gate: &G,
        risk: &dyn RiskHook,
        token: ConfirmToken,
        intent: OrderIntent,
    ) -> Result<Submitted, SubmitError<G::Refusal>> {
        self.submit_spent(gate, risk, token.spend(), intent).await
    }

    /// An automatic path (condition 71): pre-authorised, and only ever reducing.
    pub async fn submit_auto<G: SendGate>(
        &self,
        gate: &G,
        risk: &dyn RiskHook,
        auto: AutoIntent,
    ) -> Result<Submitted, SubmitError<G::Refusal>> {
        log::info!("auto order path {:?}", auto.path);
        self.submit_spent(gate, risk, Spent::pre_authorised(), auto.intent)
            .await
    }

    pub(super) async fn submit_spent<G: SendGate>(
        &self,
        gate: &G,
        risk: &dyn RiskHook,
        spent: Spent,
        intent: OrderIntent,
    ) -> Result<Submitted, SubmitError<G::Refusal>> {
        // An unresolved Unknown locks new entries for that (account, symbol). Reducing and
        // cancelling stay possible so a position can always be cut (condition 60).
        self.target().check(&intent)?;
        if matches!(intent.exposure(), Exposure::Adds { .. }) && self.is_locked(&intent.symbol) {
            return Err(SubmitError::Locked);
        }
        let approved = approve(gate, risk, spent, intent, self.now_ms(), self.target())?;
        self.send_approved(approved)
            .await
            .map_err(|Locked| SubmitError::Locked)
    }

    /// The only function that sends an order, cancel or close. Crate-private: it takes an
    /// [`ApprovedOrder`], which only the gate can make.
    pub(crate) async fn send_approved(&self, approved: ApprovedOrder) -> Result<Submitted, Locked> {
        let ApprovedOrder { intent, priority } = approved;
        // An entry is marked in flight before it goes out (F2): a second entry on the same
        // symbol is refused until this one returns, and a send that is dropped half way
        // leaves the symbol locked.
        let inflight = match intent.exposure() {
            Exposure::Adds { .. } => Some(locks::reserve(self.account, &intent.symbol)?),
            Exposure::Reduces | Exposure::Neutral => None,
        };
        let lookup: Option<Lookup> = match &intent.prepared {
            Prepared::Binance(r) => r.lookup(),
            Prepared::Okx(r) => r.lookup(),
        };
        let client_id = lookup.as_ref().map(|l| l.client_id.clone());
        let outcome = match &intent.prepared {
            Prepared::Binance(r) => {
                let (result, status) = self.call_binance(r, priority).await;
                if is_binance_bulk(r.path()) {
                    from_binance_bulk(result, status)
                } else {
                    from_binance(result, status, client_id.as_deref())
                }
            }
            Prepared::Okx(r) => {
                let (result, status) = self.call_okx(r, priority).await;
                let require_id = r.exposure() != Exposure::Neutral;
                from_okx(result, status, client_id.as_deref(), require_id)
            }
        };
        let unknown = lock_on_unknown(
            matches!(outcome, Outcome::Unknown { .. }),
            inflight.is_some(),
            lookup.is_some(),
        );
        match inflight {
            Some(g) => g.finish(unknown),
            None if unknown => locks::lock_unknown(self.account, &intent.symbol),
            None => {}
        }
        let pending = match (&outcome, lookup) {
            (Outcome::Unknown { .. }, Some(lookup)) => Some(super::unknown::PendingUnknown {
                symbol: intent.symbol.clone(),
                lookup,
                rules: intent.rules.clone(),
                // After the response: later than the request's own timestamp, so the wait
                // before "not found" counts is never too short.
                sent_mono_ms: self.mono_ms(),
            }),
            _ => None,
        };
        Ok(Submitted { outcome, pending })
    }

    /// True while a send for this (account, symbol) has an unresolved outcome or an entry in
    /// flight. The table is shared by every client of the same account.
    pub fn is_locked(&self, symbol: &str) -> bool {
        locks::is_locked(self.account, symbol)
    }

    pub(super) fn unlock(&self, symbol: &str) {
        locks::unlock_unknown(self.account, symbol);
    }

    /// Look one order up by its client id. `Ok(true)` when the exchange knows it, `Ok(false)`
    /// when the exchange answered that it does not (a verified not-found code, or no data).
    /// Throttling, timeouts and every other exchange error are `Err`, never "not found".
    pub async fn query_order(
        &self,
        fresh: &super::rules::FreshRules<'_>,
        kind: LookupKind,
        client_id: &ClientId,
    ) -> Result<bool, ReadError> {
        let lookup = Lookup {
            kind,
            client_id: client_id.as_str().to_string(),
        };
        // Same classification as the Unknown flow: only a verified "no such order" answer is
        // `Ok(false)`; any other exchange error stays an error.
        self.query_lookup(fresh.rules(), &lookup).await
    }

    // ── Reads ───────────────────────────────────────────────────────────

    /// A signed GET built by one of the builders.
    pub(super) async fn read_binance(&self, req: &BinanceRequest) -> Result<Value, ReadError> {
        debug_assert_eq!(req.exposure(), Exposure::Neutral);
        Ok(self.call_binance(req, Priority::Query).await.0?)
    }

    pub(super) async fn read_okx(&self, req: &OkxRequest) -> Result<Vec<Value>, ReadError> {
        let (result, _) = self.call_okx(req, Priority::Query).await;
        okx_data(result?)
    }

    /// Open orders of every category: plain, algo and (spot) order lists.
    pub async fn open_orders(&self) -> Result<Vec<OpenOrder>, ReadError> {
        let mut out = Vec::new();
        match (self.venue, self.product) {
            (AccountVenue::Binance, AccountProduct::UsdM) => {
                let v = self
                    .read_binance(&queries::futures_open_orders(None))
                    .await?;
                out.extend(parse_open(
                    &list_of(&v)?,
                    "symbol",
                    "orderId",
                    OpenKind::Plain,
                ));
                let v = self
                    .read_binance(&queries::futures_open_algo_orders(None))
                    .await?;
                out.extend(parse_open(
                    &list_of(&v)?,
                    "symbol",
                    "algoId",
                    OpenKind::Algo(None),
                ));
            }
            (AccountVenue::Binance, AccountProduct::Spot) => {
                let v = self.read_binance(&queries::spot_open_orders(None)).await?;
                out.extend(parse_open(
                    &list_of(&v)?,
                    "symbol",
                    "orderId",
                    OpenKind::Plain,
                ));
                let v = self.read_binance(&queries::spot_open_order_lists()).await?;
                out.extend(parse_open_lists(&list_of(&v)?));
            }
            (AccountVenue::Okx, _) => {
                let inst = match self.product {
                    AccountProduct::Spot => queries::OkxInstType::Spot,
                    AccountProduct::UsdM => queries::OkxInstType::Swap,
                };
                let data = self.read_okx(&queries::okx_pending_orders(inst)).await?;
                out.extend(parse_open(&data, "instId", "ordId", OpenKind::Plain));
                for t in [
                    super::okx::OkxAlgoType::Conditional,
                    super::okx::OkxAlgoType::Oco,
                ] {
                    let data = self.read_okx(&queries::okx_pending_algos(t)?).await?;
                    out.extend(parse_open(
                        &data,
                        "instId",
                        "algoId",
                        OpenKind::Algo(Some(t)),
                    ));
                }
            }
        }
        Ok(out)
    }

    pub async fn positions(&self) -> Result<Vec<Position>, ReadError> {
        match (self.venue, self.product) {
            (AccountVenue::Binance, AccountProduct::UsdM) => {
                let v = self.read_binance(&queries::futures_positions()).await?;
                Ok(list_of(&v)?
                    .iter()
                    .filter_map(parse_binance_position)
                    .collect())
            }
            (AccountVenue::Binance, AccountProduct::Spot) => Ok(Vec::new()),
            (AccountVenue::Okx, _) => {
                let data = self.read_okx(&queries::okx_positions()).await?;
                Ok(data.iter().filter_map(parse_okx_position).collect())
            }
        }
    }

    /// Free balances per asset (after fees, available to sell).
    pub async fn balances(&self) -> Result<Vec<Balance>, ReadError> {
        match (self.venue, self.product) {
            (AccountVenue::Binance, AccountProduct::Spot) => {
                let v = self.read_binance(&queries::spot_account()).await?;
                let arr = v
                    .get("balances")
                    .and_then(Value::as_array)
                    .ok_or(ReadError::Parse)?;
                Ok(parse_balances(arr, "asset", "free"))
            }
            (AccountVenue::Binance, AccountProduct::UsdM) => {
                let v = self.read_binance(&queries::futures_balance()).await?;
                Ok(parse_balances(&list_of(&v)?, "asset", "availableBalance"))
            }
            (AccountVenue::Okx, _) => {
                let data = self.read_okx(&queries::okx_balance()).await?;
                let details = data
                    .first()
                    .and_then(|d| d.get("details"))
                    .and_then(Value::as_array)
                    .ok_or(ReadError::Parse)?;
                Ok(parse_balances(details, "ccy", "availBal"))
            }
        }
    }

    /// Leverage and margin mode the exchange has for `symbol` (PRD 3.2: compare with what
    /// the order form assumes before sending).
    pub async fn settings(
        &self,
        symbol: &ValidSymbol,
        okx_margin: MarginMode,
    ) -> Result<TradeSettings, ReadError> {
        match (self.venue, self.product) {
            (AccountVenue::Binance, AccountProduct::UsdM) => {
                let v = self.read_binance(&queries::futures_positions()).await?;
                let found = list_of(&v)?
                    .iter()
                    .filter_map(parse_binance_position)
                    .find(|p| p.symbol == symbol.as_str());
                found
                    .map(|p| TradeSettings {
                        leverage: p.leverage,
                        margin_mode: p.margin,
                    })
                    // `positionRisk` v3 lists only symbols with a position or open order.
                    .ok_or(ReadError::Parse)
            }
            (AccountVenue::Okx, _) => {
                let data = self
                    .read_okx(&queries::okx_leverage_info(symbol, okx_margin))
                    .await?;
                let d = data.first().ok_or(ReadError::Parse)?;
                Ok(TradeSettings {
                    leverage: d["lever"].as_str().and_then(|s| s.parse().ok()),
                    margin_mode: d["mgnMode"].as_str().and_then(parse_margin),
                })
            }
            _ => Err(ReadError::Trade(TradeError::Unsupported)),
        }
    }

    // ── Trading rules (public) ──────────────────────────────────────────

    /// Fetch the trading rules of one product from the exchange. This is the only way to get
    /// a [`RulesTable`]: tables cannot be built from caller-supplied JSON. Also reads the
    /// order rate limits (`rateLimits`) into the limiter for Binance.
    pub async fn fetch_rules(&self, product: TradeProduct) -> Result<RulesTable, ReadError> {
        check_proxy_snapshot(&self.proxy_snapshot, &(self.current_proxy)())?;
        let now = chrono::Utc::now().timestamp_millis();
        let expected = match (self.venue, self.product) {
            (AccountVenue::Binance, AccountProduct::Spot) => TradeProduct::BinanceSpot,
            (AccountVenue::Binance, AccountProduct::UsdM) => TradeProduct::BinanceUsdM,
            (AccountVenue::Okx, _) => product,
        };
        if expected != product {
            return Err(TradeError::ProductMismatch.into());
        }
        match product {
            TradeProduct::BinanceSpot | TradeProduct::BinanceUsdM => {
                let (_, host, _) = binance_hosts(self.env, self.product);
                let path = if product == TradeProduct::BinanceSpot {
                    "/api/v3/exchangeInfo"
                } else {
                    "/fapi/v1/exchangeInfo"
                };
                let weight = if product == TradeProduct::BinanceSpot {
                    20
                } else {
                    1
                };
                let key = LimitKey::binance(&self.limit_host(host), weight);
                let (result, _) = send_limited_meta(
                    &self.http,
                    &key,
                    Priority::Query,
                    reqwest::Method::GET,
                    self.url(host, path)?,
                    HeaderMap::new(),
                    None,
                )
                .await;
                let info = result?;
                apply_exchange_info_limits(&self.limit_host(host), &info);
                Ok(RulesTable::from_binance_exchange_info(product, &info, now)?)
            }
            TradeProduct::OkxSpot | TradeProduct::OkxSwap => {
                let inst = if product == TradeProduct::OkxSpot {
                    "SPOT"
                } else {
                    "SWAP"
                };
                let path = format!("/api/v5/public/instruments?instType={inst}");
                let (result, _) = send_limited_meta(
                    &self.http,
                    &LimitKey::Okx(OkxEndpoint::Instruments),
                    Priority::Query,
                    reqwest::Method::GET,
                    self.url("www.okx.com", &path)?,
                    HeaderMap::new(),
                    None,
                )
                .await;
                let resp = result?;
                if resp.get("code").and_then(Value::as_str) != Some("0") {
                    return Err(ReadError::Parse);
                }
                Ok(RulesTable::from_okx_instruments(product, &resp, now)?)
            }
        }
    }
}

// ── Weights and endpoints ───────────────────────────────────────────────

/// IP weight of a Binance endpoint (from the docs; unlisted endpoints count 1).
fn binance_weight(path: &str, has_symbol: bool, product: AccountProduct) -> usize {
    match (path, product) {
        ("/fapi/v1/openOrders" | "/fapi/v1/openAlgoOrders", _) => {
            if has_symbol {
                1
            } else {
                40
            }
        }
        ("/fapi/v3/positionRisk" | "/fapi/v3/balance", _) => 5,
        ("/api/v3/account", _) => 20,
        ("/api/v3/openOrders", _) => {
            if has_symbol {
                6
            } else {
                80
            }
        }
        ("/api/v3/openOrderList", _) => 6,
        _ => 1,
    }
}

fn is_binance_bulk(path: &str) -> bool {
    matches!(
        path,
        "/fapi/v1/allOpenOrders" | "/fapi/v1/algoOpenOrders" | "/api/v3/openOrders"
    )
}

/// The `instId` of an OKX order request, from the body or from the GET query.
pub(super) fn okx_inst_id(req: &OkxRequest) -> Option<&str> {
    req.symbol().or_else(|| {
        let query = req.path().split_once('?')?.1;
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix("instId="))
            .filter(|v| !v.is_empty())
    })
}

/// Whether an unknown outcome leaves the symbol locked. An entry (`adds`, i.e. it holds an
/// in-flight mark) always locks, whether or not it has a lookup; anything else only locks when
/// there is a lookup to resolve it by (a cancel has none and must not lock).
pub(super) fn lock_on_unknown(is_unknown: bool, adds: bool, has_lookup: bool) -> bool {
    is_unknown && (adds || has_lookup)
}

pub(super) fn okx_endpoint(req: &OkxRequest) -> OkxEndpoint {
    let path = req.path().split('?').next().unwrap_or_default();
    match path {
        "/api/v5/trade/order-algo" => OkxEndpoint::OrderAlgo,
        "/api/v5/trade/cancel-algos" => OkxEndpoint::CancelAlgos,
        "/api/v5/trade/orders-algo-pending" => OkxEndpoint::OrdersAlgoPending,
        "/api/v5/trade/orders-pending" => OkxEndpoint::OrdersPending,
        "/api/v5/account/positions" | "/api/v5/account/leverage-info" => OkxEndpoint::Positions,
        "/api/v5/account/balance" => OkxEndpoint::Balance,
        "/api/v5/trade/order" | "/api/v5/trade/cancel-order" | "/api/v5/trade/close-position" => {
            // POST has the instId in its body, GET carries it in the query.
            OkxEndpoint::Order(okx_inst_id(req).unwrap_or("?").to_string())
        }
        _ => OkxEndpoint::AccountConfig,
    }
}

// ── Parsed read results ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenKind {
    Plain,
    /// OKX knows which algo type (conditional / oco); Binance does not need it.
    Algo(Option<super::okx::OkxAlgoType>),
    /// Binance spot order list (OCO / OTOCO).
    List,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenOrder {
    pub symbol: String,
    pub id: String,
    pub kind: OpenKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub symbol: String,
    pub dir: Dir,
    /// Absolute size (contracts for OKX).
    pub qty: Decimal,
    /// Hedge-style entry (`LONG`/`SHORT` / `long`/`short`) rather than one-way.
    pub hedge_side: bool,
    pub margin: Option<MarginMode>,
    pub leverage: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Balance {
    pub asset: String,
    pub free: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TradeSettings {
    pub leverage: Option<u32>,
    pub margin_mode: Option<MarginMode>,
}

/// The exchange's leverage and margin mode must equal what the order form assumes (PRD 3.2).
pub fn settings_match(
    expected_leverage: u32,
    expected_margin: MarginMode,
    actual: &TradeSettings,
) -> bool {
    actual.leverage == Some(expected_leverage) && actual.margin_mode == Some(expected_margin)
}

fn parse_margin(s: &str) -> Option<MarginMode> {
    match s.to_ascii_lowercase().as_str() {
        "cross" | "crossed" => Some(MarginMode::Cross),
        "isolated" => Some(MarginMode::Isolated),
        _ => None,
    }
}

/// An array body, or the first array-valued field of an object (the algo-order list's shape is
/// not verified against the docs copy).
fn list_of(v: &Value) -> Result<Vec<Value>, ReadError> {
    match v {
        Value::Array(a) => Ok(a.clone()),
        Value::Object(o) => o
            .values()
            .find_map(|x| x.as_array().cloned())
            .ok_or(ReadError::Parse),
        _ => Err(ReadError::Parse),
    }
}

fn okx_data(v: Value) -> Result<Vec<Value>, ReadError> {
    if v.get("code").and_then(Value::as_str) != Some("0") {
        let code = v
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        return Err(ReadError::Transport(AccountError::Exchange {
            code: super::outcome::clean_code(&code),
            msg: String::new(),
        }));
    }
    v.get("data")
        .and_then(Value::as_array)
        .cloned()
        .ok_or(ReadError::Parse)
}

fn text(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn parse_open(items: &[Value], sym: &str, id: &str, kind: OpenKind) -> Vec<OpenOrder> {
    items
        .iter()
        .filter_map(|o| {
            let symbol = text(o, sym)?;
            crate::signing::validate_symbol(&symbol).ok()?;
            Some(OpenOrder {
                symbol,
                id: text(o, id)?,
                kind: kind.clone(),
            })
        })
        .collect()
}

fn parse_open_lists(items: &[Value]) -> Vec<OpenOrder> {
    items
        .iter()
        .filter_map(|o| {
            let symbol = text(o, "symbol")?;
            crate::signing::validate_symbol(&symbol).ok()?;
            Some(OpenOrder {
                symbol,
                id: text(o, "orderListId")?,
                kind: OpenKind::List,
            })
        })
        .collect()
}

fn parse_balances(items: &[Value], asset: &str, free: &str) -> Vec<Balance> {
    items
        .iter()
        .filter_map(|b| {
            Some(Balance {
                asset: text(b, asset)?,
                free: Decimal::parse(&text(b, free)?).ok()?,
            })
        })
        .collect()
}

/// Split a signed size string into direction (by sign) and absolute value.
fn signed_size(raw: &str) -> Option<(bool, Decimal)> {
    let d = Decimal::parse(raw).ok()?;
    if d.is_zero() {
        return None;
    }
    let negative = d.is_negative();
    Some((negative, d.checked_abs().ok()?))
}

fn parse_binance_position(p: &Value) -> Option<Position> {
    let symbol = text(p, "symbol")?;
    crate::signing::validate_symbol(&symbol).ok()?;
    let amt = text(p, "positionAmt")?;
    let (negative, qty) = signed_size(&amt)?;
    let (dir, hedge_side) = match p.get("positionSide").and_then(Value::as_str) {
        Some("LONG") => (Dir::Long, true),
        Some("SHORT") => (Dir::Short, true),
        _ => (if negative { Dir::Short } else { Dir::Long }, false),
    };
    Some(Position {
        symbol,
        dir,
        qty,
        hedge_side,
        margin: p
            .get("marginType")
            .and_then(Value::as_str)
            .and_then(parse_margin),
        leverage: text(p, "leverage").and_then(|s| s.parse().ok()),
    })
}

fn parse_okx_position(p: &Value) -> Option<Position> {
    let symbol = text(p, "instId")?;
    crate::signing::validate_symbol(&symbol).ok()?;
    let (negative, qty) = signed_size(&text(p, "pos")?)?;
    let (dir, hedge_side) = match p.get("posSide").and_then(Value::as_str) {
        Some("long") => (Dir::Long, true),
        Some("short") => (Dir::Short, true),
        _ => (if negative { Dir::Short } else { Dir::Long }, false),
    };
    Some(Position {
        symbol,
        dir,
        qty,
        hedge_side,
        margin: p
            .get("mgnMode")
            .and_then(Value::as_str)
            .and_then(parse_margin),
        leverage: text(p, "lever").and_then(|s| s.parse().ok()),
    })
}
