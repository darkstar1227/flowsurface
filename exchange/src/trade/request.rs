//! Built requests: what the builders return. They are plain data; nothing here sends.
//!
//! The signed string and the sent bytes come from one function each (`signed`), and the
//! golden tests compare against that output (condition 44).

use crate::signing::{BinanceSigned, OkxSigned, binance_signed_form, okx_signed_json};
use serde::Serialize;
use std::fmt;

use super::decimal::Decimal;
use super::okx::OkxAlgoType;
use super::types::TradeError;

/// What a request does to the account's exposure. Set by the builders, never by the caller:
/// the default for anything that is not a cancel or a read is "adds" (fail safe), so a
/// request can only be sent as a reducing one if the builder said so (condition 71).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exposure {
    /// Opens or grows a position. `notional` is the order value in the quote currency when
    /// the builder could compute it (limit price or reference price known).
    Adds { notional: Option<Decimal> },
    /// Only closes or reduces (reduce-only, close, protective orders).
    Reduces,
    /// Cancels and reads.
    Neutral,
}

/// How to find an order again by its client id (Unknown outcome flow, condition 60).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupKind {
    FuturesOrder,
    FuturesAlgo,
    SpotOrder,
    SpotList,
    OkxOrder,
    OkxAlgo(OkxAlgoType),
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Lookup {
    pub(crate) kind: LookupKind,
    pub(crate) client_id: String,
}

impl std::fmt::Debug for Lookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lookup")
            .field("kind", &self.kind)
            .field("client_id", &"<redacted>")
            .finish()
    }
}

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
#[derive(PartialEq, Eq)]
pub struct BinanceRequest {
    method: Method,
    path: &'static str,
    params: Vec<(&'static str, String)>,
    exposure: Exposure,
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
            exposure: match method {
                Method::Post => Exposure::Adds { notional: None },
                Method::Get | Method::Delete => Exposure::Neutral,
            },
        }
    }

    pub(crate) fn reducing(mut self) -> Self {
        self.exposure = Exposure::Reduces;
        self
    }

    pub(crate) fn adding(mut self, notional: Option<Decimal>) -> Self {
        self.exposure = Exposure::Adds { notional };
        self
    }

    pub fn exposure(&self) -> Exposure {
        self.exposure
    }

    pub(crate) fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn symbol(&self) -> Option<&str> {
        self.param("symbol")
    }

    /// Every client id in the request (entry, legs, list).
    pub fn client_ids(&self) -> Vec<&str> {
        self.params
            .iter()
            .filter(|(n, _)| n.ends_with("ClientOrderId") || *n == "clientAlgoId")
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// Where to look the order up by client id, from the endpoint that created it.
    /// Only a placing request (POST) has something to look up: a cancel (DELETE) carries the
    /// id of the order it cancels, and finding that order says nothing about the cancel (F1).
    /// An unknown cancel is judged by reading the open orders again.
    pub(crate) fn lookup(&self) -> Option<Lookup> {
        if self.method != Method::Post {
            return None;
        }
        let (kind, name) = match self.path {
            "/fapi/v1/order" => (LookupKind::FuturesOrder, "newClientOrderId"),
            "/fapi/v1/algoOrder" => (LookupKind::FuturesAlgo, "clientAlgoId"),
            "/api/v3/order" => (LookupKind::SpotOrder, "newClientOrderId"),
            "/api/v3/orderList/oco" | "/api/v3/orderList/otoco" => {
                (LookupKind::SpotList, "listClientOrderId")
            }
            _ => return None,
        };
        Some(Lookup {
            kind,
            client_id: self.param(name)?.to_string(),
        })
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

    /// Every parameter with its value, in the order they are signed and sent (the preview, and
    /// its hash, are made from this).
    pub(crate) fn preview_fields(&self) -> Vec<(String, String)> {
        self.params
            .iter()
            .map(|(n, v)| ((*n).to_string(), v.clone()))
            .collect()
    }

    /// Parameter names in order (for tests and audit; values are not exposed here).
    pub fn param_names(&self) -> Vec<&'static str> {
        self.params.iter().map(|(n, _)| *n).collect()
    }

    /// Encode, add `recvWindow` and `timestamp`, and sign. `signed.query` is the signed
    /// string; `signed.body` is what is sent (POST body, or after `?` for GET/DELETE).
    pub(crate) fn signed(
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
    ClosePosition(OkxClosePositionBody),
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
#[derive(PartialEq, Eq)]
pub struct OkxRequest {
    method: Method,
    path: String,
    body: Option<OkxBody>,
    exposure: Exposure,
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
        let neutral = method != Method::Post
            || path.ends_with("/cancel-order")
            || path.ends_with("/cancel-algos");
        OkxRequest {
            method,
            path,
            body,
            exposure: if neutral {
                Exposure::Neutral
            } else {
                Exposure::Adds { notional: None }
            },
        }
    }

    pub(crate) fn reducing(mut self) -> Self {
        self.exposure = Exposure::Reduces;
        self
    }

    pub(crate) fn adding(mut self, notional: Option<Decimal>) -> Self {
        self.exposure = Exposure::Adds { notional };
        self
    }

    pub fn exposure(&self) -> Exposure {
        self.exposure
    }

    pub fn symbol(&self) -> Option<&str> {
        match self.body.as_ref()? {
            OkxBody::Order(b) => Some(&b.inst_id),
            OkxBody::Algo(b) => Some(&b.inst_id),
            OkxBody::CancelOrder(b) => Some(&b.inst_id),
            OkxBody::CancelAlgos(v) => v.first().map(|c| c.inst_id.as_str()),
            OkxBody::ClosePosition(b) => Some(&b.inst_id),
        }
    }

    pub fn client_ids(&self) -> Vec<&str> {
        match &self.body {
            Some(OkxBody::Order(b)) => std::iter::once(b.cl_ord_id.as_str())
                .chain(
                    b.attach_algo_ords
                        .iter()
                        .map(|a| a.attach_algo_cl_ord_id.as_str()),
                )
                .collect(),
            Some(OkxBody::Algo(b)) => vec![b.algo_cl_ord_id.as_str()],
            Some(OkxBody::ClosePosition(b)) => b.cl_ord_id.as_deref().into_iter().collect(),
            _ => Vec::new(),
        }
    }

    /// The body of a `order-algo` request.
    pub(crate) fn algo_body(&self) -> Option<&OkxAlgoBody> {
        match self.body.as_ref()? {
            OkxBody::Algo(b) => Some(b),
            _ => None,
        }
    }

    /// The items of a `cancel-algos` request.
    pub(crate) fn cancel_algo_items(&self) -> Option<&[OkxCancelAlgo]> {
        match self.body.as_ref()? {
            OkxBody::CancelAlgos(v) => Some(v),
            _ => None,
        }
    }

    pub(crate) fn lookup(&self) -> Option<Lookup> {
        match self.body.as_ref()? {
            OkxBody::Order(b) => Some(Lookup {
                kind: LookupKind::OkxOrder,
                client_id: b.cl_ord_id.clone(),
            }),
            OkxBody::Algo(b) => {
                let kind = match b.ord_type {
                    "conditional" => OkxAlgoType::Conditional,
                    "oco" => OkxAlgoType::Oco,
                    _ => return None,
                };
                Some(Lookup {
                    kind: LookupKind::OkxAlgo(kind),
                    client_id: b.algo_cl_ord_id.clone(),
                })
            }
            OkxBody::ClosePosition(b) => Some(Lookup {
                kind: LookupKind::OkxOrder,
                client_id: b.cl_ord_id.clone()?,
            }),
            _ => None,
        }
    }

    pub fn method(&self) -> Method {
        self.method
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// The JSON body flattened into name and value pairs (see `preview`).
    pub(crate) fn preview_fields(&self) -> Vec<(String, String)> {
        match &self.body {
            Some(body) => serde_json::to_value(body)
                .map(|v| super::preview::flatten_json(&v))
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// Serialise the body once, sign that exact string, and return both.
    pub(crate) fn signed(&self, secret: &str, timestamp: &str) -> Result<OkxSigned, TradeError> {
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

/// `POST /api/v5/trade/close-position`: a market close of a whole position.
#[derive(Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OkxClosePositionBody {
    pub(crate) inst_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pos_side: Option<&'static str>,
    pub(crate) mgn_mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cl_ord_id: Option<String>,
}
