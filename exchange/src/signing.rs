//! Request encoding for signed trading requests.
//!
//! The bytes sent must equal the string that was signed, or the exchange answers `-1022`
//! (and a stray `&`/`=` in a user-controlled value could inject extra parameters). These
//! helpers make that structural: the signed string and the sent string come from one
//! function, and symbols are restricted to `[A-Z0-9-]`.

use crate::account::{binance_sign, okx_sign};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigningError {
    /// Symbol / instId contains characters outside `[A-Z0-9-]`, or is empty or too long.
    InvalidSymbol,
    /// Parameter name is empty or not alphanumeric.
    InvalidParamName,
}

impl fmt::Display for SigningError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SigningError::InvalidSymbol => f.write_str("invalid symbol"),
            SigningError::InvalidParamName => f.write_str("invalid request parameter name"),
        }
    }
}

const MAX_SYMBOL_LEN: usize = 32;

/// Accept a Binance symbol or OKX instId: `[A-Z0-9-]`, 1 to 32 characters.
/// (Binance delivery symbols such as `BTCUSDT_240628` contain `_` and are rejected on
/// purpose; only perpetual and spot symbols are supported.)
pub fn validate_symbol(symbol: &str) -> Result<&str, SigningError> {
    let ok = !symbol.is_empty()
        && symbol.len() <= MAX_SYMBOL_LEN
        && symbol
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(symbol)
    } else {
        Err(SigningError::InvalidSymbol)
    }
}

/// Percent-encode everything except RFC 3986 unreserved characters.
fn percent_encode(value: &str, out: &mut String) {
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

/// `name=value&name=value`, in the given order, values percent-encoded.
/// The order is the caller's (a builder keeps a fixed order per order type).
pub fn form_encode(params: &[(&str, &str)]) -> Result<String, SigningError> {
    let mut out = String::new();
    for (i, (name, value)) in params.iter().enumerate() {
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(SigningError::InvalidParamName);
        }
        if i > 0 {
            out.push('&');
        }
        out.push_str(name);
        out.push('=');
        percent_encode(value, &mut out);
    }
    Ok(out)
}

/// A Binance signed form: `query` is what was signed, `body` is what is sent
/// (`query` + `&signature=…`). Use `body` as the POST body, or after `?` for GET/DELETE.
#[derive(Clone, PartialEq, Eq)]
pub struct BinanceSigned {
    pub query: String,
    pub body: String,
}

// Masked: the query holds order parameters and the body holds the signature.
impl fmt::Debug for BinanceSigned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BinanceSigned(<redacted>)")
    }
}

/// Encode `params` once, sign that string, and append the signature.
pub fn binance_signed_form(
    secret: &str,
    params: &[(&str, &str)],
) -> Result<BinanceSigned, SigningError> {
    let query = form_encode(params)?;
    let signature = binance_sign(secret, &query);
    let body = format!("{query}&signature={signature}");
    Ok(BinanceSigned { query, body })
}

/// An OKX signed request: `body` is both what was signed and what must be sent.
#[derive(Clone, PartialEq, Eq)]
pub struct OkxSigned {
    pub body: String,
    pub sign: String,
}

// Masked: the body holds order parameters and `sign` is the signature.
impl fmt::Debug for OkxSigned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OkxSigned(<redacted>)")
    }
}

/// Serialise `body` once, sign that exact string, and return both.
pub fn okx_signed_json<T: serde::Serialize>(
    secret: &str,
    timestamp: &str,
    method: &str,
    request_path: &str,
    body: &T,
) -> Result<OkxSigned, serde_json::Error> {
    let body = serde_json::to_string(body)?;
    let sign = okx_sign(secret, timestamp, method, request_path, &body);
    Ok(OkxSigned { body, sign })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Public example values from Binance's official docs (request-security): not real credentials.
    const DOC_SECRET: &str = "NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j";
    const DOC_QUERY: &str = "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559";
    const DOC_SIGNATURE: &str = "c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71";

    #[test]
    fn binance_form_signs_exactly_what_it_sends() {
        let params = [
            ("symbol", "LTCBTC"),
            ("side", "BUY"),
            ("type", "LIMIT"),
            ("timeInForce", "GTC"),
            ("quantity", "1"),
            ("price", "0.1"),
            ("recvWindow", "5000"),
            ("timestamp", "1499827319559"),
        ];
        let signed = binance_signed_form(DOC_SECRET, &params).unwrap();
        assert_eq!(signed.query, DOC_QUERY);
        assert_eq!(
            signed.body,
            format!("{DOC_QUERY}&signature={DOC_SIGNATURE}")
        );
        // Re-signing the sent bytes (minus the signature suffix) reproduces the signature.
        let (sent_query, sent_sig) = signed.body.rsplit_once("&signature=").unwrap();
        assert_eq!(binance_sign(DOC_SECRET, sent_query), sent_sig);
    }

    #[test]
    fn signed_requests_are_masked_in_debug_output() {
        let b =
            binance_signed_form(DOC_SECRET, &[("symbol", "LTCBTC"), ("quantity", "7")]).unwrap();
        let o = okx_signed_json("s", "t", "POST", "/p", &json!({"sz": "7"})).unwrap();
        for text in [
            format!("{b:?}"),
            format!("{b:#?}"),
            format!("{o:?}"),
            format!("{o:#?}"),
        ] {
            assert!(!text.contains("LTCBTC") && !text.contains("signature"));
            assert!(!text.contains("sz") && !text.contains(&o.sign));
            assert!(text.contains("redacted"));
        }
    }

    #[test]
    fn binance_form_keeps_caller_order() {
        let a = form_encode(&[("a", "1"), ("b", "2")]).unwrap();
        let b = form_encode(&[("b", "2"), ("a", "1")]).unwrap();
        assert_eq!(a, "a=1&b=2");
        assert_eq!(b, "b=2&a=1");
    }

    #[test]
    fn values_are_percent_encoded_so_they_cannot_inject_parameters() {
        let q = form_encode(&[("newClientOrderId", "a&quantity=999"), ("x", "a b")]).unwrap();
        assert_eq!(q, "newClientOrderId=a%26quantity%3D999&x=a%20b");
        assert!(!q.contains("&quantity="));
        assert_eq!(
            form_encode(&[("a=b", "1")]).unwrap_err(),
            SigningError::InvalidParamName
        );
        assert_eq!(
            form_encode(&[("", "1")]).unwrap_err(),
            SigningError::InvalidParamName
        );
    }

    #[test]
    fn symbols_with_separators_or_lowercase_are_rejected() {
        for ok in ["BTCUSDT", "BTC-USDT-SWAP", "1000PEPEUSDT"] {
            assert_eq!(validate_symbol(ok), Ok(ok));
        }
        for bad in [
            "",
            "BTCUSDT&quantity=1",
            "BTC=USDT",
            "btcusdt",
            "BTC USDT",
            "BTCUSDT\n",
            "BTCUSDT_240628",
            "BTC/USDT",
            "ÀBTC",
            &"A".repeat(33),
        ] {
            assert_eq!(
                validate_symbol(bad),
                Err(SigningError::InvalidSymbol),
                "{bad}"
            );
        }
    }

    #[test]
    fn okx_signs_the_serialised_body_string_it_returns() {
        let body = json!({"instId": "BTC-USDT-SWAP", "tdMode": "cross", "side": "buy"});
        let signed = okx_signed_json(
            "test-secret-not-real",
            "2020-12-08T09:08:57.715Z",
            "POST",
            "/api/v5/trade/order",
            &body,
        )
        .unwrap();
        // The returned body is the exact string that was signed.
        assert_eq!(signed.body, serde_json::to_string(&body).unwrap());
        assert_eq!(
            signed.sign,
            okx_sign(
                "test-secret-not-real",
                "2020-12-08T09:08:57.715Z",
                "POST",
                "/api/v5/trade/order",
                &signed.body
            )
        );
        // A different body gives a different signature.
        let other = okx_signed_json(
            "test-secret-not-real",
            "2020-12-08T09:08:57.715Z",
            "POST",
            "/api/v5/trade/order",
            &json!({"instId": "BTC-USDT-SWAP", "tdMode": "cross", "side": "sell"}),
        )
        .unwrap();
        assert_ne!(signed.sign, other.sign);
    }

    #[test]
    fn okx_golden_vector_for_a_post_body() {
        // Computed independently with `openssl dgst -sha256 -hmac ... -binary | base64`.
        let signed = okx_signed_json(
            "test-secret-not-real",
            "2020-12-08T09:08:57.715Z",
            "POST",
            "/api/v5/trade/order",
            &json!({"instId": "BTC-USDT"}),
        )
        .unwrap();
        assert_eq!(signed.body, r#"{"instId":"BTC-USDT"}"#);
        assert_eq!(signed.sign, "R19M44MAMFZOYcwgW80XzGvjjxHOkESoA1aHA125r28=");
    }
}
