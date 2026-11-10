//! Built requests: what the builders return. They are plain data; nothing here sends.
//!
//! The signed string and the sent bytes come from one function each (`signed`), and the
//! golden tests compare against that output (condition 44).

use crate::signing::{BinanceSigned, OkxSigned, binance_signed_form, okx_signed_json};
use serde::Serialize;
use std::fmt;

use super::types::TradeError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Delete => "DELETE",
        }
    }
}

/// A Binance request with its parameters in the fixed order the builder chose.
/// `timestamp` and `recvWindow` are added last, at signing time.
#[derive(Clone, PartialEq, Eq)]
pub struct BinanceRequest {
    method: Method,
    path: &'static str,
    params: Vec<(&'static str, String)>,
}

// Masked: the parameters are the order.
impl fmt::Debug for BinanceRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "BinanceRequest({} {} <redacted>)",
            self.method.as_str(),
            self.path
        )
    }
}

impl BinanceRequest {
    pub(crate) fn new(method: Method, path: &'static str) -> Self {
        BinanceRequest {
            method,
            path,
            params: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, name: &'static str, value: impl Into<String>) {
        self.params.push((name, value.into()));
    }

    pub(crate) fn push_opt(&mut self, name: &'static str, value: Option<String>) {
        if let Some(v) = value {
            self.params.push((name, v));
        }
    }

    pub fn method(&self) -> Method {
        self.method
    }

    pub fn path(&self) -> &'static str {
        self.path
    }

    /// Parameter names in order (for tests and audit; values are not exposed here).
    pub fn param_names(&self) -> Vec<&'static str> {
        self.params.iter().map(|(n, _)| *n).collect()
    }

    /// Encode, add `recvWindow` and `timestamp`, and sign. `signed.query` is the signed
    /// string; `signed.body` is what is sent (POST body, or after `?` for GET/DELETE).
    pub fn signed(
        &self,
        secret: &str,
        recv_window_ms: u32,
        timestamp_ms: i64,
    ) -> Result<BinanceSigned, TradeError> {
        let recv = recv_window_ms.to_string();
        let ts = timestamp_ms.to_string();
        let mut params: Vec<(&str, &str)> =
            self.params.iter().map(|(n, v)| (*n, v.as_str())).collect();
        params.push(("recvWindow", &recv));
        params.push(("timestamp", &ts));
        Ok(binance_signed_form(secret, &params)?)
    }
}

/// A JSON body for an OKX POST. Untagged, so the body is exactly the inner struct; the
/// field order is the struct's declaration order, which is what gets signed.
#[derive(Clone, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum OkxBody {
    Order(OkxOrderBody),
    Algo(OkxAlgoBody),
    CancelOrder(OkxCancelOrderBody),
    CancelAlgos(Vec<OkxCancelAlgo>),
}

/// An OKX request. For GET the query is part of `path` and there is no body, which is
/// exactly what OKX signs.
///
/// The fields are private and only the builders in this module tree can construct one, so
/// no code outside the crate can skip the symbol, rounding, mode and client id checks
/// (condition 44):
///
/// ```compile_fail,E0451
/// use flowsurface_exchange::trade::{Method, OkxRequest};
/// let _ = OkxRequest {
///     method: Method::Post,
///     path: "/api/v5/trade/order".to_string(),
///     body: None,
/// };
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct OkxRequest {
    method: Method,
    path: String,
    body: Option<OkxBody>,
}

impl fmt::Debug for OkxRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Only the path before `?`: the query holds order ids.
        let path = self.path.split('?').next().unwrap_or_default();
        write!(
            f,
            "OkxRequest({} {} <redacted>)",
            self.method.as_str(),
            path
        )
    }
}

impl OkxRequest {
    pub(crate) fn new(method: Method, path: String, body: Option<OkxBody>) -> Self {
        OkxRequest { method, path, body }
    }

    pub fn method(&self) -> Method {
        self.method
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Serialise the body once, sign that exact string, and return both.
    pub fn signed(&self, secret: &str, timestamp: &str) -> Result<OkxSigned, TradeError> {
        match &self.body {
            Some(body) => {
                Ok(
                    okx_signed_json(secret, timestamp, self.method.as_str(), &self.path, body)
                        .map_err(|_| TradeError::Unsupported)?,
                )
            }
            None => {
                let sign = crate::account::okx_sign(
                    secret,
                    timestamp,
                    self.method.as_str(),
                    &self.path,
                    "",
                );
                Ok(OkxSigned {
                    body: String::new(),
                    sign,
                })
            }
        }
    }
}

#[derive(Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OkxOrderBody {
    pub(crate) inst_id: String,
    pub(crate) td_mode: &'static str,
    pub(crate) side: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pos_side: Option<&'static str>,
    pub(crate) ord_type: &'static str,
    pub(crate) sz: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reduce_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tgt_ccy: Option<&'static str>,
    pub(crate) cl_ord_id: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) attach_algo_ords: Vec<OkxAttachAlgo>,
}

#[derive(Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OkxAttachAlgo {
    pub(crate) attach_algo_cl_ord_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tp_trigger_px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tp_ord_px: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tp_trigger_px_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sl_trigger_px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sl_ord_px: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sl_trigger_px_type: Option<&'static str>,
}

#[derive(Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OkxAlgoBody {
    pub(crate) inst_id: String,
    pub(crate) td_mode: &'static str,
    pub(crate) side: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pos_side: Option<&'static str>,
    pub(crate) ord_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sz: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) close_fraction: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reduce_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tp_trigger_px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tp_ord_px: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tp_trigger_px_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sl_trigger_px: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sl_ord_px: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sl_trigger_px_type: Option<&'static str>,
    pub(crate) algo_cl_ord_id: String,
}

#[derive(Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OkxCancelOrderBody {
    pub(crate) inst_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ord_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cl_ord_id: Option<String>,
}

#[derive(Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OkxCancelAlgo {
    pub(crate) inst_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) algo_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) algo_cl_ord_id: Option<String>,
}
