//! The three-state result of a send (conditions 39, 76, 77): `Accepted`, `Rejected` or
//! `Unknown`, plus `NotSent` for requests that never left this process.
//!
//! `Unknown` is never shown as a failure and the request is never resent automatically; the
//! Unknown flow (`unknown.rs`) looks the order up by client id instead.

use crate::account::{AccountError, Delivery};
use serde_json::Value;
use std::fmt;

/// Longest exchange `msg` kept (characters, not bytes; never `String::truncate`).
const MAX_MSG_CHARS: usize = 120;
/// Longest exchange error code kept.
const MAX_CODE_CHARS: usize = 16;

/// OKX codes after which the execution status is unknown (condition 39).
const OKX_UNKNOWN_CODES: [&str; 2] = ["50004", "51149"];
/// OKX codes for a timestamp the server refused before processing the request.
const OKX_CLOCK_CODES: [&str; 2] = ["50102", "50112"];
/// Binance codes with an unknown execution status.
const BINANCE_UNKNOWN_CODES: [&str; 2] = ["-1006", "-1007"];
const BINANCE_CLOCK_CODE: &str = "-1021";
/// OKX: the client order id is already used (docs copy: "Client order ID already exists").
pub const OKX_DUPLICATE_CLIENT_ID: &str = "51016";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectKind {
    /// The exchange refused the request (an error code or `sCode`).
    Exchange,
    /// The timestamp was outside the window; nothing was processed. Not resent
    /// (condition 76): fix the clock, then send again by hand with a new signature.
    ClockSkew,
    /// HTTP 429 or 418: the request was not executed.
    RateLimited(u16),
    /// HTTP 403: Binance WAF block (docs: "a WAF rule has been violated").
    Blocked,
}

/// One refused item of a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemRejection {
    pub code: String,
    pub msg: String,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The exchange took the request; `ids` are the exchange-assigned ids.
    Accepted { ids: Vec<String> },
    Rejected {
        kind: RejectKind,
        code: String,
        /// For the UI only. Never logged.
        msg: String,
    },
    /// The request may or may not have been executed. `http_status` is for display.
    Unknown {
        client_id: Option<String>,
        http_status: Option<u16>,
    },
    /// A batch where some items were accepted and some refused (OKX `cancel-algos`).
    Partial {
        accepted: Vec<String>,
        rejected: Vec<ItemRejection>,
    },
    /// The request never left this process.
    NotSent(AccountError),
}

/// `Debug` hides the client id of an `Unknown` outcome, so that a `{:?}` of a `Submitted`, a
/// `ReplaceError` or a `ReplaceDone` cannot put it into a log. Everything else prints as the
/// derive would.
impl fmt::Debug for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Accepted { ids } => f.debug_struct("Accepted").field("ids", ids).finish(),
            Outcome::Rejected { kind, code, msg } => f
                .debug_struct("Rejected")
                .field("kind", kind)
                .field("code", code)
                .field("msg", msg)
                .finish(),
            Outcome::Unknown { http_status, .. } => f
                .debug_struct("Unknown")
                .field("client_id", &"<redacted>")
                .field("http_status", http_status)
                .finish(),
            Outcome::Partial { accepted, rejected } => f
                .debug_struct("Partial")
                .field("accepted", accepted)
                .field("rejected", rejected)
                .finish(),
            Outcome::NotSent(e) => f.debug_tuple("NotSent").field(e).finish(),
        }
    }
}

/// `[-0-9A-Za-z]`, at most 16 characters; anything else is replaced (condition 77).
pub(crate) fn clean_code(code: &str) -> String {
    let ok = !code.is_empty()
        && code.chars().count() <= MAX_CODE_CHARS
        && code.chars().all(|c| c == '-' || c.is_ascii_alphanumeric());
    if ok {
        code.to_string()
    } else {
        "invalid".to_string()
    }
}

pub(crate) fn clean_msg(msg: &str) -> String {
    msg.chars()
        .filter(|c| !c.is_control())
        .take(MAX_MSG_CHARS)
        .collect()
}

fn id_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Map a transport-level failure.
fn from_error(e: AccountError, status: Option<u16>, client_id: Option<&str>) -> Outcome {
    let unknown = || Outcome::Unknown {
        client_id: client_id.map(str::to_string),
        http_status: status,
    };
    match e.delivery() {
        Delivery::NotSent => Outcome::NotSent(e),
        Delivery::Unknown => match e {
            AccountError::Http(s) => Outcome::Unknown {
                client_id: client_id.map(str::to_string),
                http_status: Some(s),
            },
            _ => unknown(),
        },
        Delivery::Rejected => match e {
            AccountError::ClockSkew => Outcome::Rejected {
                kind: RejectKind::ClockSkew,
                code: "ClockSkew".to_string(),
                msg: String::new(),
            },
            AccountError::RateLimited(s) => Outcome::Rejected {
                kind: RejectKind::RateLimited(s),
                code: s.to_string(),
                msg: String::new(),
            },
            AccountError::Http(s) => Outcome::Rejected {
                kind: RejectKind::Blocked,
                code: s.to_string(),
                msg: String::new(),
            },
            AccountError::Exchange { code, msg } => Outcome::Rejected {
                kind: RejectKind::Exchange,
                code: clean_code(&code),
                msg: clean_msg(&msg),
            },
            _ => unknown(),
        },
    }
}

/// Binance: a 2xx with an order id (`orderId`, `orderListId` or `algoId`) is accepted; a 2xx
/// without one is `Unknown` (condition 39). Errors map through [`AccountError::delivery`].
pub fn from_binance(
    result: Result<Value, AccountError>,
    status: Option<u16>,
    client_id: Option<&str>,
) -> Outcome {
    match result {
        Err(e) => from_error(e, status, client_id),
        Ok(v) => {
            let ids: Vec<String> = ["orderId", "orderListId", "algoId"]
                .iter()
                .filter_map(|k| v.get(*k).and_then(id_string))
                .collect();
            if ids.is_empty() {
                Outcome::Unknown {
                    client_id: client_id.map(str::to_string),
                    http_status: status,
                }
            } else {
                Outcome::Accepted { ids }
            }
        }
    }
}

/// Binance bulk cancel (`allOpenOrders`, `openOrders`): `{"code":200,...}` or a list. Any
/// parsable 2xx counts as accepted; the caller re-queries to confirm.
pub fn from_binance_bulk(result: Result<Value, AccountError>, status: Option<u16>) -> Outcome {
    match result {
        Err(e) => from_error(e, status, None),
        Ok(_) => Outcome::Accepted { ids: Vec::new() },
    }
}

/// OKX: the top-level `code` and every `data[].sCode` are judged separately (condition 77).
/// `require_id` is true for placing orders: a success without an id is `Unknown`.
pub fn from_okx(
    result: Result<Value, AccountError>,
    status: Option<u16>,
    client_id: Option<&str>,
    require_id: bool,
) -> Outcome {
    let v = match result {
        Err(e) => return from_error(e, status, client_id),
        Ok(v) => v,
    };
    let unknown = || Outcome::Unknown {
        client_id: client_id.map(str::to_string),
        http_status: status,
    };
    let Some(code) = v.get("code").and_then(Value::as_str) else {
        return unknown();
    };
    let data = v.get("data").and_then(Value::as_array);
    let items = data.map(Vec::as_slice).unwrap_or(&[]);

    // Top-level failure with no per-item detail.
    if !matches!(code, "0" | "1" | "2") || (code != "0" && items.is_empty()) {
        return okx_top_level(
            code,
            v.get("msg").and_then(Value::as_str).unwrap_or(""),
            &unknown,
        );
    }
    if items.is_empty() {
        // code "0" with no data: the request may have been executed.
        return unknown();
    }

    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    for item in items {
        let s_code = item.get("sCode").and_then(Value::as_str).unwrap_or("");
        match s_code {
            "0" => {
                let id = ["ordId", "algoId", "clOrdId", "algoClOrdId"]
                    .iter()
                    .find_map(|k| item.get(*k).and_then(id_string));
                match id {
                    Some(id) => accepted.push(id),
                    None if require_id => return unknown(),
                    None => accepted.push(String::new()),
                }
            }
            "" => return unknown(), // an item without a verdict
            c if OKX_UNKNOWN_CODES.contains(&c) => return unknown(),
            c if OKX_CLOCK_CODES.contains(&c) => {
                return Outcome::Rejected {
                    kind: RejectKind::ClockSkew,
                    code: clean_code(c),
                    msg: clean_msg(item.get("sMsg").and_then(Value::as_str).unwrap_or("")),
                };
            }
            c => rejected.push(ItemRejection {
                code: clean_code(c),
                msg: clean_msg(item.get("sMsg").and_then(Value::as_str).unwrap_or("")),
            }),
        }
    }
    match (accepted.is_empty(), rejected.is_empty()) {
        (false, true) => Outcome::Accepted { ids: accepted },
        (true, false) if rejected.len() == 1 => {
            let r = rejected.remove(0);
            Outcome::Rejected {
                kind: RejectKind::Exchange,
                code: r.code,
                msg: r.msg,
            }
        }
        (true, false) => Outcome::Partial { accepted, rejected },
        _ => Outcome::Partial { accepted, rejected },
    }
}

fn okx_top_level(code: &str, msg: &str, unknown: &dyn Fn() -> Outcome) -> Outcome {
    if OKX_UNKNOWN_CODES.contains(&code) {
        return unknown();
    }
    let kind = if OKX_CLOCK_CODES.contains(&code) {
        RejectKind::ClockSkew
    } else {
        RejectKind::Exchange
    };
    Outcome::Rejected {
        kind,
        code: clean_code(code),
        msg: clean_msg(msg),
    }
}

/// Binance error-body code check for a 200-level reply is not needed: Binance answers errors
/// with 4xx/5xx, which `AccountError::delivery` already maps (`-1006`/`-1007` → Unknown).
pub fn binance_code_is_unknown(code: &str) -> bool {
    BINANCE_UNKNOWN_CODES.contains(&code)
}

pub fn binance_code_is_clock_skew(code: &str) -> bool {
    code == BINANCE_CLOCK_CODE
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn unknown(client: &str, status: Option<u16>) -> Outcome {
        Outcome::Unknown {
            client_id: Some(client.to_string()),
            http_status: status,
        }
    }

    fn ex(code: &str) -> AccountError {
        AccountError::Exchange {
            code: code.to_string(),
            msg: "m".to_string(),
        }
    }

    #[test]
    fn binance_three_state_table() {
        let c = Some("cid");
        // (result, status, expected)
        let rows: Vec<(Result<Value, AccountError>, Option<u16>, Outcome)> = vec![
            (
                Ok(json!({"orderId": 12345, "status": "NEW"})),
                Some(200),
                Outcome::Accepted {
                    ids: vec!["12345".into()],
                },
            ),
            (
                Ok(json!({"algoId": 7})),
                Some(200),
                Outcome::Accepted {
                    ids: vec!["7".into()],
                },
            ),
            // a 2xx without an id
            (Ok(json!({})), Some(200), unknown("cid", Some(200))),
            (Err(AccountError::Timeout), None, unknown("cid", None)),
            (Err(AccountError::Network), None, unknown("cid", None)),
            (
                Err(AccountError::BadResponse),
                Some(200),
                unknown("cid", Some(200)),
            ),
            (
                Err(AccountError::Http(503)),
                Some(503),
                unknown("cid", Some(503)),
            ),
            (Err(ex("-1006")), Some(500), unknown("cid", Some(500))),
            (Err(ex("-1007")), Some(504), unknown("cid", Some(504))),
            (
                Err(ex("-2010")),
                Some(400),
                Outcome::Rejected {
                    kind: RejectKind::Exchange,
                    code: "-2010".into(),
                    msg: "m".into(),
                },
            ),
            (
                Err(AccountError::ClockSkew),
                Some(400),
                Outcome::Rejected {
                    kind: RejectKind::ClockSkew,
                    code: "ClockSkew".into(),
                    msg: String::new(),
                },
            ),
            (
                Err(AccountError::RateLimited(429)),
                Some(429),
                Outcome::Rejected {
                    kind: RejectKind::RateLimited(429),
                    code: "429".into(),
                    msg: String::new(),
                },
            ),
            (
                Err(AccountError::Http(403)),
                Some(403),
                Outcome::Rejected {
                    kind: RejectKind::Blocked,
                    code: "403".into(),
                    msg: String::new(),
                },
            ),
            (
                Err(AccountError::LocalRateLimited),
                None,
                Outcome::NotSent(AccountError::LocalRateLimited),
            ),
            (
                Err(AccountError::Connect),
                None,
                Outcome::NotSent(AccountError::Connect),
            ),
        ];
        for (n, (result, status, want)) in rows.into_iter().enumerate() {
            assert_eq!(from_binance(result, status, c), want, "row {n}");
        }
    }

    #[test]
    fn okx_top_level_and_per_item_codes_are_judged_separately() {
        let ok =
            json!({"code":"0","msg":"","data":[{"ordId":"9","clOrdId":"c","sCode":"0","sMsg":""}]});
        assert_eq!(
            from_okx(Ok(ok), Some(200), Some("c"), true),
            Outcome::Accepted {
                ids: vec!["9".into()]
            }
        );
        // HTTP 200, code 0 at the top, but the item failed: Rejected, not Accepted
        let item_fail = json!({"code":"1","msg":"All operations failed","data":[{"sCode":"51008","sMsg":"insufficient"}]});
        assert_eq!(
            from_okx(Ok(item_fail), Some(200), Some("c"), true),
            Outcome::Rejected {
                kind: RejectKind::Exchange,
                code: "51008".into(),
                msg: "insufficient".into()
            }
        );
        // code "0" with empty data -> Unknown
        let empty = json!({"code":"0","msg":"","data":[]});
        assert_eq!(
            from_okx(Ok(empty), Some(200), Some("c"), true),
            unknown("c", Some(200))
        );
        // a success without an id on a placement -> Unknown
        let no_id = json!({"code":"0","data":[{"sCode":"0"}]});
        assert_eq!(
            from_okx(Ok(no_id.clone()), Some(200), Some("c"), true),
            unknown("c", Some(200))
        );
        assert!(matches!(
            from_okx(Ok(no_id), Some(200), None, false),
            Outcome::Accepted { .. }
        ));
        // 50004 and 51149, top level or per item -> Unknown
        for body in [
            json!({"code":"50004","msg":"timeout","data":[]}),
            json!({"code":"1","data":[{"sCode":"51149","sMsg":"timeout"}]}),
            json!({"code":"1","data":[{"sCode":"50004","sMsg":"x"}]}),
        ] {
            assert_eq!(
                from_okx(Ok(body), Some(200), Some("c"), true),
                unknown("c", Some(200))
            );
        }
        // clock skew, top level or per item -> Rejected(ClockSkew)
        for (body, code) in [
            (
                json!({"code":"50102","msg":"Timestamp request expired","data":[]}),
                "50102",
            ),
            (json!({"code":"50112","msg":"x","data":[]}), "50112"),
            (
                json!({"code":"1","data":[{"sCode":"50102","sMsg":"x"}]}),
                "50102",
            ),
        ] {
            match from_okx(Ok(body), Some(200), Some("c"), true) {
                Outcome::Rejected {
                    kind, code: got, ..
                } => {
                    assert_eq!(kind, RejectKind::ClockSkew);
                    assert_eq!(got, code);
                }
                other => panic!("{other:?}"),
            }
        }
        // an unknown top-level code is a refusal
        assert!(matches!(
            from_okx(
                Ok(json!({"code":"50011","msg":"rate","data":[]})),
                Some(200),
                None,
                true
            ),
            Outcome::Rejected {
                kind: RejectKind::Exchange,
                ..
            }
        ));
        // no `code` at all -> Unknown
        assert_eq!(
            from_okx(Ok(json!({"data":[]})), Some(200), Some("c"), true),
            unknown("c", Some(200))
        );
    }

    #[test]
    fn okx_batch_mixed_result_is_partial() {
        let body = json!({"code":"1","data":[
            {"algoId":"1","sCode":"0"},
            {"algoId":"2","sCode":"51400","sMsg":"gone"}]});
        match from_okx(Ok(body), Some(200), None, false) {
            Outcome::Partial { accepted, rejected } => {
                assert_eq!(accepted, vec!["1"]);
                assert_eq!(rejected[0].code, "51400");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn codes_and_messages_are_sanitised() {
        assert_eq!(clean_code("-1021"), "-1021");
        assert_eq!(clean_code("a b"), "invalid");
        assert_eq!(clean_code("12345678901234567"), "invalid");
        assert_eq!(clean_code(""), "invalid");
        // characters, not bytes: no panic in the middle of a multi-byte character
        let long = "字".repeat(500);
        assert_eq!(clean_msg(&long).chars().count(), MAX_MSG_CHARS);
        assert_eq!(clean_msg("a\nb\u{7}c"), "abc");
    }

    #[test]
    fn duplicate_client_id_code_is_the_documented_one() {
        assert_eq!(OKX_DUPLICATE_CLIENT_ID, "51016");
        assert!(binance_code_is_unknown("-1006") && binance_code_is_unknown("-1007"));
        assert!(binance_code_is_clock_skew("-1021"));
    }
}
