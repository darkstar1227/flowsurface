//! Exchange account credentials, request signing and the connection test for
//! Binance and OKX.
//!
//! Everything here is independent of the GUI. Secrets are wrapped in
//! [`secrecy::SecretString`] and only exposed inside the signing functions and
//! when a request header is built.
//!
//! Signed requests deliberately bypass `adapter::http::HttpHub`: that path logs the full
//! URL (including the Binance `signature` query parameter) and a body preview.
//! [`send`] only ever reports `method + host + path`, never the query or body.

use crate::rate_limit::{self, LimitKey, OkxEndpoint, Priority};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use ring::hmac;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::time::Instant;

/// Largest response body accepted from a signed endpoint.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Binance `recvWindow` for signed requests (the docs recommend 5000 or less).
const BINANCE_RECV_WINDOW_MS: u64 = 5000;
/// Longest exchange `msg` kept in an error.
const MAX_MSG_CHARS: usize = 120;

// ── Basic types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum AccountVenue {
    #[default]
    Binance,
    Okx,
}

impl AccountVenue {
    pub const ALL: [AccountVenue; 2] = [AccountVenue::Binance, AccountVenue::Okx];

    /// Lowercase id used in Keychain account names.
    pub fn as_str(self) -> &'static str {
        match self {
            AccountVenue::Binance => "binance",
            AccountVenue::Okx => "okx",
        }
    }
}

impl fmt::Display for AccountVenue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountVenue::Binance => f.write_str("Binance"),
            AccountVenue::Okx => f.write_str("OKX"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum AccountEnv {
    #[default]
    Live,
    /// Binance testnet, or OKX demo trading (`x-simulated-trading: 1`).
    Testnet,
}

impl AccountEnv {
    pub const ALL: [AccountEnv; 2] = [AccountEnv::Live, AccountEnv::Testnet];

    pub fn as_str(self) -> &'static str {
        match self {
            AccountEnv::Live => "live",
            AccountEnv::Testnet => "testnet",
        }
    }
}

impl fmt::Display for AccountEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountEnv::Live => f.write_str("Live"),
            AccountEnv::Testnet => f.write_str("Testnet"),
        }
    }
}

/// Binance product an account is bound to. Testnet keys of the two products are issued by
/// different hosts, so a testnet key must never be sent to the other product's host.
/// Live keys are shared by both products. OKX uses one key for everything and ignores this
/// (its accounts keep the default).
///
/// `UsdM` is the default because M4 supported futures only: existing accounts without the
/// field load as `UsdM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum AccountProduct {
    Spot,
    #[default]
    UsdM,
}

impl AccountProduct {
    pub const ALL: [AccountProduct; 2] = [AccountProduct::Spot, AccountProduct::UsdM];
}

impl fmt::Display for AccountProduct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountProduct::Spot => f.write_str("Spot"),
            AccountProduct::UsdM => f.write_str("USDⓈ-M"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum PositionMode {
    #[default]
    OneWay,
    Hedge,
}

impl PositionMode {
    pub const ALL: [PositionMode; 2] = [PositionMode::OneWay, PositionMode::Hedge];
}

impl fmt::Display for PositionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PositionMode::OneWay => f.write_str("One-way"),
            PositionMode::Hedge => f.write_str("Hedge (long/short)"),
        }
    }
}

/// Opaque account identity for per-account rate accounting. The caller derives it from its own
/// account id; this crate never interprets the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AccountKey([u8; 16]);

impl AccountKey {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

/// API key, secret and (OKX only) passphrase. Debug output is masked.
pub struct ApiCredentials {
    key: SecretString,
    secret: SecretString,
    passphrase: Option<SecretString>,
}

impl ApiCredentials {
    pub fn new(key: String, secret: String, passphrase: Option<String>) -> Self {
        Self {
            key: SecretString::from(key),
            secret: SecretString::from(secret),
            passphrase: passphrase.map(SecretString::from),
        }
    }

    pub fn key(&self) -> &SecretString {
        &self.key
    }

    pub fn secret(&self) -> &SecretString {
        &self.secret
    }

    pub fn passphrase(&self) -> Option<&SecretString> {
        self.passphrase.as_ref()
    }

    /// Field-by-field comparison, used to confirm a Keychain write by reading it back.
    pub fn same_as(&self, other: &Self) -> bool {
        self.key.expose_secret() == other.key.expose_secret()
            && self.secret.expose_secret() == other.secret.expose_secret()
            && self.passphrase.as_ref().map(ExposeSecret::expose_secret)
                == other.passphrase.as_ref().map(ExposeSecret::expose_secret)
    }
}

impl fmt::Debug for ApiCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiCredentials(***)")
    }
}

// ── Verdicts ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Permissions were read and there is no withdrawal permission.
    Usable,
    /// The key must not be used. Carries the reason.
    Rejected(String),
    /// Permissions could not be read (only acceptable on testnet).
    Unverified(String),
}

/// Result of a connection test. Only the verdict is kept, never response bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    pub verdict: Verdict,
    pub warnings: Vec<String>,
    /// Position mode reported by the exchange, if it could be read.
    pub actual_mode: Option<PositionMode>,
    /// Whether the key has the trading permission of the account's product
    /// (`None` = could not be read, e.g. on testnet). Live Binance orders need `Some(true)`.
    pub can_trade: Option<bool>,
}

/// `Live` accounts need `Usable`; `Testnet` accounts may also be `Unverified`.
/// `Rejected` is never usable.
pub fn is_usable(env: AccountEnv, verdict: &Verdict) -> bool {
    matches!(
        (env, verdict),
        (_, Verdict::Usable) | (AccountEnv::Testnet, Verdict::Unverified(_))
    )
}

fn unconfirmed(why: &str) -> Verdict {
    Verdict::Rejected(format!("cannot confirm withdrawal permission ({why})"))
}

/// Warning shown when the exchange's position mode differs from the expected one.
/// Compares against the value reported by the exchange, never a stored value.
pub fn position_mode_warning(desired: PositionMode, actual: PositionMode) -> Option<String> {
    (desired != actual).then(|| {
        format!(
            "Position mode mismatch: exchange is {actual}, account expects {desired}. \
             Change it on the exchange or recreate the account."
        )
    })
}

const NO_IP_WARNING: &str = "API key has no IP restriction. Bind it to your IP; \
     an unrestricted key must not be used for trading.";

// ── Signing ─────────────────────────────────────────────────────────────

fn hmac_sha256(secret: &str, message: &str) -> hmac::Tag {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    hmac::sign(&key, message.as_bytes())
}

/// Binance: hex(HMAC-SHA256(secret, query)).
pub(crate) fn binance_sign(secret: &str, query: &str) -> String {
    let tag = hmac_sha256(secret, query);
    let mut out = String::with_capacity(64);
    for b in tag.as_ref() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// OKX: base64(HMAC-SHA256(secret, timestamp + method + requestPath + body)).
/// `request_path` includes the query string.
pub(crate) fn okx_sign(
    secret: &str,
    timestamp: &str,
    method: &str,
    request_path: &str,
    body: &str,
) -> String {
    let prehash = format!("{timestamp}{method}{request_path}{body}");
    BASE64.encode(hmac_sha256(secret, &prehash).as_ref())
}

pub(crate) fn okx_timestamp(now: chrono::DateTime<chrono::Utc>) -> String {
    now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

pub(crate) fn sensitive(value: &str) -> Result<HeaderValue, AccountError> {
    let mut v = HeaderValue::from_str(value).map_err(|_| AccountError::InvalidCredentials)?;
    v.set_sensitive(true);
    Ok(v)
}

/// OKX request headers. `x-simulated-trading: 1` is sent for testnet (demo) and never for live.
pub(crate) fn okx_headers(
    env: AccountEnv,
    key: &str,
    sign: &str,
    timestamp: &str,
    passphrase: &str,
) -> Result<HeaderMap, AccountError> {
    let mut h = HeaderMap::new();
    h.insert("OK-ACCESS-KEY", sensitive(key)?);
    h.insert("OK-ACCESS-SIGN", sensitive(sign)?);
    h.insert("OK-ACCESS-TIMESTAMP", sensitive(timestamp)?);
    h.insert("OK-ACCESS-PASSPHRASE", sensitive(passphrase)?);
    if env == AccountEnv::Testnet {
        h.insert(
            HeaderName::from_static("x-simulated-trading"),
            HeaderValue::from_static("1"),
        );
    }
    Ok(h)
}

// ── Endpoints ───────────────────────────────────────────────────────────

/// REST hosts a signed request for `(venue, env, product)` may be sent to. Hard-coded on
/// purpose: never read from settings or user input.
///
/// - Binance Live keys are shared by both products (`api.binance.com` for spot and `/sapi`,
///   `fapi.binance.com` for USDⓈ-M).
/// - Binance testnet keys are per product: the futures testnet and the spot testnet are
///   separate hosts and a key from one must never reach the other.
/// - OKX is global only (`www.okx.com`, `openapi.okx.com`); the demo environment uses the
///   same hosts with `x-simulated-trading: 1`. `us.okx.com`, `eea.okx.com` are not allowed.
fn https_hosts(
    venue: AccountVenue,
    env: AccountEnv,
    product: AccountProduct,
) -> &'static [&'static str] {
    match (venue, env, product) {
        (AccountVenue::Binance, AccountEnv::Live, _) => &["api.binance.com", "fapi.binance.com"],
        (AccountVenue::Binance, AccountEnv::Testnet, AccountProduct::UsdM) => {
            &["testnet.binancefuture.com"]
        }
        (AccountVenue::Binance, AccountEnv::Testnet, AccountProduct::Spot) => {
            &["testnet.binance.vision"]
        }
        (AccountVenue::Okx, _, _) => &["www.okx.com", "openapi.okx.com"],
    }
}

/// WebSocket hosts (always `wss` on port 443).
///
/// Partly verified only: the Binance futures private stream path is derived from the SDK
/// (`fstream.binance.com`), and the futures testnet stream host (`fstream.binancefuture.com`)
/// and its private path are not verified yet; the private stream (segment b) must confirm them
/// on the testnet before use.
fn wss_hosts(
    venue: AccountVenue,
    env: AccountEnv,
    product: AccountProduct,
) -> &'static [&'static str] {
    match (venue, env, product) {
        (AccountVenue::Binance, AccountEnv::Live, _) => {
            &["fstream.binance.com", "ws-api.binance.com"]
        }
        (AccountVenue::Binance, AccountEnv::Testnet, AccountProduct::UsdM) => {
            &["fstream.binancefuture.com"]
        }
        (AccountVenue::Binance, AccountEnv::Testnet, AccountProduct::Spot) => {
            &["ws-api.testnet.binance.vision"]
        }
        (AccountVenue::Okx, AccountEnv::Live, _) => &["ws.okx.com"],
        (AccountVenue::Okx, AccountEnv::Testnet, _) => &["wspap.okx.com"],
    }
}

/// Build an `https://` URL after checking the host against the allow-list.
pub fn https_endpoint(
    venue: AccountVenue,
    env: AccountEnv,
    product: AccountProduct,
    host: &str,
    path_and_query: &str,
) -> Result<reqwest::Url, AccountError> {
    if !https_hosts(venue, env, product).contains(&host) {
        return Err(AccountError::InvalidEndpoint);
    }
    let url = reqwest::Url::parse(&format!("https://{host}{path_and_query}"))
        .map_err(|_| AccountError::InvalidEndpoint)?;
    if url.scheme() != "https" || url.host_str() != Some(host) || url.port().is_some() {
        return Err(AccountError::InvalidEndpoint);
    }
    Ok(url)
}

/// Validate a WebSocket URL: only `wss`, only port 443, only an allow-listed host.
/// `ws://` and any other port (such as 8443) are refused.
pub fn wss_endpoint(
    venue: AccountVenue,
    env: AccountEnv,
    product: AccountProduct,
    url: &str,
) -> Result<reqwest::Url, AccountError> {
    let url = reqwest::Url::parse(url).map_err(|_| AccountError::InvalidEndpoint)?;
    let host_ok = url
        .host_str()
        .is_some_and(|h| wss_hosts(venue, env, product).contains(&h));
    // `Url::port` is `None` for the scheme default, so an explicit `:443` and no port both
    // resolve to 443 here; anything else is refused.
    if url.scheme() != "wss"
        || !host_ok
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(AccountError::InvalidEndpoint);
    }
    Ok(url)
}

// ── Proxy snapshot (F10) ────────────────────────────────────────────────

/// What the proxy configuration was when a signed client (or private stream) was built.
///
/// `Broken` means the user's proxy setting could not be read (for example the saved state was
/// unreadable): it must never be treated like "no proxy", which would connect directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyState {
    Direct,
    Proxied(crate::proxy::Proxy),
    Broken,
}

impl ProxyState {
    pub fn from_config(proxy: Option<&crate::proxy::Proxy>) -> Self {
        proxy.map_or(ProxyState::Direct, |p| ProxyState::Proxied(p.clone()))
    }
}

/// Check the snapshot taken when the connection was built against the current setting.
/// Run before every signed request and before every private stream (re)connect.
pub fn check_proxy_snapshot(
    snapshot: &ProxyState,
    current: &ProxyState,
) -> Result<(), AccountError> {
    if *snapshot == ProxyState::Broken || *current == ProxyState::Broken {
        return Err(AccountError::ProxyBroken);
    }
    if snapshot != current {
        return Err(AccountError::ProxyChanged);
    }
    Ok(())
}

// ── Errors ──────────────────────────────────────────────────────────────

/// Error from a signed request. Display never contains a URL, query, header or body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountError {
    InvalidEndpoint,
    InvalidCredentials,
    /// The proxy setting changed since the client was built; restart to apply it.
    ProxyChanged,
    /// The proxy setting could not be read; signed requests are refused.
    ProxyBroken,
    /// Held back by the local rate limiter; nothing was sent.
    LocalRateLimited,
    Timeout,
    Connect,
    Network,
    Redirected,
    RateLimited(u16),
    ClockSkew,
    Http(u16),
    BadResponse,
    Exchange {
        code: String,
        msg: String,
    },
}

/// What is known about a request after it failed. Segment a3 maps this onto
/// `Accepted` / `Rejected` / `Unknown`; an `Unknown` outcome must never be retried blindly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The request never left this process (validation, local limiter, no connection).
    NotSent,
    /// The exchange explicitly refused the request, so it was not executed.
    Rejected,
    /// The request may or may not have been executed.
    Unknown,
}

/// Error codes after which the execution status is unknown even though the exchange answered
/// (Binance `-1006` UNEXPECTED_RESP, `-1007` TIMEOUT; OKX `50004`, `51149`).
const UNKNOWN_OUTCOME_CODES: [&str; 4] = ["-1006", "-1007", "50004", "51149"];

impl AccountError {
    pub fn delivery(&self) -> Delivery {
        match self {
            AccountError::InvalidEndpoint
            | AccountError::InvalidCredentials
            | AccountError::ProxyChanged
            | AccountError::ProxyBroken
            | AccountError::LocalRateLimited
            | AccountError::Connect => Delivery::NotSent,
            AccountError::RateLimited(_) | AccountError::ClockSkew => Delivery::Rejected,
            AccountError::Exchange { code, .. } => {
                if UNKNOWN_OUTCOME_CODES.contains(&code.as_str()) {
                    Delivery::Unknown
                } else {
                    Delivery::Rejected
                }
            }
            // A 4xx without an exchange `code` may come from a proxy or CDN (408, an HTML error
            // page), so it proves nothing. Only 403 counts as a refusal: the spot REST docs
            // say 403 is returned when a WAF rule was violated (a rate-limit violation or a
            // security block), i.e. the gateway refused the request itself. The docs copy has
            // no USDⓈ-M page, so the same meaning for futures is not verified; every other
            // status is inconclusive.
            AccountError::Http(403) => Delivery::Rejected,
            AccountError::Http(_)
            | AccountError::Timeout
            | AccountError::Network
            | AccountError::Redirected
            | AccountError::BadResponse => Delivery::Unknown,
        }
    }
}

impl fmt::Display for AccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountError::InvalidEndpoint => {
                f.write_str("endpoint rejected (not an allowed https host)")
            }
            AccountError::InvalidCredentials => {
                f.write_str("credentials contain invalid characters")
            }
            AccountError::ProxyChanged => f.write_str(
                "proxy settings changed since startup; restart Flowsurface before using accounts",
            ),
            AccountError::ProxyBroken => f.write_str(
                "the proxy setting could not be read; refusing to connect (fix it and restart)",
            ),
            AccountError::LocalRateLimited => {
                f.write_str("held back by the local rate limiter; try again shortly")
            }
            AccountError::Timeout => f.write_str("request timed out"),
            AccountError::Connect => f.write_str("could not connect to the exchange"),
            AccountError::Network => f.write_str("network error"),
            AccountError::Redirected => f.write_str("unexpected redirect (not followed)"),
            AccountError::RateLimited(s) => {
                write!(
                    f,
                    "rate limited by the exchange (HTTP {s}); try again later"
                )
            }
            AccountError::ClockSkew => f.write_str(
                "local clock differs too much from the exchange; sync your system clock",
            ),
            AccountError::Http(s) => write!(f, "unexpected HTTP status {s}"),
            AccountError::BadResponse => f.write_str("unreadable response"),
            AccountError::Exchange { code, msg } => write!(f, "exchange error {code}: {msg}"),
        }
    }
}

fn classify_reqwest(e: &reqwest::Error) -> AccountError {
    // Only categories: reqwest's Display and Debug both embed the URL.
    if e.is_timeout() {
        AccountError::Timeout
    } else if e.is_connect() {
        AccountError::Connect
    } else {
        AccountError::Network
    }
}

/// Like [`classify_reqwest`] for errors after the request went out (reading the body): the
/// request was already delivered, so a connection error here is a plain network error.
fn classify_reqwest_after_send(e: &reqwest::Error) -> AccountError {
    if e.is_timeout() {
        AccountError::Timeout
    } else {
        AccountError::Network
    }
}

fn exchange_error(status: u16, body: &Value) -> AccountError {
    if status == 429 || status == 418 {
        return AccountError::RateLimited(status);
    }
    if (300..400).contains(&status) {
        return AccountError::Redirected;
    }
    let code = match body.get("code") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    let Some(code) = code else {
        return AccountError::Http(status);
    };
    // A 5xx is inconclusive unless the code itself says so; keep the status (-> `Unknown`).
    if status >= 500 && !UNKNOWN_OUTCOME_CODES.contains(&code.as_str()) {
        return AccountError::Http(status);
    }
    // Binance -1021 (timestamp outside recvWindow); OKX 50102 / 50112 (timestamp expired / invalid).
    if matches!(code.as_str(), "-1021" | "50102" | "50112") {
        return AccountError::ClockSkew;
    }
    let msg = body
        .get("msg")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(MAX_MSG_CHARS)
        .collect();
    AccountError::Exchange { code, msg }
}

// ── Transport ───────────────────────────────────────────────────────────

/// Response facts the rate limiter needs. Never the body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseMeta {
    pub status: u16,
    /// Binance `X-MBX-USED-WEIGHT-1M`.
    pub used_weight_1m: Option<usize>,
    /// Binance `X-MBX-ORDER-COUNT-10S` / `-1M`.
    pub order_count_10s: Option<usize>,
    pub order_count_1m: Option<usize>,
    /// `Retry-After` in seconds.
    pub retry_after_secs: Option<u64>,
}

fn header_usize(h: &reqwest::header::HeaderMap, name: &str) -> Option<usize> {
    h.get(name)?.to_str().ok()?.trim().parse().ok()
}

impl ResponseMeta {
    fn from_response(resp: &reqwest::Response) -> Self {
        let h = resp.headers();
        Self {
            status: resp.status().as_u16(),
            used_weight_1m: header_usize(h, "x-mbx-used-weight-1m"),
            order_count_10s: header_usize(h, "x-mbx-order-count-10s"),
            order_count_1m: header_usize(h, "x-mbx-order-count-1m"),
            retry_after_secs: header_usize(h, "retry-after").map(|n| n as u64),
        }
    }
}

/// Send one request and return the parsed JSON body of a 2xx response.
///
/// Logs and errors carry only `METHOD host path` and the status: never the query, the
/// body, or (on success) the response content, which holds order ids, sizes and prices.
/// The client must not follow redirects (see `connector::client::signed_client`), so
/// credential headers are never forwarded to another host.
///
/// `body` is sent exactly as given: the caller signs the same string it passes here.
#[cfg(test)]
pub(crate) async fn send(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: reqwest::Url,
    headers: HeaderMap,
    body: Option<String>,
) -> Result<Value, AccountError> {
    send_with_meta(client, method, url, headers, body).await.0
}

/// [`send`] plus the response facts, which are also available when the request failed
/// after the response arrived.
pub(crate) async fn send_with_meta(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: reqwest::Url,
    headers: HeaderMap,
    body: Option<String>,
) -> (Result<Value, AccountError>, Option<ResponseMeta>) {
    let host = url.host_str().unwrap_or("?").to_string();
    let path = url.path().to_string();
    let method_name = method.as_str().to_string();

    let mut request = client.request(method, url).headers(headers);
    if let Some(body) = body {
        request = request.body(body);
    }
    let mut resp = match request.send().await {
        Ok(r) => r,
        Err(e) => return (Err(classify_reqwest(&e)), None),
    };
    let meta = ResponseMeta::from_response(&resp);
    let status = meta.status;
    log::debug!("signed request {method_name} {host}{path} -> {status}");

    if resp
        .content_length()
        .is_some_and(|n| n > MAX_BODY_BYTES as u64)
    {
        return (Err(AccountError::BadResponse), Some(meta));
    }
    // Chunked responses carry no Content-Length, so the size is enforced while reading.
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if bytes.len() + chunk.len() > MAX_BODY_BYTES {
                    return (Err(AccountError::BadResponse), Some(meta));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => return (Err(classify_reqwest_after_send(&e)), Some(meta)),
        }
    }
    let body: Result<Value, _> = serde_json::from_slice(&bytes);

    if !(200..300).contains(&status) {
        // Only the exchange's code and msg are kept, not the body.
        let err = match body {
            Ok(v) => exchange_error(status, &v),
            Err(_) => exchange_error(status, &Value::Null),
        };
        // The exchange's msg is shown in the UI only: it may echo request details (such as
        // the caller's IP), so the log keeps the code alone.
        let summary = match &err {
            AccountError::Exchange { code, .. } => format!("exchange error {code}"),
            other => other.to_string(),
        };
        log::warn!("signed request {method_name} {host}{path} failed: {summary}");
        return (Err(err), Some(meta));
    }
    match body {
        Ok(v) => (Ok(v), Some(meta)),
        Err(_) => {
            log::warn!("signed request {method_name} {host}{path}: unreadable response");
            (Err(AccountError::BadResponse), Some(meta))
        }
    }
}

/// [`send`] behind the local rate limiter: nothing goes out when the budget (or the Binance
/// IP-ban pause) says no, and the response's weight, order counts, 429 and 418 are fed back.
/// Never retries.
pub(crate) async fn send_limited(
    client: &reqwest::Client,
    key: &LimitKey,
    priority: Priority,
    method: reqwest::Method,
    url: reqwest::Url,
    headers: HeaderMap,
    body: Option<String>,
) -> Result<Value, AccountError> {
    send_limited_meta(client, key, priority, method, url, headers, body)
        .await
        .0
}

/// [`send_limited`] plus the HTTP status (for the `Unknown` outcome's display).
pub(crate) async fn send_limited_meta(
    client: &reqwest::Client,
    key: &LimitKey,
    priority: Priority,
    method: reqwest::Method,
    url: reqwest::Url,
    headers: HeaderMap,
    body: Option<String>,
) -> (Result<Value, AccountError>, Option<u16>) {
    if rate_limit::acquire(key, priority, Instant::now()).is_err() {
        return (Err(AccountError::LocalRateLimited), None);
    }
    let (result, meta) = send_with_meta(client, method, url, headers, body).await;
    let status = meta.as_ref().map(|m| m.status);
    if let Some(meta) = meta {
        rate_limit::record(key, &meta, Instant::now());
    }
    (result, status)
}

// ── Binance ─────────────────────────────────────────────────────────────

/// `(host for time + /sapi, host for the product's signed calls, time path)`.
pub(crate) fn binance_hosts(
    env: AccountEnv,
    product: AccountProduct,
) -> (&'static str, &'static str, &'static str) {
    match (env, product) {
        (AccountEnv::Live, AccountProduct::Spot) => {
            ("api.binance.com", "api.binance.com", "/api/v3/time")
        }
        (AccountEnv::Live, AccountProduct::UsdM) => {
            ("api.binance.com", "fapi.binance.com", "/api/v3/time")
        }
        (AccountEnv::Testnet, AccountProduct::UsdM) => (
            "testnet.binancefuture.com",
            "testnet.binancefuture.com",
            "/fapi/v1/time",
        ),
        (AccountEnv::Testnet, AccountProduct::Spot) => (
            "testnet.binance.vision",
            "testnet.binance.vision",
            "/api/v3/time",
        ),
    }
}

/// Parse Binance `GET /sapi/v1/account/apiRestrictions`.
///
/// Fail closed: a missing or non-boolean `enableWithdrawals` is `Rejected`.
pub fn assess_binance_restrictions(v: &Value, product: AccountProduct) -> (Verdict, Vec<String>) {
    let flag = |name: &str| v.get(name).and_then(Value::as_bool);
    let mut warnings = Vec::new();

    let verdict = match flag("enableWithdrawals") {
        Some(true) => Verdict::Rejected("API key has withdrawal permission".into()),
        Some(false) => Verdict::Usable,
        None => unconfirmed("enableWithdrawals missing in response"),
    };

    match flag("ipRestrict") {
        Some(true) => {}
        Some(false) => warnings.push(NO_IP_WARNING.to_string()),
        None => warnings.push("Could not confirm whether the key is IP-restricted.".to_string()),
    }
    if flag("enableInternalTransfer") == Some(true)
        || flag("permitsUniversalTransfer") == Some(true)
    {
        warnings.push(
            "API key can transfer funds between accounts (internal / universal transfer). \
             Disable it if not needed."
                .to_string(),
        );
    }
    match product {
        AccountProduct::UsdM if flag("enableFutures") != Some(true) => {
            warnings.push("API key does not have Futures permission enabled.".to_string());
        }
        AccountProduct::Spot if flag("enableSpotAndMarginTrading") != Some(true) => {
            warnings.push("API key does not have Spot trading permission enabled.".to_string());
        }
        _ => {}
    }
    (verdict, warnings)
}

/// The trading permission of `product` from `apiRestrictions`: futures need `enableFutures`,
/// spot needs `enableSpotAndMarginTrading`. Missing or non-boolean is `None`, and a gate
/// treats `None` like a missing permission on Live.
pub fn binance_trade_permission(v: &Value, product: AccountProduct) -> Option<bool> {
    let name = match product {
        AccountProduct::UsdM => "enableFutures",
        AccountProduct::Spot => "enableSpotAndMarginTrading",
    };
    v.get(name).and_then(Value::as_bool)
}

/// Parse Binance `GET /fapi/v1/positionSide/dual` (`dualSidePosition`).
pub fn parse_binance_position_mode(v: &Value) -> Option<PositionMode> {
    match v.get("dualSidePosition")? {
        Value::Bool(true) => Some(PositionMode::Hedge),
        Value::Bool(false) => Some(PositionMode::OneWay),
        _ => None,
    }
}

async fn binance_signed_get(
    client: &reqwest::Client,
    env: AccountEnv,
    product: AccountProduct,
    creds: &ApiCredentials,
    host: &str,
    path: &str,
    weight: usize,
    offset_ms: i64,
) -> Result<Value, AccountError> {
    let timestamp = (chrono::Utc::now().timestamp_millis() + offset_ms).to_string();
    let recv_window = BINANCE_RECV_WINDOW_MS.to_string();
    let signed = crate::signing::binance_signed_form(
        creds.secret().expose_secret(),
        &[("recvWindow", &recv_window), ("timestamp", &timestamp)],
    )
    .map_err(|_| AccountError::InvalidEndpoint)?;
    let url = https_endpoint(
        AccountVenue::Binance,
        env,
        product,
        host,
        &format!("{path}?{}", signed.body),
    )?;
    let mut headers = HeaderMap::new();
    headers.insert("X-MBX-APIKEY", sensitive(creds.key().expose_secret())?);
    send_limited(
        client,
        &LimitKey::binance(host, weight),
        Priority::Query,
        reqwest::Method::GET,
        url,
        headers,
        None,
    )
    .await
}

/// Server time minus local time. A failure falls back to no correction.
async fn binance_clock_offset(
    client: &reqwest::Client,
    env: AccountEnv,
    product: AccountProduct,
) -> i64 {
    let (host, _, path) = binance_hosts(env, product);
    let Ok(url) = https_endpoint(AccountVenue::Binance, env, product, host, path) else {
        return 0;
    };
    let local = chrono::Utc::now().timestamp_millis();
    let key = LimitKey::binance(host, 1);
    match send_limited(
        client,
        &key,
        Priority::Query,
        reqwest::Method::GET,
        url,
        HeaderMap::new(),
        None,
    )
    .await
    {
        Ok(v) => v
            .get("serverTime")
            .and_then(Value::as_i64)
            .map_or(0, |server| server - local),
        Err(_) => 0,
    }
}

async fn check_binance(
    client: &reqwest::Client,
    env: AccountEnv,
    product: AccountProduct,
    creds: &ApiCredentials,
    desired: PositionMode,
) -> CheckReport {
    let (sapi_host, product_host, _) = binance_hosts(env, product);
    let offset = binance_clock_offset(client, env, product).await;

    let mut can_trade = None;
    let (mut verdict, mut warnings) = match env {
        AccountEnv::Live => match binance_signed_get(
            client,
            env,
            product,
            creds,
            sapi_host,
            "/sapi/v1/account/apiRestrictions",
            1,
            offset,
        )
        .await
        {
            Ok(v) => {
                can_trade = binance_trade_permission(&v, product);
                assess_binance_restrictions(&v, product)
            }
            Err(e) => (unconfirmed(&e.to_string()), Vec::new()),
        },
        // Neither testnet has /sapi, so permissions cannot be read.
        AccountEnv::Testnet => (
            Verdict::Unverified("cannot confirm permissions (testnet has no /sapi)".into()),
            Vec::new(),
        ),
    };

    let mut actual_mode = None;
    if !matches!(verdict, Verdict::Rejected(_)) {
        // Futures: the position mode. Spot has none, so the account endpoint serves as the
        // one signed call that proves the key works.
        let (path, weight) = match product {
            AccountProduct::UsdM => ("/fapi/v1/positionSide/dual", 30),
            AccountProduct::Spot => ("/api/v3/account", 20),
        };
        match binance_signed_get(
            client,
            env,
            product,
            creds,
            product_host,
            path,
            weight,
            offset,
        )
        .await
        {
            Ok(v) if product == AccountProduct::UsdM => match parse_binance_position_mode(&v) {
                Some(mode) => actual_mode = Some(mode),
                None => warnings.push("Could not read the futures position mode.".into()),
            },
            Ok(_) => {}
            Err(e) if env == AccountEnv::Testnet => {
                // On testnet this is the only signed call, so a failure means the key
                // could not be checked at all.
                verdict = Verdict::Rejected(format!("key could not be checked on testnet ({e})"));
            }
            Err(e) if product == AccountProduct::UsdM => {
                warnings.push(format!("Could not read the futures position mode ({e})."));
            }
            Err(e) => warnings.push(format!("Could not read the spot account ({e}).")),
        }
    }
    if let Some(actual) = actual_mode {
        warnings.extend(position_mode_warning(desired, actual));
    }
    CheckReport {
        verdict,
        warnings,
        actual_mode,
        can_trade,
    }
}

// ── OKX ─────────────────────────────────────────────────────────────────

/// Parse OKX `GET /api/v5/account/config`.
///
/// `perm` is a comma separated list; only an exact `withdraw` token rejects the key,
/// and a missing `perm` is also `Rejected` (fail closed).
pub fn assess_okx_config(v: &Value) -> CheckReport {
    let rejected = |why: &str| CheckReport {
        verdict: unconfirmed(why),
        warnings: Vec::new(),
        actual_mode: None,
        can_trade: None,
    };

    if v.get("code").and_then(Value::as_str) != Some("0") {
        return rejected("unexpected response code");
    }
    let Some(data) = v
        .get("data")
        .and_then(Value::as_array)
        .and_then(|d| d.first())
    else {
        return rejected("empty response data");
    };
    let Some(perm) = data.get("perm").and_then(Value::as_str) else {
        return rejected("perm missing in response");
    };

    let verdict = if perm.split(',').map(str::trim).any(|p| p == "withdraw") {
        Verdict::Rejected("API key has withdrawal permission".into())
    } else {
        Verdict::Usable
    };

    let mut warnings = Vec::new();
    match data.get("ip").and_then(Value::as_str) {
        Some("") => warnings.push(NO_IP_WARNING.to_string()),
        Some(_) => {}
        None => warnings.push("Could not confirm whether the key is IP-restricted.".into()),
    }

    let actual_mode = match data.get("posMode").and_then(Value::as_str) {
        Some("net_mode") => Some(PositionMode::OneWay),
        Some("long_short_mode") => Some(PositionMode::Hedge),
        other => {
            let shown: String = other.unwrap_or("missing").chars().take(32).collect();
            warnings.push(format!("Unknown position mode ({shown})."));
            None
        }
    };

    let can_trade = Some(perm.split(',').map(str::trim).any(|p| p == "trade"));

    CheckReport {
        verdict,
        warnings,
        actual_mode,
        can_trade,
    }
}

async fn check_okx(
    client: &reqwest::Client,
    env: AccountEnv,
    creds: &ApiCredentials,
    desired: PositionMode,
) -> CheckReport {
    let fail = |e: AccountError| CheckReport {
        verdict: unconfirmed(&e.to_string()),
        warnings: Vec::new(),
        actual_mode: None,
        can_trade: None,
    };
    let Some(passphrase) = creds.passphrase() else {
        return fail(AccountError::InvalidCredentials);
    };

    const PATH: &str = "/api/v5/account/config";
    let timestamp = okx_timestamp(chrono::Utc::now());
    let sign = okx_sign(creds.secret().expose_secret(), &timestamp, "GET", PATH, "");
    let headers = match okx_headers(
        env,
        creds.key().expose_secret(),
        &sign,
        &timestamp,
        passphrase.expose_secret(),
    ) {
        Ok(h) => h,
        Err(e) => return fail(e),
    };
    let url = match https_endpoint(
        AccountVenue::Okx,
        env,
        AccountProduct::default(),
        "www.okx.com",
        PATH,
    ) {
        Ok(u) => u,
        Err(e) => return fail(e),
    };

    match send_limited(
        client,
        &LimitKey::Okx(OkxEndpoint::AccountConfig),
        Priority::Query,
        reqwest::Method::GET,
        url,
        headers,
        None,
    )
    .await
    {
        Ok(v) => {
            let mut report = assess_okx_config(&v);
            if let Some(actual) = report.actual_mode {
                report
                    .warnings
                    .extend(position_mode_warning(desired, actual));
            }
            report
        }
        Err(e) => fail(e),
    }
}

// ── Entry point ─────────────────────────────────────────────────────────

/// Connection test: read the key's permissions and the position mode.
///
/// Never panics and never returns the credentials or any response body. Each call signs
/// fresh requests with a new timestamp; nothing is retried.
pub async fn check_account(
    client: &reqwest::Client,
    venue: AccountVenue,
    env: AccountEnv,
    product: AccountProduct,
    creds: &ApiCredentials,
    desired: PositionMode,
) -> CheckReport {
    match venue {
        AccountVenue::Binance => check_binance(client, env, product, creds, desired).await,
        AccountVenue::Okx => check_okx(client, env, creds, desired).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Public example values from Binance's official docs (request-security): not real credentials.
    const BINANCE_DOC_SECRET: &str =
        "NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j";
    const BINANCE_DOC_QUERY: &str = "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559";
    const BINANCE_DOC_SIGNATURE: &str =
        "c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71";

    const FAKE_KEY: &str = "fake-key-AAAA1111";
    const FAKE_SECRET: &str = "fake-secret-BBBB2222";
    const FAKE_PASS: &str = "fake-pass-CCCC3333";

    #[test]
    fn binance_signature_matches_official_example() {
        assert_eq!(
            binance_sign(BINANCE_DOC_SECRET, BINANCE_DOC_QUERY),
            BINANCE_DOC_SIGNATURE
        );
    }

    #[test]
    fn okx_signature_matches_fixed_vector() {
        // Computed independently with `openssl dgst -sha256 -hmac ... -binary | base64`.
        let sign = okx_sign(
            "test-secret-not-real",
            "2020-12-08T09:08:57.715Z",
            "GET",
            "/api/v5/account/config",
            "",
        );
        assert_eq!(sign, "4WtaWkuZ4PsJoJ4OaDOioQfvRdf9Dz05fZr4STd8mvA=");
    }

    #[test]
    fn okx_timestamp_is_utc_iso_millis() {
        let t = chrono::DateTime::parse_from_rfc3339("2020-12-08T09:08:57.715Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(okx_timestamp(t), "2020-12-08T09:08:57.715Z");
    }

    #[test]
    fn okx_simulated_header_only_on_testnet() {
        let live = okx_headers(AccountEnv::Live, "k", "s", "t", "p").unwrap();
        assert!(!live.contains_key("x-simulated-trading"));
        let demo = okx_headers(AccountEnv::Testnet, "k", "s", "t", "p").unwrap();
        assert_eq!(demo.get("x-simulated-trading").unwrap(), "1");
        for h in [&live, &demo] {
            for name in [
                "OK-ACCESS-KEY",
                "OK-ACCESS-SIGN",
                "OK-ACCESS-TIMESTAMP",
                "OK-ACCESS-PASSPHRASE",
            ] {
                assert!(h.get(name).unwrap().is_sensitive());
            }
        }
    }

    use AccountEnv::{Live, Testnet};
    use AccountProduct::{Spot, UsdM};
    use AccountVenue::{Binance, Okx};

    const ALL_HOSTS: [&str; 17] = [
        "api.binance.com",
        "fapi.binance.com",
        "testnet.binancefuture.com",
        "testnet.binance.vision",
        "www.okx.com",
        "openapi.okx.com",
        "us.okx.com",
        "eea.okx.com",
        "evil.example",
        "fstream.binance.com",
        "ws-api.binance.com",
        "fstream.binancefuture.com",
        "ws-api.testnet.binance.vision",
        "ws.okx.com",
        "wspap.okx.com",
        "stream.binance.com",
        "demo-api.okx.com",
    ];

    #[test]
    fn https_allow_list_table() {
        // (venue, env, product) -> the only hosts that pass.
        let table: [(AccountVenue, AccountEnv, AccountProduct, &[&str]); 8] = [
            (
                Binance,
                Live,
                Spot,
                &["api.binance.com", "fapi.binance.com"],
            ),
            (
                Binance,
                Live,
                UsdM,
                &["api.binance.com", "fapi.binance.com"],
            ),
            (Binance, Testnet, UsdM, &["testnet.binancefuture.com"]),
            (Binance, Testnet, Spot, &["testnet.binance.vision"]),
            (Okx, Live, UsdM, &["www.okx.com", "openapi.okx.com"]),
            (Okx, Testnet, UsdM, &["www.okx.com", "openapi.okx.com"]),
            (Okx, Live, Spot, &["www.okx.com", "openapi.okx.com"]),
            (Okx, Testnet, Spot, &["www.okx.com", "openapi.okx.com"]),
        ];
        for (venue, env, product, allowed) in table {
            for host in ALL_HOSTS {
                let got = https_endpoint(venue, env, product, host, "/x?a=1");
                assert_eq!(
                    got.is_ok(),
                    allowed.contains(&host),
                    "{venue:?} {env:?} {product:?} {host}"
                );
                if let Ok(url) = got {
                    assert_eq!(url.scheme(), "https");
                }
            }
        }
    }

    #[test]
    fn testnet_futures_key_never_reaches_the_spot_testnet_and_back() {
        assert!(https_endpoint(Binance, Testnet, UsdM, "testnet.binance.vision", "/").is_err());
        assert!(https_endpoint(Binance, Testnet, Spot, "testnet.binancefuture.com", "/").is_err());
        // Live and testnet never mix either.
        assert!(https_endpoint(Binance, Live, UsdM, "testnet.binancefuture.com", "/").is_err());
        assert!(https_endpoint(Binance, Testnet, UsdM, "fapi.binance.com", "/").is_err());
    }

    #[test]
    fn https_endpoint_refuses_host_smuggling_and_ports() {
        for host in [
            "www.okx.com@evil.example",
            "www.okx.com:8443",
            "www.okx.com/",
            "evil.example",
        ] {
            assert!(
                https_endpoint(Okx, Live, UsdM, host, "/").is_err(),
                "{host}"
            );
        }
    }

    #[test]
    fn wss_allow_list_table() {
        // (venue, env, product) -> the only hosts that pass on wss:443.
        let table: [(AccountVenue, AccountEnv, AccountProduct, &[&str]); 8] = [
            (
                Binance,
                Live,
                Spot,
                &["fstream.binance.com", "ws-api.binance.com"],
            ),
            (
                Binance,
                Live,
                UsdM,
                &["fstream.binance.com", "ws-api.binance.com"],
            ),
            (Binance, Testnet, UsdM, &["fstream.binancefuture.com"]),
            (Binance, Testnet, Spot, &["ws-api.testnet.binance.vision"]),
            (Okx, Live, UsdM, &["ws.okx.com"]),
            (Okx, Testnet, UsdM, &["wspap.okx.com"]),
            (Okx, Live, Spot, &["ws.okx.com"]),
            (Okx, Testnet, Spot, &["wspap.okx.com"]),
        ];
        for (venue, env, product, allowed) in table {
            for host in ALL_HOSTS {
                let wss = wss_endpoint(venue, env, product, &format!("wss://{host}/ws"));
                assert_eq!(
                    wss.is_ok(),
                    allowed.contains(&host),
                    "{venue:?} {env:?} {host}"
                );
                // Explicit :443 is the same as the default.
                let explicit = wss_endpoint(venue, env, product, &format!("wss://{host}:443/ws"));
                assert_eq!(explicit.is_ok(), allowed.contains(&host));
                // ws:// and other ports are always refused.
                for url in [
                    format!("ws://{host}/ws"),
                    format!("ws://{host}:443/ws"),
                    format!("wss://{host}:8443/ws"),
                    format!("wss://{host}:9443/ws"),
                    format!("https://{host}/ws"),
                ] {
                    assert!(wss_endpoint(venue, env, product, &url).is_err(), "{url}");
                }
            }
        }
    }

    #[test]
    fn wss_environments_do_not_mix() {
        assert!(wss_endpoint(Okx, Live, UsdM, "wss://wspap.okx.com/ws/v5/private").is_err());
        assert!(wss_endpoint(Okx, Testnet, UsdM, "wss://ws.okx.com/ws/v5/private").is_err());
        assert!(wss_endpoint(Binance, Testnet, UsdM, "wss://fstream.binance.com/ws").is_err());
        assert!(wss_endpoint(Okx, Live, UsdM, "wss://user:pw@ws.okx.com/ws").is_err());
        assert!(wss_endpoint(Okx, Live, UsdM, "not a url").is_err());
    }

    #[test]
    fn proxy_snapshot_comparison_and_broken_state() {
        use crate::proxy::{Proxy, ProxyScheme};
        let p1 =
            ProxyState::Proxied(Proxy::new(ProxyScheme::Http, "127.0.0.1", 8080, None).unwrap());
        let p2 =
            ProxyState::Proxied(Proxy::new(ProxyScheme::Http, "127.0.0.1", 9090, None).unwrap());
        assert_eq!(
            check_proxy_snapshot(&ProxyState::Direct, &ProxyState::Direct),
            Ok(())
        );
        assert_eq!(check_proxy_snapshot(&p1, &p1.clone()), Ok(()));
        assert_eq!(
            check_proxy_snapshot(&p1, &p2),
            Err(AccountError::ProxyChanged)
        );
        // Adding or removing a proxy after startup is also a change, never a silent switch.
        assert_eq!(
            check_proxy_snapshot(&ProxyState::Direct, &p1),
            Err(AccountError::ProxyChanged)
        );
        assert_eq!(
            check_proxy_snapshot(&p1, &ProxyState::Direct),
            Err(AccountError::ProxyChanged)
        );
        // Broken refuses whichever side it is on, and equal Broken states still refuse.
        for (a, b) in [
            (ProxyState::Broken, ProxyState::Direct),
            (ProxyState::Direct, ProxyState::Broken),
            (ProxyState::Broken, ProxyState::Broken),
            (p1.clone(), ProxyState::Broken),
        ] {
            assert_eq!(check_proxy_snapshot(&a, &b), Err(AccountError::ProxyBroken));
        }
        assert_eq!(ProxyState::from_config(None), ProxyState::Direct);
    }

    /// (HTTP status, body or None for unparseable, expected delivery)
    #[test]
    fn delivery_mapping_table() {
        let cases: [(u16, Option<Value>, Delivery); 20] = [
            (
                400,
                Some(json!({"code": -1121, "msg": "Invalid symbol"})),
                Delivery::Rejected,
            ),
            (
                401,
                Some(json!({"code": -2015, "msg": "x"})),
                Delivery::Rejected,
            ),
            (
                400,
                Some(json!({"code": -1021, "msg": "x"})),
                Delivery::Rejected,
            ),
            (
                400,
                Some(json!({"code": "50102", "msg": "x"})),
                Delivery::Rejected,
            ),
            (429, None, Delivery::Rejected),
            (418, None, Delivery::Rejected),
            (403, None, Delivery::Rejected),
            // A 4xx without a `code` (timeout from a proxy, CDN error page) is inconclusive.
            (400, None, Delivery::Unknown),
            (404, None, Delivery::Unknown),
            (408, None, Delivery::Unknown),
            (409, Some(json!({"msg": "no code"})), Delivery::Unknown),
            // Inconclusive: the order may exist.
            (500, None, Delivery::Unknown),
            (
                503,
                Some(json!({"code": -1000, "msg": "x"})),
                Delivery::Unknown,
            ),
            (302, None, Delivery::Unknown),
            // Codes that say "status unknown" win over the 4xx / 5xx reading.
            (
                503,
                Some(json!({"code": -1006, "msg": "x"})),
                Delivery::Unknown,
            ),
            (
                504,
                Some(json!({"code": -1007, "msg": "x"})),
                Delivery::Unknown,
            ),
            (
                400,
                Some(json!({"code": -1007, "msg": "x"})),
                Delivery::Unknown,
            ),
            (
                200,
                Some(json!({"code": "50004", "msg": "x"})),
                Delivery::Unknown,
            ),
            (
                400,
                Some(json!({"code": "51149", "msg": "x"})),
                Delivery::Unknown,
            ),
            (
                400,
                Some(json!({"code": "51008", "msg": "x"})),
                Delivery::Rejected,
            ),
        ];
        for (status, body, want) in cases {
            let err = exchange_error(status, &body.clone().unwrap_or(Value::Null));
            assert_eq!(err.delivery(), want, "{status} {body:?} -> {err:?}");
        }
    }

    #[test]
    fn delivery_of_transport_level_errors() {
        // Never left this process.
        for e in [
            AccountError::InvalidEndpoint,
            AccountError::InvalidCredentials,
            AccountError::ProxyChanged,
            AccountError::ProxyBroken,
            AccountError::LocalRateLimited,
            AccountError::Connect,
        ] {
            assert_eq!(e.delivery(), Delivery::NotSent, "{e:?}");
        }
        // Timeout, mid-flight disconnect, unreadable or oversized 2xx body: unknown.
        for e in [
            AccountError::Timeout,
            AccountError::Network,
            AccountError::BadResponse,
            AccountError::Redirected,
            AccountError::Http(500),
            AccountError::Http(404),
            AccountError::Http(408),
        ] {
            assert_eq!(e.delivery(), Delivery::Unknown, "{e:?}");
        }
        assert_eq!(AccountError::Http(403).delivery(), Delivery::Rejected);
    }

    #[test]
    fn binance_withdraw_permission_is_rejected() {
        let v = json!({"ipRestrict": true, "enableWithdrawals": true, "enableFutures": true});
        assert!(matches!(
            assess_binance_restrictions(&v, UsdM).0,
            Verdict::Rejected(_)
        ));
    }

    #[test]
    fn binance_missing_withdraw_field_fails_closed() {
        for v in [
            json!({}),
            json!({"enableWithdrawals": "false"}),
            json!({"enableWithdrawals": null}),
        ] {
            assert!(
                matches!(
                    assess_binance_restrictions(&v, UsdM).0,
                    Verdict::Rejected(_)
                ),
                "{v}"
            );
        }
    }

    #[test]
    fn binance_safe_key_is_usable_with_ip_warning() {
        let v = json!({
            "ipRestrict": false, "createTime": 1698645219000_u64, "enableWithdrawals": false,
            "enableInternalTransfer": true, "permitsUniversalTransfer": true,
            "enableVanillaOptions": false, "enableReading": true, "enableFutures": true,
            "enableMargin": false, "enableSpotAndMarginTrading": false
        });
        let (verdict, warnings) = assess_binance_restrictions(&v, UsdM);
        assert_eq!(verdict, Verdict::Usable);
        assert!(warnings.iter().any(|w| w.contains("no IP restriction")));
        assert!(warnings.iter().any(|w| w.contains("transfer funds")));
    }

    #[test]
    fn binance_position_mode_parsing() {
        assert_eq!(
            parse_binance_position_mode(&json!({"dualSidePosition": true})),
            Some(PositionMode::Hedge)
        );
        assert_eq!(
            parse_binance_position_mode(&json!({"dualSidePosition": false})),
            Some(PositionMode::OneWay)
        );
        assert_eq!(
            parse_binance_position_mode(&json!({"dualSidePosition": "true"})),
            None
        );
        assert_eq!(parse_binance_position_mode(&json!({})), None);
    }

    fn okx_body(perm: &str, ip: &str, pos_mode: &str) -> Value {
        json!({"code": "0", "msg": "", "data": [{"perm": perm, "ip": ip, "posMode": pos_mode, "uid": "1"}]})
    }

    #[test]
    fn okx_withdraw_token_is_rejected() {
        // Shape of the official docs example: "read_only,withdraw,trade".
        let r = assess_okx_config(&okx_body("read_only,withdraw,trade", "", "long_short_mode"));
        assert!(matches!(r.verdict, Verdict::Rejected(_)));
        let r = assess_okx_config(&okx_body("read_only, withdraw", "1.2.3.4", "net_mode"));
        assert!(matches!(r.verdict, Verdict::Rejected(_)));
    }

    #[test]
    fn okx_perm_match_is_exact_token() {
        // A longer token that merely contains "withdraw" is not the withdraw permission.
        let r = assess_okx_config(&okx_body(
            "read_only,trade,no_withdraw_x",
            "1.2.3.4",
            "net_mode",
        ));
        assert_eq!(r.verdict, Verdict::Usable);
    }

    #[test]
    fn okx_missing_or_bad_data_fails_closed() {
        for v in [
            json!({"code": "0", "data": []}),
            json!({"code": "0", "data": [{"posMode": "net_mode"}]}),
            json!({"code": "50111", "msg": "Invalid OK-ACCESS-KEY", "data": []}),
            json!({"data": [{"perm": "read_only"}]}),
        ] {
            assert!(
                matches!(assess_okx_config(&v).verdict, Verdict::Rejected(_)),
                "{v}"
            );
        }
    }

    #[test]
    fn okx_ip_and_position_mode() {
        let r = assess_okx_config(&okx_body("read_only,trade", "", "long_short_mode"));
        assert_eq!(r.verdict, Verdict::Usable);
        assert_eq!(r.actual_mode, Some(PositionMode::Hedge));
        assert!(r.warnings.iter().any(|w| w.contains("no IP restriction")));

        let r = assess_okx_config(&okx_body("read_only,trade", "1.2.3.4", "net_mode"));
        assert_eq!(r.actual_mode, Some(PositionMode::OneWay));
        assert!(r.warnings.is_empty());

        let r = assess_okx_config(&okx_body("read_only", "1.2.3.4", "weird"));
        assert_eq!(r.actual_mode, None);
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("Unknown position mode"))
        );
    }

    #[test]
    fn position_mode_mismatch_warns_but_does_not_block() {
        assert!(position_mode_warning(PositionMode::OneWay, PositionMode::OneWay).is_none());
        assert!(position_mode_warning(PositionMode::OneWay, PositionMode::Hedge).is_some());
    }

    #[test]
    fn usability_rules_by_environment() {
        let u = Verdict::Usable;
        let unv = Verdict::Unverified("x".into());
        let rej = Verdict::Rejected("x".into());
        assert!(is_usable(AccountEnv::Live, &u));
        assert!(!is_usable(AccountEnv::Live, &unv));
        assert!(is_usable(AccountEnv::Testnet, &u));
        assert!(is_usable(AccountEnv::Testnet, &unv));
        assert!(!is_usable(AccountEnv::Live, &rej));
        assert!(!is_usable(AccountEnv::Testnet, &rej));
    }

    #[test]
    fn exchange_errors_map_to_clear_messages() {
        assert_eq!(
            exchange_error(400, &json!({"code": -1021, "msg": "x"})),
            AccountError::ClockSkew
        );
        assert_eq!(
            exchange_error(401, &json!({"code": "50102", "msg": "x"})),
            AccountError::ClockSkew
        );
        assert_eq!(
            exchange_error(429, &Value::Null),
            AccountError::RateLimited(429)
        );
        assert_eq!(
            exchange_error(418, &Value::Null),
            AccountError::RateLimited(418)
        );
        assert_eq!(exchange_error(302, &Value::Null), AccountError::Redirected);
        assert_eq!(exchange_error(500, &Value::Null), AccountError::Http(500));
        let e = exchange_error(401, &json!({"code": -2015, "msg": "Invalid API-key"}));
        assert_eq!(e.to_string(), "exchange error -2015: Invalid API-key");
    }

    #[test]
    fn debug_output_masks_credentials() {
        let creds =
            ApiCredentials::new(FAKE_KEY.into(), FAKE_SECRET.into(), Some(FAKE_PASS.into()));
        let shown = format!("{creds:?}");
        for s in [FAKE_KEY, FAKE_SECRET, FAKE_PASS] {
            assert!(!shown.contains(s));
        }
        assert!(shown.contains("***"));
    }

    #[test]
    fn credentials_comparison() {
        let a = ApiCredentials::new("k".into(), "s".into(), Some("p".into()));
        assert!(a.same_as(&ApiCredentials::new(
            "k".into(),
            "s".into(),
            Some("p".into())
        )));
        assert!(!a.same_as(&ApiCredentials::new("k".into(), "s".into(), None)));
        assert!(!a.same_as(&ApiCredentials::new(
            "k".into(),
            "x".into(),
            Some("p".into())
        )));
    }

    // ── Transport: nothing sensitive in logs or errors ──────────────────

    use std::io::{Read, Write};
    use std::sync::{Mutex, Once};

    static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    struct CaptureLogger;
    impl log::Log for CaptureLogger {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            LOGGED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("{}", record.args()));
        }
        fn flush(&self) {}
    }

    fn capture_logs() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let _ = log::set_logger(&CaptureLogger);
            log::set_max_level(log::LevelFilter::Trace);
        });
    }

    /// How the fake server answers one connection.
    enum Reply {
        Raw(&'static str),
        /// Send the header and part of the body, then close.
        Truncated,
        /// Accept and never answer.
        Silent,
    }

    /// Serve one connection with `reply` on a throw-away local port (plain http, which is
    /// fine: the signed `https_only` rule lives in the real client, not in `send`).
    fn serve_once(reply: Reply) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = conn.read(&mut buf);
            match reply {
                Reply::Raw(text) => {
                    let _ = conn.write_all(text.as_bytes());
                }
                Reply::Truncated => {
                    let _ = conn.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\nContent-Type: application/json\r\n\r\n{\"orderId\":1",
                    );
                }
                Reply::Silent => std::thread::sleep(std::time::Duration::from_secs(2)),
            }
        });
        port
    }

    const CT: &str = "Content-Type: application/json\r\nConnection: close\r\n";

    fn http(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\n{CT}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    const BODY_MARK: &str = "quantity=7.77&price=1234.5";

    async fn run(reply: Reply) -> Result<Value, AccountError> {
        capture_logs();
        let port = serve_once(reply);
        let url = reqwest::Url::parse(&format!(
            "http://127.0.0.1:{port}/fapi/v1/order?timestamp=1&signature={}",
            binance_sign(FAKE_SECRET, "timestamp=1")
        ))
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("X-MBX-APIKEY", sensitive(FAKE_KEY).unwrap());
        headers.insert("OK-ACCESS-SIGN", sensitive("sig-header-value").unwrap());
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(600))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        send(
            &client,
            reqwest::Method::POST,
            url,
            headers,
            Some(format!("{BODY_MARK}&signature=sigvalue123")),
        )
        .await
    }

    fn assert_clean(text: &str) {
        for secret in [
            "signature=",
            "sigvalue123",
            "OK-ACCESS-SIGN",
            "sig-header-value",
            FAKE_KEY,
            FAKE_SECRET,
            "timestamp=",
            BODY_MARK,
            "7.77",
            "1234.5",
            "987654321", // order id in the response
            "127.0.0.1",
        ] {
            assert!(!text.contains(secret), "leaked {secret}: {text}");
        }
    }

    #[tokio::test]
    async fn send_never_logs_or_reports_body_query_or_response_content() {
        let ok_body = r#"{"orderId":987654321,"status":"NEW"}"#;
        let results = [
            run(Reply::Raw(Box::leak(
                http("200 OK", ok_body).into_boxed_str(),
            )))
            .await,
            run(Reply::Raw(Box::leak(
                http(
                    "400 Bad Request",
                    r#"{"code":-1121,"msg":"bad echoed-ip-1.2.3.4"}"#,
                )
                .into_boxed_str(),
            )))
            .await,
            run(Reply::Raw(Box::leak(
                http("503 Service Unavailable", r#"{"code":-1000,"msg":"x"}"#).into_boxed_str(),
            )))
            .await,
            run(Reply::Raw(Box::leak(
                http("200 OK", "not json").into_boxed_str(),
            )))
            .await,
            run(Reply::Truncated).await,
            run(Reply::Silent).await,
        ];
        // Success returns the content to the caller, only the caller sees it.
        assert_eq!(results[0].as_ref().unwrap()["orderId"], 987654321);
        for r in results.iter().skip(1) {
            let err = r.as_ref().unwrap_err();
            assert_clean(&err.to_string());
            assert_clean(&format!("{err:?}").replace("echoed-ip-1.2.3.4", ""));
        }
        // Errors keep their meaning for a3's three-way mapping.
        assert_eq!(
            results[1].as_ref().unwrap_err().delivery(),
            Delivery::Rejected
        );
        assert_eq!(results[2].as_ref().unwrap_err(), &AccountError::Http(503));
        assert_eq!(results[3].as_ref().unwrap_err(), &AccountError::BadResponse);
        assert_eq!(results[4].as_ref().unwrap_err(), &AccountError::Network);
        assert_eq!(results[5].as_ref().unwrap_err(), &AccountError::Timeout);
        for r in &results[2..6] {
            assert_eq!(r.as_ref().unwrap_err().delivery(), Delivery::Unknown);
        }

        let logged = LOGGED.lock().unwrap_or_else(|e| e.into_inner()).join("\n");
        // The log keeps `METHOD host path` and statuses.
        assert!(logged.contains("signed request POST"));
        assert_clean(&logged.replace("127.0.0.1", ""));
        assert!(!logged.contains("echoed-ip"));
    }

    #[tokio::test]
    async fn connect_failure_is_not_sent_and_leaks_nothing() {
        // Nothing listens on 127.0.0.1:1, so this fails fast with a connect error.
        let url = reqwest::Url::parse(&format!(
            "https://127.0.0.1:1/sapi/v1/x?timestamp=1&signature={}",
            binance_sign(FAKE_SECRET, "timestamp=1")
        ))
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("X-MBX-APIKEY", sensitive(FAKE_KEY).unwrap());
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let err = send(&client, reqwest::Method::GET, url, headers, None)
            .await
            .unwrap_err();
        assert_eq!(err.delivery(), Delivery::NotSent);
        assert_clean(&err.to_string());
        assert_clean(&format!("{err:?}"));
    }
}
