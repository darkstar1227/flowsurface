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
use super::grant::GrantedAuto;
use super::journal::UnknownJournal;
use super::locks;
use super::outcome::{Outcome, from_binance, from_binance_bulk, from_okx};
use super::queries;
use super::request::{BinanceRequest, Exposure, Lookup, LookupKind, Method, OkxRequest};
use super::risk::{AutoCloseDecision, EquityBasis, EquityObservation, EquityTracker, RiskSnapshot};
use super::rules::{FreshRules, RulesTable, TradeProduct, ValidSymbol};
use super::types::{Dir, MarginMode, Side, TradeError};
use super::unknown::UnknownRecord;
use crate::account::{
    AccountEnv, AccountError, AccountKey, AccountProduct, AccountVenue, ApiCredentials, ProxyState,
    binance_hosts, check_proxy_snapshot, https_endpoint, okx_headers, okx_timestamp,
    send_limited_meta, sensitive,
};
use crate::rate_limit::{LimitKey, OkxEndpoint, Priority, apply_exchange_info_limits};
use crate::signed_http::SignedHttp;
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

/// Why an approved order was not sent.
pub(crate) enum NotSent {
    /// An earlier send for this account and symbol is unresolved or in flight.
    Locked,
    /// The write-ahead record could not be made.
    Journal,
}

/// What a send returned, plus the follow-up an `Unknown` outcome needs.
#[derive(Debug)]
pub struct Submitted {
    pub outcome: Outcome,
    /// Set when the outcome is `Unknown`: pass it to `TradeClient::resolve_unknown`.
    pub pending: Option<super::unknown::PendingUnknown>,
    /// A reducing, closing or protective send went out although its write-ahead record could
    /// not be written (only new entries are refused for that, review B5). If the outcome is
    /// `Unknown` and the app restarts, nothing remembers it: show a standing warning that the
    /// journal is broken (review N8).
    pub journal_failed: bool,
}

pub struct TradeClient {
    pub(super) http: SignedHttp,
    pub(super) venue: AccountVenue,
    pub(super) env: AccountEnv,
    pub(super) product: AccountProduct,
    pub(super) account: AccountKey,
    creds: ApiCredentials,
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
    /// `http` is the signed client together with the proxy it was built with (see
    /// [`SignedHttp`]); `current_proxy` reads the setting now and is compared with that proxy
    /// before every request. It must read the live setting, never a copy taken earlier.
    ///
    /// A bare `reqwest::Client` is not accepted, so a direct client cannot be paired with a
    /// `Proxied` snapshot:
    ///
    /// ```compile_fail,E0308
    /// use flowsurface_exchange::account::{AccountEnv, AccountKey, AccountProduct, AccountVenue, ApiCredentials};
    /// use flowsurface_exchange::trade::TradeClient;
    /// let _ = TradeClient::new(
    ///     reqwest::Client::new(),
    ///     AccountVenue::Okx,
    ///     AccountEnv::Live,
    ///     AccountProduct::UsdM,
    ///     AccountKey::from_bytes([1; 16]),
    ///     ApiCredentials::new("k".into(), "s".into(), None),
    ///     std::sync::Arc::new(|| flowsurface_exchange::account::ProxyState::Direct),
    /// );
    /// ```
    pub fn new(
        http: SignedHttp,
        venue: AccountVenue,
        env: AccountEnv,
        product: AccountProduct,
        account: AccountKey,
        creds: ApiCredentials,
        current_proxy: Arc<dyn Fn() -> ProxyState + Send + Sync>,
    ) -> Self {
        Self {
            http,
            venue,
            env,
            product,
            account,
            creds,
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
        check_proxy_snapshot(self.http.proxy_state(), &(self.current_proxy)())?;
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
            self.http.client(),
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
        if let Err(e) = check_proxy_snapshot(self.http.proxy_state(), &(self.current_proxy)()) {
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
        send_limited_meta(
            self.http.client(),
            &key,
            priority,
            method,
            url,
            headers,
            body,
        )
        .await
    }

    async fn call_okx(
        &self,
        req: &OkxRequest,
        priority: Priority,
    ) -> (Result<Value, AccountError>, Option<u16>) {
        if let Err(e) = check_proxy_snapshot(self.http.proxy_state(), &(self.current_proxy)()) {
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
        send_limited_meta(
            self.http.client(),
            &key,
            priority,
            method,
            url,
            headers,
            body,
        )
        .await
    }

    // ── Private stream support (stage b) ────────────────────────────────

    /// Set the server-time offset measured elsewhere (the stream task shares one offset).
    pub(super) fn set_clock_offset_ms(&self, offset: i64) {
        self.clock_offset_ms.store(offset, Ordering::Relaxed);
    }

    /// A USDⓈ-M user-data-stream call (`POST`/`PUT`/`DELETE /fapi/v1/listenKey`, condition 49).
    /// These endpoints take the API key only: no signature, no parameters (the account has one
    /// key, `POST` returns it again and extends it). The request goes through the shared limiter
    /// at IP weight 1, with protective priority so extending the key is never starved. The
    /// response is returned parsed; the caller turns it into a `ListenKey` at once.
    pub(super) async fn listen_key_call(
        &self,
        method: reqwest::Method,
    ) -> Result<Value, AccountError> {
        check_proxy_snapshot(self.http.proxy_state(), &(self.current_proxy)())?;
        if (self.venue, self.product) != (AccountVenue::Binance, AccountProduct::UsdM) {
            return Err(AccountError::InvalidEndpoint);
        }
        let (_, host, _) = binance_hosts(self.env, self.product);
        let mut headers = HeaderMap::new();
        headers.insert("X-MBX-APIKEY", sensitive(self.creds.key().expose_secret())?);
        let url = self.url(host, "/fapi/v1/listenKey")?;
        let key = LimitKey::Binance {
            host: self.limit_host(host),
            weight: 1,
            account: Some(self.account),
        };
        send_limited_meta(
            self.http.client(),
            &key,
            Priority::Protective,
            method,
            url,
            headers,
            None,
        )
        .await
        .0
    }

    // ── The gate ────────────────────────────────────────────────────────

    /// Run the single gate and send. `token` is the user's confirmation, spent here.
    ///
    /// `journal` records a send that may end `Unknown` before it goes out (see [`journal`]).
    /// Only a **new entry** is refused when that write fails (`JournalUnavailable`, nothing
    /// sent). A reducing, closing or protective send goes out anyway, and
    /// [`Submitted::journal_failed`] tells the caller the record is missing.
    ///
    /// [`journal`]: super::journal
    pub async fn submit<G: SendGate>(
        &self,
        gate: &G,
        risk: &dyn RiskHook,
        journal: &dyn UnknownJournal,
        token: ConfirmToken,
        intent: OrderIntent,
    ) -> Result<Submitted, SubmitError<G::Refusal>> {
        let spent = token
            .spend_order()
            .map_err(|_| SubmitError::ConfirmationMismatch)?;
        self.submit_spent(gate, risk, journal, spent, intent).await
    }

    /// An automatic path (condition 71): only what an [`AutoGrant`](super::AutoGrant) covers,
    /// and only ever reducing. The request was authorised by the grant (`AutoGrant::authorise`),
    /// which is the only way to make a [`GrantedAuto`].
    pub async fn submit_auto<G: SendGate>(
        &self,
        gate: &G,
        risk: &dyn RiskHook,
        journal: &dyn UnknownJournal,
        granted: GrantedAuto,
    ) -> Result<Submitted, SubmitError<G::Refusal>> {
        self.submit_auto_unbound(gate, risk, journal, granted.auto)
            .await
    }

    /// The risk-control close (`AutoPath::RiskClose`): built by this crate only, so it has no
    /// entry confirmation to bind to.
    pub(super) async fn submit_auto_unbound<G: SendGate>(
        &self,
        gate: &G,
        risk: &dyn RiskHook,
        journal: &dyn UnknownJournal,
        auto: AutoIntent,
    ) -> Result<Submitted, SubmitError<G::Refusal>> {
        log::info!("auto order path {:?}", auto.path);
        self.submit_spent(gate, risk, journal, Spent::pre_authorised(), auto.intent)
            .await
    }

    pub(super) async fn submit_spent<G: SendGate>(
        &self,
        gate: &G,
        risk: &dyn RiskHook,
        journal: &dyn UnknownJournal,
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
        self.send_approved(approved, journal)
            .await
            .map_err(|e| match e {
                NotSent::Locked => SubmitError::Locked,
                NotSent::Journal => SubmitError::JournalUnavailable,
            })
    }

    /// The only function that sends an order, cancel or close. Crate-private: it takes an
    /// [`ApprovedOrder`], which only the gate can make.
    pub(crate) async fn send_approved(
        &self,
        approved: ApprovedOrder,
        journal: &dyn UnknownJournal,
    ) -> Result<Submitted, NotSent> {
        let ApprovedOrder { intent, priority } = approved;
        // An entry is marked in flight before it goes out (F2): a second entry on the same
        // symbol is refused until this one returns, and a send that is dropped half way
        // leaves the symbol locked.
        let lookup: Option<Lookup> = match &intent.prepared {
            Prepared::Binance(r) => r.lookup(),
            Prepared::Okx(r) => r.lookup(),
        };
        let client_id = lookup.as_ref().map(|l| l.client_id.clone());
        let adds = matches!(intent.exposure(), Exposure::Adds { .. });
        let inflight = if adds {
            Some(
                locks::reserve(self.account, &intent.symbol, client_id.as_deref())
                    .map_err(|_| NotSent::Locked)?,
            )
        } else {
            None
        };
        // Write ahead (review B4). Only a new entry is refused when the write fails (B5):
        // reducing, closing and protective sends always go out (conditions 60 and 72), so a
        // full disk or a corrupt file can never stop a position from being cut. For those the
        // write is best effort, and an in-memory lock still covers an Unknown outcome.
        let mut journal_failed = false;
        let journaled = match &lookup {
            Some(l) => {
                let record =
                    UnknownRecord::for_send(self.account, &intent.symbol, l, self.now_ms());
                if journal.record(&record).is_ok() {
                    true
                } else if adds {
                    // Nothing was sent: release the in-flight mark cleanly. Dropping the armed
                    // guard would lock the symbol as Unknown.
                    if let Some(g) = inflight {
                        g.finish(false, None);
                    }
                    return Err(NotSent::Journal);
                } else {
                    log::error!("could not write ahead a non-entry send: sending anyway");
                    journal_failed = true;
                    false
                }
            }
            None => false,
        };
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
        let is_unknown = matches!(outcome, Outcome::Unknown { .. });
        let unknown = lock_on_unknown(is_unknown, inflight.is_some(), lookup.is_some());
        match inflight {
            Some(g) => g.finish(unknown, client_id.as_deref()),
            None if unknown => {
                locks::lock_unknown(self.account, &intent.symbol, client_id.as_deref())
            }
            None => {}
        }
        // The outcome is known: the record is no longer needed. An `Unknown` keeps it until the
        // lookup (or the user) resolves it. A failed clear leaves it behind, which is safe.
        if journaled
            && !is_unknown
            && let Some(id) = client_id.as_deref()
            && journal.clear(self.account, id).is_err()
        {
            log::warn!("could not clear the journal record of a settled send");
        }
        let pending = match (&outcome, lookup) {
            (Outcome::Unknown { .. }, Some(lookup)) => Some(super::unknown::PendingUnknown {
                account: self.account,
                symbol: intent.symbol.clone(),
                lookup,
                rules: intent.rules.clone(),
                // After the response: later than the request's own timestamp, so the wait
                // before "not found" counts is never too short.
                sent_mono_ms: self.mono_ms(),
            }),
            _ => None,
        };
        Ok(Submitted {
            outcome,
            pending,
            journal_failed,
        })
    }

    /// True while a send for this (account, symbol) has an unresolved outcome or an entry in
    /// flight. The table is shared by every client of the same account.
    pub fn is_locked(&self, symbol: &str) -> bool {
        locks::is_locked(self.account, symbol)
    }

    /// Release the lock of a resolved send, by its client id (see `locks::resolve_client_id`).
    pub(super) fn unlock(&self, symbol: &str, client_id: &str) -> bool {
        locks::resolve_client_id(self.account, symbol, client_id)
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
        self.read_binance_with(req, Priority::Query).await
    }

    async fn read_binance_with(
        &self,
        req: &BinanceRequest,
        priority: Priority,
    ) -> Result<Value, ReadError> {
        debug_assert_eq!(req.exposure(), Exposure::Neutral);
        Ok(self.call_binance(req, priority).await.0?)
    }

    pub(super) async fn read_okx(&self, req: &OkxRequest) -> Result<Vec<Value>, ReadError> {
        self.read_okx_with(req, Priority::Query).await
    }

    async fn read_okx_with(
        &self,
        req: &OkxRequest,
        priority: Priority,
    ) -> Result<Vec<Value>, ReadError> {
        let (result, _) = self.call_okx(req, priority).await;
        okx_data(result?)
    }

    /// Open orders of every category: plain, algo and (spot) order lists. Any failed read
    /// fails the whole call.
    pub async fn open_orders(&self) -> Result<Vec<OpenOrder>, ReadError> {
        let read = self.read_open_orders(Priority::Query, false).await?;
        debug_assert!(read.unknown_algo_types.is_empty());
        Ok(read.orders)
    }

    /// The read before a cancel-all: it uses the budget kept for cancels (user decision 6),
    /// and an OKX algo read that fails does not stop it. What was read is returned, and the
    /// types that could not be read are listed, so the caller cancels what it knows and marks
    /// the result as not clean.
    pub(super) async fn open_orders_for_cancel(&self) -> Result<OpenOrdersRead, ReadError> {
        self.read_open_orders(Priority::Protective, true).await
    }

    async fn read_open_orders(
        &self,
        priority: Priority,
        lenient: bool,
    ) -> Result<OpenOrdersRead, ReadError> {
        let mut out = Vec::new();
        let mut unknown_algo_types = Vec::new();
        match (self.venue, self.product) {
            (AccountVenue::Binance, AccountProduct::UsdM) => {
                let v = self
                    .read_binance_with(&queries::futures_open_orders(None), priority)
                    .await?;
                out.extend(parse_open(
                    &list_of(&v)?,
                    "symbol",
                    "orderId",
                    OpenKind::Plain,
                    Wire::Binance,
                ));
                let v = self
                    .read_binance_with(&queries::futures_open_algo_orders(None), priority)
                    .await?;
                out.extend(parse_open(
                    &list_of(&v)?,
                    "symbol",
                    "algoId",
                    OpenKind::Algo(None),
                    Wire::Binance,
                ));
            }
            (AccountVenue::Binance, AccountProduct::Spot) => {
                let v = self
                    .read_binance_with(&queries::spot_open_orders(None), priority)
                    .await?;
                out.extend(parse_open(
                    &list_of(&v)?,
                    "symbol",
                    "orderId",
                    OpenKind::Plain,
                    Wire::Binance,
                ));
                let v = self
                    .read_binance_with(&queries::spot_open_order_lists(), priority)
                    .await?;
                out.extend(parse_open_lists(&list_of(&v)?));
            }
            (AccountVenue::Okx, _) => {
                let inst = match self.product {
                    AccountProduct::Spot => queries::OkxInstType::Spot,
                    AccountProduct::UsdM => queries::OkxInstType::Swap,
                };
                let data = self
                    .read_okx_with(&queries::okx_pending_orders(inst), priority)
                    .await?;
                out.extend(parse_open(
                    &data,
                    "instId",
                    "ordId",
                    OpenKind::Plain,
                    Wire::Okx,
                ));
                for group in super::okx::OkxAlgoType::PENDING_READS {
                    // Chase orders exist on futures and swaps only.
                    let group: Vec<_> = group
                        .iter()
                        .copied()
                        .filter(|t| {
                            !(*t == super::okx::OkxAlgoType::Chase
                                && self.product == AccountProduct::Spot)
                        })
                        .collect();
                    if group.is_empty() {
                        continue;
                    }
                    let data = match self
                        .read_okx_with(&queries::okx_pending_algos(&group)?, priority)
                        .await
                    {
                        Ok(data) => data,
                        Err(_) if lenient => {
                            unknown_algo_types.extend(group);
                            continue;
                        }
                        Err(e) => return Err(e),
                    };
                    out.extend(parse_open_algos(&data, &group));
                }
            }
        }
        Ok(OpenOrdersRead {
            orders: out,
            unknown_algo_types,
        })
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

    /// Read the account's equity for the risk controls (REST source).
    ///
    /// Binance USDⓈ-M: `totalMarginBalance` of `GET /fapi/v3/account` (weight 5; USDT only in
    /// single-asset mode, USD-denominated in multi-assets mode). OKX: `totalEq` of
    /// `GET /api/v5/account/balance` (denominated in USD). Both field names are checked
    /// against the docs copies, **not** against a testnet or demo response. Binance spot has
    /// no equity source yet (`Unsupported`).
    ///
    /// A missing, empty or unparsable field is `ReadError::Parse`, never zero. The time is the
    /// client's clock when the response arrived. REST readings cannot say whether the change
    /// since the last one was a transfer, so `is_transfer` is always `false`: a withdrawal
    /// reads as a loss until the private stream marks transfers.
    pub async fn equity(&self) -> Result<EquityObservation, ReadError> {
        let (raw, basis) = match (self.venue, self.product) {
            (AccountVenue::Binance, AccountProduct::UsdM) => {
                let v = self.read_binance(&queries::futures_account()).await?;
                (
                    v.get("totalMarginBalance")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    EquityBasis::BinanceTotalMarginBalance,
                )
            }
            (AccountVenue::Okx, _) => {
                let data = self.read_okx(&queries::okx_balance()).await?;
                (
                    data.first()
                        .and_then(|d| d.get("totalEq"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    EquityBasis::OkxTotalEq,
                )
            }
            (AccountVenue::Binance, AccountProduct::Spot) => {
                return Err(ReadError::Trade(TradeError::Unsupported));
            }
        };
        let raw = raw.filter(|s| !s.is_empty()).ok_or(ReadError::Parse)?;
        let equity = Decimal::parse(&raw).map_err(|_| ReadError::Parse)?;
        Ok(EquityObservation::rest(self.now_ms(), equity, basis))
    }

    /// The risk hook for `tracker` at this client's clock, so callers never supply a time.
    pub fn risk_snapshot<'a>(&self, tracker: &'a EquityTracker) -> RiskSnapshot<'a> {
        tracker.snapshot(self.now_ms())
    }

    /// [`EquityTracker::begin_auto_close`] at this client's clock.
    pub fn begin_auto_close(
        &self,
        tracker: &mut EquityTracker,
        enabled: bool,
    ) -> AutoCloseDecision {
        tracker.begin_auto_close(self.now_ms(), enabled)
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
        check_proxy_snapshot(self.http.proxy_state(), &(self.current_proxy)())?;
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
                    self.http.client(),
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
                    self.http.client(),
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
        ("/fapi/v3/positionRisk" | "/fapi/v3/balance" | "/fapi/v3/account", _) => 5,
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

/// Open orders as read before a cancel-all.
#[derive(Debug)]
pub(super) struct OpenOrdersRead {
    pub orders: Vec<OpenOrder>,
    /// OKX algo types whose read failed: orders of these types may exist and are not listed.
    pub unknown_algo_types: Vec<super::okx::OkxAlgoType>,
}

/// One open order as read over REST. The fields after `kind` are what the protection status
/// needs (d1); each is `None` / `false` when the row does not carry it, and the status then
/// counts the order as not protecting anything (it never guesses).
#[derive(Clone, PartialEq, Eq)]
pub struct OpenOrder {
    pub symbol: String,
    pub id: String,
    pub kind: OpenKind,
    pub side: Option<Side>,
    /// The hedge-mode leg the order belongs to; `None` for one-way (`BOTH` / `net`) or unknown.
    pub position_side: Option<Dir>,
    pub qty: Option<Decimal>,
    /// The stop or take-profit trigger price (OKX: `slTriggerPx`, else `tpTriggerPx`).
    pub trigger_price: Option<Decimal>,
    /// Closes the whole position (Binance `closePosition`, OKX `closeFraction`).
    pub close_position: bool,
    pub reduce_only: bool,
    /// What kind of conditional order this is; `None` for anything else.
    pub role: Option<super::protect::ProtectRole>,
    /// OKX: a plain entry order that carries attached take-profit / stop-loss orders
    /// (`attachAlgoOrds`). They exist on the exchange but act only once the entry is fully
    /// filled; they are not in force, so this never makes a position read as protected.
    pub attached: Option<super::protect::ProtectRole>,
    /// The client id the order was sent under (Binance `clientOrderId` / `clientAlgoId`, OKX
    /// `clOrdId` / `algoClOrdId`); `None` when the row does not carry a readable one. It is
    /// never printed (see the `Debug` impl).
    pub client_id: Option<String>,
    /// The conditional order fills as a limit order once triggered (Binance `STOP` /
    /// `TAKE_PROFIT`, OKX an order price other than `-1`): it may not fill.
    pub limit: bool,
    /// That limit order's price, when it could be read.
    pub limit_price: Option<Decimal>,
    /// A trailing order's activation price, when it has one and it could be read.
    pub activation_price: Option<Decimal>,
    /// A trailing order's callback, as a percent, when it could be read.
    pub callback_rate: Option<Decimal>,
}

// The client id ties a log line to an order: it stays out of `{:?}` output.
impl std::fmt::Debug for OpenOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenOrder")
            .field("symbol", &self.symbol)
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("side", &self.side)
            .field("position_side", &self.position_side)
            .field("qty", &self.qty)
            .field("trigger_price", &self.trigger_price)
            .field("close_position", &self.close_position)
            .field("reduce_only", &self.reduce_only)
            .field("role", &self.role)
            .field("attached", &self.attached)
            .field("client_id", &self.client_id.as_ref().map(|_| "<redacted>"))
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl OpenOrder {
    fn bare(symbol: String, id: String, kind: OpenKind) -> Self {
        Self {
            symbol,
            id,
            kind,
            side: None,
            position_side: None,
            qty: None,
            trigger_price: None,
            close_position: false,
            reduce_only: false,
            role: None,
            attached: None,
            client_id: None,
            limit: false,
            limit_price: None,
            activation_price: None,
            callback_rate: None,
        }
    }
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

/// Which wire format a row is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    Binance,
    Okx,
}

fn flag(v: &Value, key: &str) -> bool {
    match v.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn decimal(v: &Value, key: &str) -> Option<Decimal> {
    Decimal::parse(&text(v, key)?).ok()
}

fn parse_row_side(v: &Value) -> Option<Side> {
    match v.get("side")?.as_str()?.to_ascii_lowercase().as_str() {
        "buy" => Some(Side::Buy),
        "sell" => Some(Side::Sell),
        _ => None,
    }
}

fn parse_row_position_side(v: &Value) -> Option<Dir> {
    let key = if v.get("positionSide").is_some() {
        "positionSide"
    } else {
        "posSide"
    };
    match v.get(key)?.as_str()?.to_ascii_lowercase().as_str() {
        "long" => Some(Dir::Long),
        "short" => Some(Dir::Short),
        _ => None,
    }
}

/// A client id longer than this is not read (an id of this app is 21 characters, exchanges allow
/// at most 36 to 64): the row then has no id.
const MAX_CLIENT_ID_LEN: usize = 64;

/// The protection fields of one row. Field names follow the exchange docs copy; the algo-order
/// list shapes are not verified against live responses (see the d1 report).
fn fill_protection(o: &mut OpenOrder, row: &Value, wire: Wire, is_algo: bool) {
    use super::protect::ProtectRole;
    o.side = parse_row_side(row);
    o.position_side = parse_row_position_side(row);
    o.reduce_only = flag(row, "reduceOnly");
    o.client_id = match (wire, is_algo) {
        (Wire::Binance, true) => text(row, "clientAlgoId"),
        (Wire::Binance, false) => text(row, "clientOrderId"),
        (Wire::Okx, true) => text(row, "algoClOrdId"),
        (Wire::Okx, false) => text(row, "clOrdId"),
    }
    .filter(|id| !id.is_empty() && id.len() <= MAX_CLIENT_ID_LEN);
    match wire {
        Wire::Binance => {
            o.qty = decimal(row, "origQty").or_else(|| decimal(row, "quantity"));
            o.trigger_price = decimal(row, "stopPrice").or_else(|| decimal(row, "triggerPrice"));
            o.close_position = flag(row, "closePosition");
            let ty = text(row, "orderType").or_else(|| text(row, "type"));
            // Only algo rows take part: a plain conditional row is never judged protective,
            // because a plain order event does not mark the lists stale (fail closed).
            o.role = match ty.as_deref() {
                _ if !is_algo => None,
                Some("STOP_MARKET" | "STOP") => Some(ProtectRole::Stop),
                Some("TAKE_PROFIT_MARKET" | "TAKE_PROFIT") => Some(ProtectRole::Target),
                Some("TRAILING_STOP_MARKET") => Some(ProtectRole::Trailing),
                _ => None,
            };
            o.limit = matches!(ty.as_deref(), Some("STOP" | "TAKE_PROFIT"));
            if o.limit {
                o.limit_price = decimal(row, "price");
            }
            o.activation_price =
                decimal(row, "activatePrice").or_else(|| decimal(row, "activationPrice"));
            o.callback_rate = decimal(row, "callbackRate");
        }
        Wire::Okx => {
            o.qty = decimal(row, "sz");
            o.close_position = decimal(row, "closeFraction").is_some_and(|d| {
                d.checked_cmp(&Decimal::from_int(1)) == Ok(std::cmp::Ordering::Equal)
            });
            if !is_algo {
                o.attached = parse_attached(row);
            }
            if is_algo {
                let sl = decimal(row, "slTriggerPx");
                let tp = decimal(row, "tpTriggerPx");
                o.trigger_price = sl.or(tp).or_else(|| decimal(row, "triggerPx"));
                // An order price of `-1` is a market order; any other value is a limit price.
                let ord_px = decimal(row, "slOrdPx").or_else(|| decimal(row, "tpOrdPx"));
                if let Some(px) = ord_px
                    && px.checked_cmp(&Decimal::from_int(-1)) != Ok(std::cmp::Ordering::Equal)
                {
                    o.limit = true;
                    o.limit_price = Some(px);
                }
                o.activation_price = decimal(row, "activePx");
                o.callback_rate = decimal(row, "callbackRatio").and_then(|r| {
                    // OKX gives a ratio (0.01 = 1%): shown as a percent.
                    r.checked_mul(&Decimal::from_int(100)).ok()
                });
                o.role = match text(row, "ordType").as_deref() {
                    Some("move_order_stop") => Some(ProtectRole::Trailing),
                    Some("conditional" | "oco") => match (sl.is_some(), tp.is_some()) {
                        (true, true) => Some(ProtectRole::StopAndTarget),
                        (true, false) => Some(ProtectRole::Stop),
                        (false, true) => Some(ProtectRole::Target),
                        (false, false) => None,
                    },
                    _ => None,
                };
            }
        }
    }
    // A role without a readable trigger price points at a field name that drifted: fail closed.
    // A trailing order has no trigger price of its own (it carries an activation price).
    if o.trigger_price.is_none() && o.role != Some(ProtectRole::Trailing) {
        o.role = None;
    }
}

/// The attached orders of an OKX entry row (`attachAlgoOrds`), as the kind they would be once
/// in force. An empty or missing list, or entries with no readable trigger, are `None`.
fn parse_attached(row: &Value) -> Option<super::protect::ProtectRole> {
    use super::protect::ProtectRole;
    let list = row.get("attachAlgoOrds")?.as_array()?;
    let (mut sl, mut tp) = (false, false);
    for a in list {
        sl |= decimal(a, "slTriggerPx").is_some();
        tp |= decimal(a, "tpTriggerPx").is_some();
    }
    match (sl, tp) {
        (true, true) => Some(ProtectRole::StopAndTarget),
        (true, false) => Some(ProtectRole::Stop),
        (false, true) => Some(ProtectRole::Target),
        (false, false) => None,
    }
}

fn parse_open(items: &[Value], sym: &str, id: &str, kind: OpenKind, wire: Wire) -> Vec<OpenOrder> {
    items
        .iter()
        .filter_map(|row| {
            let symbol = text(row, sym)?;
            crate::signing::validate_symbol(&symbol).ok()?;
            let is_algo = matches!(kind, OpenKind::Algo(_));
            let mut o = OpenOrder::bare(symbol, text(row, id)?, kind.clone());
            fill_protection(&mut o, row, wire, is_algo);
            Some(o)
        })
        .collect()
}

/// OKX algo rows of one read of `group`. The type comes from each row's `ordType`; a row
/// whose type is not one of the group's is kept with no type, so it is never cancelled under
/// the wrong endpoint but still listed as remaining.
fn parse_open_algos(items: &[Value], group: &[super::okx::OkxAlgoType]) -> Vec<OpenOrder> {
    items
        .iter()
        .flat_map(|o| {
            let kind = text(o, "ordType")
                .and_then(|n| super::okx::OkxAlgoType::from_pending_name(&n))
                .filter(|t| group.contains(t))
                .or_else(|| (group.len() == 1).then(|| group[0]));
            parse_open(
                std::slice::from_ref(o),
                "instId",
                "algoId",
                OpenKind::Algo(kind),
                Wire::Okx,
            )
        })
        .collect()
}

fn parse_open_lists(items: &[Value]) -> Vec<OpenOrder> {
    items
        .iter()
        .filter_map(|o| {
            let symbol = text(o, "symbol")?;
            crate::signing::validate_symbol(&symbol).ok()?;
            Some(OpenOrder::bare(
                symbol,
                text(o, "orderListId")?,
                OpenKind::List,
            ))
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

#[cfg(test)]
mod open_order_tests {
    use super::*;
    use crate::trade::protect::ProtectRole;
    use serde_json::json;

    fn d(s: &str) -> Decimal {
        Decimal::parse(s).expect("decimal")
    }

    #[test]
    fn binance_algo_rows_carry_the_protection_fields() {
        let rows = vec![
            json!({"algoId": 11, "symbol": "BTCUSDT", "side": "SELL", "positionSide": "BOTH",
                   "orderType": "STOP_MARKET", "triggerPrice": "60000", "closePosition": true,
                   "reduceOnly": false, "quantity": "0"}),
            json!({"algoId": 12, "symbol": "BTCUSDT", "side": "SELL", "positionSide": "LONG",
                   "orderType": "TAKE_PROFIT_MARKET", "triggerPrice": "70000",
                   "closePosition": true}),
            json!({"algoId": 13, "symbol": "BTCUSDT", "side": "BUY", "positionSide": "BOTH",
                   "orderType": "TRAILING_STOP_MARKET", "quantity": "0.5", "reduceOnly": true}),
            json!({"algoId": 14, "symbol": "BTCUSDT", "side": "BUY", "orderType": "LIMIT"}),
        ];
        let o = parse_open(
            &rows,
            "symbol",
            "algoId",
            OpenKind::Algo(None),
            Wire::Binance,
        );
        assert_eq!(o.len(), 4);
        assert_eq!(o[0].role, Some(ProtectRole::Stop));
        assert_eq!(o[0].side, Some(Side::Sell));
        assert_eq!(o[0].position_side, None);
        assert_eq!(o[0].trigger_price, Some(d("60000")));
        assert!(o[0].close_position && !o[0].reduce_only);
        assert_eq!(o[1].role, Some(ProtectRole::Target));
        assert_eq!(o[1].position_side, Some(Dir::Long));
        assert_eq!(o[2].role, Some(ProtectRole::Trailing));
        assert_eq!(o[2].qty, Some(d("0.5")));
        assert!(o[2].reduce_only && !o[2].close_position);
        // Not a conditional protective type: no role.
        assert_eq!(o[3].role, None);
    }

    #[test]
    fn client_ids_and_trailing_fields_are_read_per_venue() {
        let algo = vec![
            json!({"algoId": 1, "symbol": "BTCUSDT", "side": "SELL",
            "orderType": "STOP", "triggerPrice": "60000", "price": "59900",
            "clientAlgoId": "sAAAAAAAAAAAAAAAAAAAA", "closePosition": true}),
            json!({"algoId": 2, "symbol": "BTCUSDT", "side": "SELL",
            "orderType": "TRAILING_STOP_MARKET", "callbackRate": "1.5",
            "activatePrice": "61000", "quantity": "1", "clientAlgoId": ""}),
        ];
        let o = parse_open(
            &algo,
            "symbol",
            "algoId",
            OpenKind::Algo(None),
            Wire::Binance,
        );
        assert_eq!(o[0].client_id.as_deref(), Some("sAAAAAAAAAAAAAAAAAAAA"));
        assert!(o[0].limit);
        assert_eq!(o[0].limit_price, Some(d("59900")));
        assert_eq!(o[1].client_id, None, "an empty id is no id");
        assert_eq!(o[1].callback_rate, Some(d("1.5")));
        assert_eq!(o[1].activation_price, Some(d("61000")));
        let long = "s".repeat(65);
        let rows = vec![
            json!({"orderId": 4, "symbol": "BTCUSDT", "clientOrderId": long}),
            json!({"orderId": 5, "symbol": "BTCUSDT", "clientOrderId": "s".repeat(64)}),
        ];
        let o = parse_open(&rows, "symbol", "orderId", OpenKind::Plain, Wire::Binance);
        assert_eq!(o[0].client_id, None, "an id over 64 characters is no id");
        assert_eq!(o[1].client_id.as_deref().map(str::len), Some(64));
        let plain = vec![json!({"orderId": 3, "symbol": "BTCUSDT", "clientOrderId": "eBBBB"})];
        let o = parse_open(&plain, "symbol", "orderId", OpenKind::Plain, Wire::Binance);
        assert_eq!(o[0].client_id.as_deref(), Some("eBBBB"));
        let okx = vec![
            json!({"algoId": "9", "instId": "BTC-USDT-SWAP", "side": "sell",
            "ordType": "conditional", "slTriggerPx": "59000", "slOrdPx": "58900",
            "algoClOrdId": "sCCCC", "closeFraction": "1"}),
        ];
        let o = parse_open(&okx, "instId", "algoId", OpenKind::Algo(None), Wire::Okx);
        assert_eq!(o[0].client_id.as_deref(), Some("sCCCC"));
        assert!(o[0].limit);
        let market = vec![
            json!({"algoId": "9", "instId": "BTC-USDT-SWAP", "side": "sell",
            "ordType": "conditional", "slTriggerPx": "59000", "slOrdPx": "-1"}),
        ];
        let o = parse_open(&market, "instId", "algoId", OpenKind::Algo(None), Wire::Okx);
        assert!(!o[0].limit);
    }

    #[test]
    fn a_position_side_that_cannot_be_read_gives_no_leg() {
        for bad in [json!("FOO"), json!(7), json!(null), json!("BOTH")] {
            let row = json!({"algoId": 1, "symbol": "BTCUSDT", "side": "SELL",
                "orderType": "STOP_MARKET", "triggerPrice": "1", "closePosition": true,
                "positionSide": bad});
            let o = parse_open(
                &[row],
                "symbol",
                "algoId",
                OpenKind::Algo(None),
                Wire::Binance,
            );
            assert_eq!(o[0].position_side, None);
        }
    }

    #[test]
    fn binance_plain_rows_keep_working_without_the_new_fields() {
        let rows = vec![json!({"symbol": "BTCUSDT", "orderId": 1})];
        let o = parse_open(&rows, "symbol", "orderId", OpenKind::Plain, Wire::Binance);
        assert_eq!(
            o,
            vec![OpenOrder::bare(
                "BTCUSDT".into(),
                "1".into(),
                OpenKind::Plain
            )]
        );
        let plain_stop = vec![json!({"symbol": "BTCUSDT", "orderId": 2, "side": "SELL",
            "type": "STOP_MARKET", "stopPrice": "1.5", "origQty": "3", "reduceOnly": "true"})];
        let o = parse_open(
            &plain_stop,
            "symbol",
            "orderId",
            OpenKind::Plain,
            Wire::Binance,
        );
        // A plain row is never judged protective; its other fields still parse.
        assert_eq!(o[0].role, None);
        assert_eq!(o[0].trigger_price, Some(d("1.5")));
        assert_eq!(o[0].qty, Some(d("3")));
        assert!(o[0].reduce_only);
    }

    #[test]
    fn a_role_without_a_readable_trigger_price_is_dropped() {
        let rows = vec![
            json!({"algoId": 1, "symbol": "BTCUSDT", "side": "SELL", "orderType": "STOP_MARKET",
                   "quantity": "1", "reduceOnly": "true"}),
            json!({"algoId": 2, "symbol": "BTCUSDT", "side": "SELL", "orderType": "STOP_MARKET",
                   "triggerPrice": "abc", "quantity": "1", "reduceOnly": "true"}),
            json!({"algoId": 3, "symbol": "BTCUSDT", "side": "SELL", "orderType": "STOP_MARKET",
                   "triggerPrice": "90", "quantity": "1", "reduceOnly": "true"}),
        ];
        let o = parse_open(
            &rows,
            "symbol",
            "algoId",
            OpenKind::Algo(None),
            Wire::Binance,
        );
        assert_eq!(o[0].role, None);
        assert_eq!(o[1].role, None);
        assert_eq!(o[2].role, Some(ProtectRole::Stop));
    }

    #[test]
    fn okx_close_fraction_is_a_full_close_only_when_it_equals_one() {
        for (raw, full) in [
            ("0", false),
            ("0.5", false),
            ("", false),
            ("abc", false),
            ("1", true),
            ("1.0", true),
        ] {
            let rows = vec![json!({"algoId": "1", "instId": "BTC-USDT-SWAP",
                "ordType": "conditional", "side": "sell", "posSide": "net", "sz": "1",
                "slTriggerPx": "60000", "closeFraction": raw})];
            let o = parse_open(&rows, "instId", "algoId", OpenKind::Algo(None), Wire::Okx);
            assert_eq!(o[0].close_position, full, "closeFraction {raw:?}");
        }
    }

    #[test]
    fn okx_rows_carry_the_protection_fields() {
        use crate::trade::okx::OkxAlgoType;
        let rows = vec![
            json!({"algoId": "1", "instId": "BTC-USDT-SWAP", "ordType": "oco", "side": "sell",
                   "posSide": "net", "sz": "2", "slTriggerPx": "60000", "tpTriggerPx": "70000",
                   "reduceOnly": "true"}),
            json!({"algoId": "2", "instId": "BTC-USDT-SWAP", "ordType": "conditional",
                   "side": "sell", "posSide": "long", "closeFraction": "1",
                   "slTriggerPx": "", "tpTriggerPx": "71000"}),
            json!({"algoId": "3", "instId": "BTC-USDT-SWAP", "ordType": "conditional",
                   "side": "buy", "posSide": "short", "slTriggerPx": "80000", "sz": "1"}),
            json!({"algoId": "4", "instId": "BTC-USDT-SWAP", "ordType": "move_order_stop",
                   "side": "sell", "posSide": "net", "sz": "1", "reduceOnly": "true"}),
        ];
        let group = [
            OkxAlgoType::Conditional,
            OkxAlgoType::Oco,
            OkxAlgoType::MoveOrderStop,
        ];
        let o = parse_open_algos(&rows, &group);
        assert_eq!(o.len(), 4);
        assert_eq!(o[0].role, Some(ProtectRole::StopAndTarget));
        assert_eq!(o[0].trigger_price, Some(d("60000")));
        assert_eq!(o[0].position_side, None);
        assert!(o[0].reduce_only && !o[0].close_position);
        assert_eq!(o[1].role, Some(ProtectRole::Target));
        assert_eq!(o[1].position_side, Some(Dir::Long));
        assert!(o[1].close_position);
        assert_eq!(o[2].role, Some(ProtectRole::Stop));
        assert_eq!(o[2].position_side, Some(Dir::Short));
        assert_eq!(o[2].qty, Some(d("1")));
        assert_eq!(o[3].role, Some(ProtectRole::Trailing));
    }

    #[test]
    fn okx_plain_rows_are_never_protective() {
        let rows = vec![
            json!({"instId": "BTC-USDT-SWAP", "ordId": "9", "side": "buy",
            "posSide": "long", "sz": "1", "ordType": "limit", "reduceOnly": "false"}),
        ];
        let o = parse_open(&rows, "instId", "ordId", OpenKind::Plain, Wire::Okx);
        assert_eq!(o[0].role, None);
        assert_eq!(o[0].side, Some(Side::Buy));
        assert_eq!(o[0].position_side, Some(Dir::Long));
    }
}

#[cfg(test)]
mod attached_tests {
    use super::*;
    use serde_json::json;

    fn rows(v: Value) -> Vec<OpenOrder> {
        parse_open(
            v.as_array().unwrap(),
            "instId",
            "ordId",
            OpenKind::Plain,
            Wire::Okx,
        )
    }

    #[test]
    fn an_okx_entry_with_attached_orders_says_which_kind_without_being_in_force() {
        use super::super::protect::ProtectRole;
        let out = rows(json!([
            {"instId": "BTC-USDT-SWAP", "ordId": "1", "side": "buy", "sz": "3",
             "attachAlgoOrds": [{"slTriggerPx": "49000", "tpTriggerPx": "52000"}]},
            {"instId": "BTC-USDT-SWAP", "ordId": "2", "side": "buy", "sz": "3",
             "attachAlgoOrds": [{"slTriggerPx": "49000", "tpTriggerPx": ""}]},
            {"instId": "BTC-USDT-SWAP", "ordId": "3", "side": "buy", "sz": "3",
             "attachAlgoOrds": []},
            {"instId": "BTC-USDT-SWAP", "ordId": "4", "side": "buy", "sz": "3"},
            {"instId": "BTC-USDT-SWAP", "ordId": "5", "side": "buy", "sz": "3",
             "attachAlgoOrds": [{"slTriggerPx": "", "tpTriggerPx": ""}]}
        ]));
        let kinds: Vec<_> = out.iter().map(|o| o.attached).collect();
        assert_eq!(
            kinds,
            vec![
                Some(ProtectRole::StopAndTarget),
                Some(ProtectRole::Stop),
                None,
                None,
                None
            ]
        );
        // Attached orders are not in force: the row itself has no protective role.
        assert!(out.iter().all(|o| o.role.is_none()));
    }
}
