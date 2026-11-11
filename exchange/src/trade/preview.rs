//! What the user is shown before confirming, read from the request the builder made (condition
//! 69), and the hash that ties a confirmation to exactly that request.
//!
//! [`OrderIntent::preview`](super::OrderIntent::preview) lists the parameters **as they will be
//! sent**: the same strings the signed form or JSON body is made from, after the builder's
//! rounding. The window must show these, not values it rounded itself. Client ids are shown only
//! by their last characters. The signature, timestamp and credentials are not part of the
//! request yet and never appear here.
//!
//! [`ConfirmToken::for_preview`](super::ConfirmToken::for_preview) keeps the preview's hash, and
//! the gate refuses an order whose hash differs (`SubmitError::ConfirmationMismatch`). The hash
//! covers the full request including the client ids, so a token confirms one request only: an
//! order rebuilt with other values, or even only with a new client id, needs a new confirmation.

use super::request::{Exposure, Method};
use super::rules::TradeProduct;
use crate::account::AccountKey;
use ring::digest;
use std::fmt;

/// SHA-256 over the request. Opaque: it only compares.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct PreviewHash([u8; 32]);

impl fmt::Debug for PreviewHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PreviewHash(..)")
    }
}

/// One parameter of the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewField {
    pub name: String,
    /// The value as sent, except a client id, which is `…` and its last four characters.
    pub value: String,
}

/// The request of an [`OrderIntent`](super::OrderIntent) as it will go out. `Clone` so the
/// window can keep it; the hash inside is private, so a copy that is edited still confirms the
/// original request only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderPreview {
    pub account: AccountKey,
    pub product: TradeProduct,
    pub method: Method,
    /// The endpoint path, without a query.
    pub path: String,
    pub symbol: String,
    pub exposure: Exposure,
    /// Binance: in the order sent. OKX: the JSON body flattened (`attachAlgoOrds[0].slTriggerPx`),
    /// sorted by name.
    pub fields: Vec<PreviewField>,
    pub(crate) hash: PreviewHash,
}

impl OrderPreview {
    /// The value of a parameter, by its wire name (for example `quantity`, `sz`, `px`).
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.value.as_str())
    }

    pub(crate) fn build(
        account: AccountKey,
        product: TradeProduct,
        method: Method,
        path: &str,
        symbol: &str,
        exposure: Exposure,
        full: Vec<(String, String)>,
    ) -> Self {
        // The hash covers the full path with its query: two requests that differ only in the
        // query (an OKX GET) must not share a hash. The shown path drops the query.
        let hash = hash_request(account, product, method, path, symbol, &full);
        let path = path.split('?').next().unwrap_or_default().to_string();
        let fields = full
            .into_iter()
            .map(|(name, value)| {
                let value = if is_client_id(&name) {
                    mask(&value)
                } else {
                    value
                };
                PreviewField { name, value }
            })
            .collect();
        OrderPreview {
            account,
            product,
            method,
            path,
            symbol: symbol.to_string(),
            exposure,
            fields,
            hash,
        }
    }
}

fn is_client_id(name: &str) -> bool {
    let last = name.rsplit('.').next().unwrap_or(name);
    last.ends_with("ClientOrderId")
        || last == "clientAlgoId"
        || last.ends_with("ClOrdId")
        || last == "clOrdId"
        || last == "algoClOrdId"
}

fn mask(id: &str) -> String {
    let n = id.chars().count();
    if n <= 4 {
        return "…".to_string();
    }
    let tail: String = id.chars().skip(n - 4).collect();
    format!("…{tail}")
}

fn hash_request(
    account: AccountKey,
    product: TradeProduct,
    method: Method,
    path: &str,
    symbol: &str,
    full: &[(String, String)],
) -> PreviewHash {
    let mut ctx = digest::Context::new(&digest::SHA256);
    // Every part is length-prefixed, so two different requests cannot hash the same bytes.
    let mut part = |bytes: &[u8]| {
        ctx.update(&(bytes.len() as u64).to_be_bytes());
        ctx.update(bytes);
    };
    part(&account.to_bytes());
    part(format!("{product:?}").as_bytes());
    part(method.as_str().as_bytes());
    part(path.as_bytes());
    part(symbol.as_bytes());
    for (name, value) in full {
        part(name.as_bytes());
        part(value.as_bytes());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(ctx.finish().as_ref());
    PreviewHash(out)
}

/// Flatten a JSON body into `(name, value)` pairs: objects with dots, arrays with `[i]`, and
/// no entry for `null`.
pub(crate) fn flatten_json(value: &serde_json::Value) -> Vec<(String, String)> {
    fn walk(prefix: &str, v: &serde_json::Value, out: &mut Vec<(String, String)>) {
        use serde_json::Value;
        match v {
            Value::Null => {}
            Value::Bool(b) => out.push((prefix.to_string(), b.to_string())),
            Value::Number(n) => out.push((prefix.to_string(), n.to_string())),
            Value::String(s) => out.push((prefix.to_string(), s.clone())),
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    walk(&format!("{prefix}[{i}]"), item, out);
                }
            }
            Value::Object(map) => {
                for (k, item) in map {
                    let name = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    walk(&name, item, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk("", value, &mut out);
    out
}
