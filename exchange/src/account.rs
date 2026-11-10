//! Exchange account credentials, request signing and the connection test for
//! Binance and OKX.
//!
//! Everything here is independent of the GUI. Secrets are wrapped in
//! [`secrecy::SecretString`] and only exposed inside the signing functions and
//! when a request header is built.
//!
//! Signed requests deliberately bypass `adapter::http::HttpHub`: that path logs the full
//! URL (including the Binance `signature` query parameter) and a body preview.
//! [`send_get`] only ever reports `method + host + path`, never the query.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use ring::hmac;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

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
pub fn binance_sign(secret: &str, query: &str) -> String {
    let tag = hmac_sha256(secret, query);
    let mut out = String::with_capacity(64);
    for b in tag.as_ref() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// OKX: base64(HMAC-SHA256(secret, timestamp + method + requestPath + body)).
/// `request_path` includes the query string.
pub fn okx_sign(
    secret: &str,
    timestamp: &str,
    method: &str,
    request_path: &str,
    body: &str,
) -> String {
    let prehash = format!("{timestamp}{method}{request_path}{body}");
    BASE64.encode(hmac_sha256(secret, &prehash).as_ref())
}

fn okx_timestamp(now: chrono::DateTime<chrono::Utc>) -> String {
    now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn sensitive(value: &str) -> Result<HeaderValue, AccountError> {
    let mut v = HeaderValue::from_str(value).map_err(|_| AccountError::InvalidCredentials)?;
    v.set_sensitive(true);
    Ok(v)
}

/// OKX request headers. `x-simulated-trading: 1` is sent for testnet (demo) and never for live.
fn okx_headers(
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

/// Hosts a signed request for `(venue, env)` may be sent to. Hard-coded on purpose:
/// never read from settings or user input.
fn allowed_hosts(venue: AccountVenue, env: AccountEnv) -> &'static [&'static str] {
    match (venue, env) {
        (AccountVenue::Binance, AccountEnv::Live) => &["api.binance.com", "fapi.binance.com"],
        (AccountVenue::Binance, AccountEnv::Testnet) => &["testnet.binancefuture.com"],
        (AccountVenue::Okx, _) => &["www.okx.com"],
    }
}

/// Build an `https://` URL after checking the host against the allow-list.
fn endpoint_url(
    venue: AccountVenue,
    env: AccountEnv,
    host: &str,
    path_and_query: &str,
) -> Result<reqwest::Url, AccountError> {
    if !allowed_hosts(venue, env).contains(&host) {
        return Err(AccountError::InvalidEndpoint);
    }
    let url = reqwest::Url::parse(&format!("https://{host}{path_and_query}"))
        .map_err(|_| AccountError::InvalidEndpoint)?;
    if url.scheme() != "https" || url.host_str() != Some(host) {
        return Err(AccountError::InvalidEndpoint);
    }
    Ok(url)
}

// ── Errors ──────────────────────────────────────────────────────────────

/// Error from a signed request. Display never contains a URL, query, header or body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountError {
    InvalidEndpoint,
    InvalidCredentials,
    Timeout,
    Connect,
    Network,
    Redirected,
    RateLimited(u16),
    ClockSkew,
    Http(u16),
    BadResponse,
    Exchange { code: String, msg: String },
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

/// Send one GET and return the parsed JSON body of a 2xx response.
///
/// Logs and errors carry only `GET host path` and the status, never the query.
/// The client must not follow redirects (see `connector::client::signed_client`), so
/// credential headers are never forwarded to another host.
async fn send_get(
    client: &reqwest::Client,
    url: reqwest::Url,
    headers: HeaderMap,
) -> Result<Value, AccountError> {
    let host = url.host_str().unwrap_or("?").to_string();
    let path = url.path().to_string();

    let mut resp = client
        .get(url)
        .headers(headers)
        .send()
        .await
        .map_err(|e| classify_reqwest(&e))?;
    let status = resp.status().as_u16();
    log::debug!("signed request GET {host}{path} -> {status}");

    if resp
        .content_length()
        .is_some_and(|n| n > MAX_BODY_BYTES as u64)
    {
        return Err(AccountError::BadResponse);
    }
    // Chunked responses carry no Content-Length, so the size is enforced while reading.
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| classify_reqwest(&e))? {
        if bytes.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(AccountError::BadResponse);
        }
        bytes.extend_from_slice(&chunk);
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
        log::warn!("signed request GET {host}{path} failed: {summary}");
        return Err(err);
    }
    body.map_err(|_| {
        log::warn!("signed request GET {host}{path}: unreadable response");
        AccountError::BadResponse
    })
}

// ── Binance ─────────────────────────────────────────────────────────────

fn binance_hosts(env: AccountEnv) -> (&'static str, &'static str, &'static str) {
    // (host for time + /sapi, host for /fapi, time path)
    match env {
        AccountEnv::Live => ("api.binance.com", "fapi.binance.com", "/api/v3/time"),
        AccountEnv::Testnet => (
            "testnet.binancefuture.com",
            "testnet.binancefuture.com",
            "/fapi/v1/time",
        ),
    }
}

/// Parse Binance `GET /sapi/v1/account/apiRestrictions`.
///
/// Fail closed: a missing or non-boolean `enableWithdrawals` is `Rejected`.
pub fn assess_binance_restrictions(v: &Value) -> (Verdict, Vec<String>) {
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
    if flag("enableFutures") != Some(true) {
        warnings.push("API key does not have Futures permission enabled.".to_string());
    }
    (verdict, warnings)
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
    creds: &ApiCredentials,
    host: &str,
    path: &str,
    offset_ms: i64,
) -> Result<Value, AccountError> {
    let timestamp = chrono::Utc::now().timestamp_millis() + offset_ms;
    let query = format!("recvWindow={BINANCE_RECV_WINDOW_MS}&timestamp={timestamp}");
    let signature = binance_sign(creds.secret().expose_secret(), &query);
    let url = endpoint_url(
        AccountVenue::Binance,
        env,
        host,
        &format!("{path}?{query}&signature={signature}"),
    )?;
    let mut headers = HeaderMap::new();
    headers.insert("X-MBX-APIKEY", sensitive(creds.key().expose_secret())?);
    send_get(client, url, headers).await
}

/// Server time minus local time. A failure falls back to no correction.
async fn binance_clock_offset(client: &reqwest::Client, env: AccountEnv) -> i64 {
    let (host, _, path) = binance_hosts(env);
    let Ok(url) = endpoint_url(AccountVenue::Binance, env, host, path) else {
        return 0;
    };
    let local = chrono::Utc::now().timestamp_millis();
    match send_get(client, url, HeaderMap::new()).await {
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
    creds: &ApiCredentials,
    desired: PositionMode,
) -> CheckReport {
    let (sapi_host, fapi_host, _) = binance_hosts(env);
    let offset = binance_clock_offset(client, env).await;

    let (mut verdict, mut warnings) = match env {
        AccountEnv::Live => match binance_signed_get(
            client,
            env,
            creds,
            sapi_host,
            "/sapi/v1/account/apiRestrictions",
            offset,
        )
        .await
        {
            Ok(v) => assess_binance_restrictions(&v),
            Err(e) => (unconfirmed(&e.to_string()), Vec::new()),
        },
        // The futures testnet has no /sapi, so permissions cannot be read.
        AccountEnv::Testnet => (
            Verdict::Unverified("cannot confirm permissions (testnet has no /sapi)".into()),
            Vec::new(),
        ),
    };

    let mut actual_mode = None;
    if !matches!(verdict, Verdict::Rejected(_)) {
        match binance_signed_get(
            client,
            env,
            creds,
            fapi_host,
            "/fapi/v1/positionSide/dual",
            offset,
        )
        .await
        {
            Ok(v) => match parse_binance_position_mode(&v) {
                Some(mode) => actual_mode = Some(mode),
                None => warnings.push("Could not read the futures position mode.".into()),
            },
            Err(e) if env == AccountEnv::Testnet => {
                // On testnet this is the only signed call, so a failure means the key
                // could not be checked at all.
                verdict = Verdict::Rejected(format!("key could not be checked on testnet ({e})"));
            }
            Err(e) => warnings.push(format!("Could not read the futures position mode ({e}).")),
        }
    }
    if let Some(actual) = actual_mode {
        warnings.extend(position_mode_warning(desired, actual));
    }
    CheckReport {
        verdict,
        warnings,
        actual_mode,
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

    CheckReport {
        verdict,
        warnings,
        actual_mode,
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
    let url = match endpoint_url(AccountVenue::Okx, env, "www.okx.com", PATH) {
        Ok(u) => u,
        Err(e) => return fail(e),
    };

    match send_get(client, url, headers).await {
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
    creds: &ApiCredentials,
    desired: PositionMode,
) -> CheckReport {
    match venue {
        AccountVenue::Binance => check_binance(client, env, creds, desired).await,
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

    #[test]
    fn endpoints_are_https_and_allow_listed() {
        let url =
            endpoint_url(AccountVenue::Okx, AccountEnv::Live, "www.okx.com", "/x?a=1").unwrap();
        assert_eq!(url.scheme(), "https");
        // Wrong venue/env/host combinations are refused.
        assert!(endpoint_url(AccountVenue::Binance, AccountEnv::Live, "www.okx.com", "/").is_err());
        assert!(
            endpoint_url(
                AccountVenue::Binance,
                AccountEnv::Live,
                "testnet.binancefuture.com",
                "/"
            )
            .is_err()
        );
        assert!(
            endpoint_url(
                AccountVenue::Binance,
                AccountEnv::Testnet,
                "fapi.binance.com",
                "/"
            )
            .is_err()
        );
        assert!(endpoint_url(AccountVenue::Okx, AccountEnv::Live, "evil.example", "/").is_err());
        // Host smuggling through the host argument.
        assert!(
            endpoint_url(
                AccountVenue::Okx,
                AccountEnv::Live,
                "www.okx.com@evil.example",
                "/"
            )
            .is_err()
        );
    }

    #[test]
    fn binance_withdraw_permission_is_rejected() {
        let v = json!({"ipRestrict": true, "enableWithdrawals": true, "enableFutures": true});
        assert!(matches!(
            assess_binance_restrictions(&v).0,
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
                matches!(assess_binance_restrictions(&v).0, Verdict::Rejected(_)),
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
        let (verdict, warnings) = assess_binance_restrictions(&v);
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

    #[tokio::test]
    async fn transport_errors_do_not_leak_signature_key_or_secret() {
        // Nothing listens on 127.0.0.1:1, so this fails fast with a connect error.
        let url = reqwest::Url::parse(&format!(
            "https://127.0.0.1:1/sapi/v1/x?timestamp=1&signature={}",
            binance_sign(FAKE_SECRET, "timestamp=1")
        ))
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("X-MBX-APIKEY", sensitive(FAKE_KEY).unwrap());
        headers.insert("OK-ACCESS-SIGN", sensitive("sig-header-value").unwrap());

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let err = send_get(&client, url, headers).await.unwrap_err();
        let (display, debug) = (err.to_string(), format!("{err:?}"));
        for shown in [display, debug] {
            for secret in [
                "signature=",
                "OK-ACCESS-SIGN",
                "sig-header-value",
                FAKE_KEY,
                FAKE_SECRET,
                "timestamp=",
                "127.0.0.1",
            ] {
                assert!(!shown.contains(secret), "leaked {secret}: {shown}");
            }
        }
    }
}
