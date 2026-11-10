//! Binance private streams: stream URLs, the spot subscribe frame, and the event parsers.
//!
//! - USDⓈ-M: a listenKey (REST, see `TradeClient::listen_key_*`) names the stream; events are
//!   `ORDER_TRADE_UPDATE`, `ALGO_UPDATE`, `ACCOUNT_UPDATE` and `listenKeyExpired`.
//! - Spot: the WebSocket API method `userDataStream.subscribe.signature` (HMAC), condition 48.
//!   Not verified against the testnet, so [`SPOT_PRIVATE_STREAM_VERIFIED`] keeps it off.
//!
//! The event field names follow the official docs and SDK models; the exact payloads still
//! need one run on the testnet (see the report).

use super::events::{
    AccountReason, BalanceUpdate, Fill, OrderCategory, OrderStatus, OrderUpdate, ParseError,
    PositionSide, PositionUpdate, PrivateEvent, ResyncReason, decimal_opt, decimal_req, decode,
    id_opt, int_opt, loggable_name, side_of, str_of, symbol_req,
};
use super::secret::ListenKey;
use crate::account::{
    AccountEnv, AccountError, AccountProduct, AccountVenue, binance_sign, wss_endpoint,
};
use serde_json::Value;

/// The one switch for the spot private stream (condition 48, user decision 3). It stays
/// `false` until `userDataStream.subscribe.signature` has been run on the spot testnet with an
/// HMAC key and worked. While `false`, the spot stream is `Unavailable` and nothing is
/// connected, so spot trading stays off. Flip it only together with the testnet record.
pub const SPOT_PRIVATE_STREAM_VERIFIED: bool = false;

/// Path of the USDⓈ-M private stream. Derived from the official SDK (`private` + `/stream`),
/// not read from an official page, and not confirmed on the testnet: verify before use.
const FUTURES_PRIVATE_PATH: &str = "/private/stream";
const SPOT_WS_API_PATH: &str = "/ws-api/v3";
/// `recvWindow` of the spot subscribe request (condition 48: at most 5000).
pub const SPOT_RECV_WINDOW_MS: u32 = 5000;

/// The USDⓈ-M private stream URL. It contains the listenKey, so it must not be logged; log the
/// host only.
pub(crate) fn futures_stream_url(
    env: AccountEnv,
    key: &ListenKey,
) -> Result<reqwest::Url, AccountError> {
    let host = match env {
        AccountEnv::Live => "fstream.binance.com",
        AccountEnv::Testnet => "fstream.binancefuture.com",
    };
    wss_endpoint(
        AccountVenue::Binance,
        env,
        AccountProduct::UsdM,
        &format!(
            "wss://{host}{FUTURES_PRIVATE_PATH}?streams={}",
            key.expose()
        ),
    )
}

/// The spot WebSocket API URL (no secret in it).
pub(crate) fn spot_ws_api_url(env: AccountEnv) -> Result<reqwest::Url, AccountError> {
    let host = match env {
        AccountEnv::Live => "ws-api.binance.com",
        AccountEnv::Testnet => "ws-api.testnet.binance.vision",
    };
    wss_endpoint(
        AccountVenue::Binance,
        env,
        AccountProduct::Spot,
        &format!("wss://{host}{SPOT_WS_API_PATH}"),
    )
}

/// The `userDataStream.subscribe.signature` frame. The signed string is the parameters
/// (without `signature`) in alphabetical order, `name=value` joined by `&`; the frame carries
/// the same values. The frame holds the api key and signature: never log it.
pub(crate) fn spot_subscribe_frame(
    id: &str,
    api_key: &str,
    secret: &str,
    timestamp_ms: i64,
) -> String {
    let payload = spot_sign_payload(api_key, timestamp_ms);
    let signature = binance_sign(secret, &payload);
    serde_json::json!({
        "id": id,
        "method": "userDataStream.subscribe.signature",
        "params": {
            "apiKey": api_key,
            "recvWindow": SPOT_RECV_WINDOW_MS,
            "timestamp": timestamp_ms,
            "signature": signature,
        }
    })
    .to_string()
}

/// Parameters sorted by name: `apiKey`, `recvWindow`, `timestamp`.
fn spot_sign_payload(api_key: &str, timestamp_ms: i64) -> String {
    format!("apiKey={api_key}&recvWindow={SPOT_RECV_WINDOW_MS}&timestamp={timestamp_ms}")
}

// ── USDⓈ-M events ───────────────────────────────────────────────────────

/// Parse one USDⓈ-M private frame (raw, or inside the combined-stream envelope). An event this
/// build does not know is ignored: only its type name is logged.
pub fn parse_futures(frame: &[u8]) -> Result<Vec<PrivateEvent>, ParseError> {
    let root = decode(frame)?;
    // Combined streams wrap the event as `{"stream": <name>, "data": {...}}`; the stream name
    // is the listenKey, so it is dropped here and never looked at.
    let v = match root.get("data") {
        Some(d) if root.get("stream").is_some() => d,
        _ => &root,
    };
    let Some(kind) = str_of(v, "e") else {
        return Ok(Vec::new());
    };
    match kind {
        "ORDER_TRADE_UPDATE" => futures_order(v.get("o").ok_or(ParseError::Shape)?, v),
        "ALGO_UPDATE" => futures_algo(v.get("o").ok_or(ParseError::Shape)?, v),
        "ACCOUNT_UPDATE" => futures_account(v.get("a").ok_or(ParseError::Shape)?),
        "listenKeyExpired" => Ok(vec![PrivateEvent::ListenKeyExpired]),
        other => {
            log::debug!(
                "binance private stream: ignoring event type {}",
                loggable_name(other)
            );
            Ok(Vec::new())
        }
    }
}

fn futures_status(raw: &str) -> Option<OrderStatus> {
    Some(match raw {
        "NEW" => OrderStatus::New,
        "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
        "FILLED" => OrderStatus::Filled,
        "CANCELED" => OrderStatus::Canceled,
        "EXPIRED" | "EXPIRED_IN_MATCH" => OrderStatus::Expired,
        "REJECTED" => OrderStatus::Rejected,
        _ => return None,
    })
}

fn algo_status(raw: &str) -> Option<OrderStatus> {
    Some(match raw {
        "NEW" => OrderStatus::New,
        "TRIGGERING" | "TRIGGERED" => OrderStatus::Triggered,
        "FINISHED" => OrderStatus::Finished,
        "CANCELED" => OrderStatus::Canceled,
        "EXPIRED" => OrderStatus::Expired,
        "REJECTED" => OrderStatus::Rejected,
        _ => return None,
    })
}

fn position_side(raw: Option<&str>) -> Option<PositionSide> {
    match raw? {
        "BOTH" => Some(PositionSide::Both),
        "LONG" => Some(PositionSide::Long),
        "SHORT" => Some(PositionSide::Short),
        _ => None,
    }
}

fn futures_order(o: &Value, envelope: &Value) -> Result<Vec<PrivateEvent>, ParseError> {
    let symbol = symbol_req(o, "s")?;
    let raw_status = str_of(o, "X").ok_or(ParseError::Shape)?;
    let Some(status) = futures_status(raw_status) else {
        // Never guess what a new status means: read the orders again.
        return Ok(vec![PrivateEvent::ResyncRequired(
            ResyncReason::UnknownOrderStatus,
        )]);
    };
    let client_id = id_opt(o, "c");
    let order_id = id_opt(o, "i");
    let side = side_of(str_of(o, "S"));
    let time_ms = int_opt(o, "T").or_else(|| int_opt(envelope, "E"));
    let mut out = vec![PrivateEvent::Order(OrderUpdate {
        symbol: symbol.clone(),
        client_id: client_id.clone(),
        order_id: order_id.clone(),
        category: OrderCategory::Plain,
        side,
        status,
        price: decimal_opt(o, "p")?,
        qty: decimal_opt(o, "q")?,
        filled_qty: decimal_opt(o, "z")?,
        reduce_only: o.get("R").and_then(Value::as_bool),
        position_side: position_side(str_of(o, "ps")),
        time_ms,
    })];
    if str_of(o, "x") == Some("TRADE") {
        let qty = decimal_req(o, "l")?;
        if !qty.is_zero() {
            out.push(PrivateEvent::Fill(Fill {
                symbol,
                client_id,
                order_id,
                trade_id: id_opt(o, "t"),
                side,
                qty,
                price: decimal_req(o, "L")?,
                fee: decimal_opt(o, "n")?,
                fee_asset: str_of(o, "N").map(str::to_string).filter(|a| a.len() <= 16),
                realized_pnl: decimal_opt(o, "rp")?,
                time_ms,
            }));
        }
    }
    Ok(out)
}

fn futures_algo(o: &Value, envelope: &Value) -> Result<Vec<PrivateEvent>, ParseError> {
    let symbol = symbol_req(o, "s")?;
    let raw_status = str_of(o, "X").ok_or(ParseError::Shape)?;
    let Some(status) = algo_status(raw_status) else {
        return Ok(vec![PrivateEvent::ResyncRequired(
            ResyncReason::UnknownOrderStatus,
        )]);
    };
    Ok(vec![PrivateEvent::Order(OrderUpdate {
        symbol,
        client_id: id_opt(o, "caid"),
        order_id: id_opt(o, "aid"),
        category: OrderCategory::Algo,
        side: side_of(str_of(o, "S")),
        status,
        price: decimal_opt(o, "p")?,
        qty: decimal_opt(o, "q")?,
        filled_qty: decimal_opt(o, "aq")?,
        reduce_only: o.get("R").and_then(Value::as_bool),
        position_side: position_side(str_of(o, "ps")),
        time_ms: int_opt(envelope, "T").or_else(|| int_opt(envelope, "E")),
    })])
}

fn account_reason(raw: Option<&str>) -> AccountReason {
    match raw {
        Some("ORDER") => AccountReason::Order,
        Some("FUNDING_FEE") => AccountReason::FundingFee,
        Some("DEPOSIT" | "ADMIN_DEPOSIT" | "COIN_SWAP_DEPOSIT") => AccountReason::Deposit,
        Some("WITHDRAW" | "ADMIN_WITHDRAW" | "COIN_SWAP_WITHDRAW") => AccountReason::Withdraw,
        Some("MARGIN_TRANSFER" | "ASSET_TRANSFER" | "WITHDRAW_REJECT") => AccountReason::Transfer,
        _ => AccountReason::Other,
    }
}

fn futures_account(a: &Value) -> Result<Vec<PrivateEvent>, ParseError> {
    let mut out = vec![PrivateEvent::Account(account_reason(str_of(a, "m")))];
    let mut skipped = false;
    for b in a.get("B").and_then(Value::as_array).into_iter().flatten() {
        match futures_balance(b) {
            Ok(u) => out.push(PrivateEvent::Balance(u)),
            Err(e) => skipped = skip_item("balance", &e),
        }
    }
    for p in a.get("P").and_then(Value::as_array).into_iter().flatten() {
        match futures_position(p) {
            Ok(u) => out.push(PrivateEvent::Position(u)),
            Err(e) => skipped = skip_item("position", &e),
        }
    }
    if skipped {
        out.push(PrivateEvent::ResyncRequired(ResyncReason::MalformedFrame));
    }
    Ok(out)
}

/// An item that cannot be read is skipped and only its kind is logged; the caller then asks
/// for one REST read (the machine rate-limits it). Always `true`.
fn skip_item(kind: &str, e: &ParseError) -> bool {
    log::warn!("binance private stream: skipped an unreadable {kind} item ({e})");
    true
}

fn futures_balance(b: &Value) -> Result<BalanceUpdate, ParseError> {
    let asset = str_of(b, "a").ok_or(ParseError::Shape)?;
    if asset.is_empty() || asset.len() > 16 {
        return Err(ParseError::Shape);
    }
    Ok(BalanceUpdate {
        asset: asset.to_string(),
        free: None,
        total: decimal_opt(b, "wb")?,
    })
}

fn futures_position(p: &Value) -> Result<PositionUpdate, ParseError> {
    Ok(PositionUpdate {
        symbol: symbol_req(p, "s")?,
        position_side: position_side(str_of(p, "ps")).ok_or(ParseError::Shape)?,
        qty: decimal_req(p, "pa")?,
        entry_price: decimal_opt(p, "ep")?,
    })
}

// ── Spot WebSocket API ──────────────────────────────────────────────────

/// What a spot WebSocket API frame is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpotMessage {
    /// The subscribe request was accepted.
    Subscribed,
    /// A request was refused. `status` is the HTTP-like status, `code` the exchange code.
    Refused {
        status: u16,
        code: String,
    },
    Events(Vec<PrivateEvent>),
}

impl SpotMessage {
    /// A refusal that means the key or signature is wrong (do not reconnect), as opposed to
    /// a clock or rate problem (retry after a pause).
    pub fn is_auth_failure(&self) -> bool {
        match self {
            SpotMessage::Refused { status, code } => {
                matches!(status, 401 | 403)
                    || matches!(code.as_str(), "-1022" | "-2014" | "-2015" | "-1002")
            }
            _ => false,
        }
    }
}

pub fn parse_spot(frame: &[u8]) -> Result<SpotMessage, ParseError> {
    let root = decode(frame)?;
    if let Some(status) = root.get("status").and_then(Value::as_u64) {
        let status = u16::try_from(status).map_err(|_| ParseError::Shape)?;
        if status == 200 {
            return Ok(SpotMessage::Subscribed);
        }
        let code = root
            .get("error")
            .and_then(|e| e.get("code"))
            .map(|c| match c {
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                _ => String::new(),
            })
            .map(|c| super::events::clean_code(&c))
            .unwrap_or_else(|| "?".to_string());
        return Ok(SpotMessage::Refused { status, code });
    }
    let v = root.get("event").unwrap_or(&root);
    let Some(kind) = str_of(v, "e") else {
        return Ok(SpotMessage::Events(Vec::new()));
    };
    let events = match kind {
        "executionReport" => spot_order(v)?,
        "outboundAccountPosition" => {
            let mut out = Vec::new();
            let mut skipped = false;
            for b in v.get("B").and_then(Value::as_array).into_iter().flatten() {
                match spot_balance(b) {
                    Ok(u) => out.push(PrivateEvent::Balance(u)),
                    Err(e) => skipped = skip_item("balance", &e),
                }
            }
            if skipped {
                out.push(PrivateEvent::ResyncRequired(ResyncReason::MalformedFrame));
            }
            out
        }
        "balanceUpdate" => vec![PrivateEvent::Account(AccountReason::Transfer)],
        "listStatus" => vec![PrivateEvent::ResyncRequired(ResyncReason::ListStatus)],
        "eventStreamTerminated" => vec![PrivateEvent::StreamTerminated],
        other => {
            log::debug!(
                "binance spot stream: ignoring event type {}",
                loggable_name(other)
            );
            Vec::new()
        }
    };
    Ok(SpotMessage::Events(events))
}

fn spot_balance(b: &Value) -> Result<BalanceUpdate, ParseError> {
    let asset = str_of(b, "a").ok_or(ParseError::Shape)?;
    if asset.is_empty() || asset.len() > 16 {
        return Err(ParseError::Shape);
    }
    let free = decimal_opt(b, "f")?;
    let locked = decimal_opt(b, "l")?;
    let total = match (free, locked) {
        (Some(f), Some(l)) => f.checked_add(&l).ok(),
        _ => None,
    };
    Ok(BalanceUpdate {
        asset: asset.to_string(),
        free,
        total,
    })
}

fn spot_order(v: &Value) -> Result<Vec<PrivateEvent>, ParseError> {
    let symbol = symbol_req(v, "s")?;
    let raw_status = str_of(v, "X").ok_or(ParseError::Shape)?;
    let status = match raw_status {
        "NEW" | "PENDING_NEW" => OrderStatus::New,
        "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
        "FILLED" => OrderStatus::Filled,
        "CANCELED" => OrderStatus::Canceled,
        "EXPIRED" | "EXPIRED_IN_MATCH" => OrderStatus::Expired,
        "REJECTED" => OrderStatus::Rejected,
        _ => {
            return Ok(vec![PrivateEvent::ResyncRequired(
                ResyncReason::UnknownOrderStatus,
            )]);
        }
    };
    // On a cancel `c` is the id of the cancel request and `C` the id of the order itself.
    let client_id = if str_of(v, "x") == Some("CANCELED") {
        id_opt(v, "C")
    } else {
        id_opt(v, "c")
    };
    let order_id = id_opt(v, "i");
    let side = side_of(str_of(v, "S"));
    let time_ms = int_opt(v, "T").or_else(|| int_opt(v, "E"));
    let mut out = vec![PrivateEvent::Order(OrderUpdate {
        symbol: symbol.clone(),
        client_id: client_id.clone(),
        order_id: order_id.clone(),
        category: OrderCategory::Plain,
        side,
        status,
        price: decimal_opt(v, "p")?,
        qty: decimal_opt(v, "q")?,
        filled_qty: decimal_opt(v, "z")?,
        reduce_only: None,
        position_side: None,
        time_ms,
    })];
    if str_of(v, "x") == Some("TRADE") {
        let qty = decimal_req(v, "l")?;
        if !qty.is_zero() {
            out.push(PrivateEvent::Fill(Fill {
                symbol,
                client_id,
                order_id,
                trade_id: id_opt(v, "t"),
                side,
                qty,
                price: decimal_req(v, "L")?,
                fee: decimal_opt(v, "n")?,
                fee_asset: str_of(v, "N").map(str::to_string).filter(|a| a.len() <= 16),
                realized_pnl: None,
                time_ms,
            }));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "pqia91ma19a5s61cv6a81va65sdf19v8a65a1a5s61cv6a81va65sdf19v8a65a1";

    fn dec(s: &str) -> crate::trade::Decimal {
        crate::trade::Decimal::parse(s).unwrap()
    }

    // Shape of the documented ORDER_TRADE_UPDATE example (values are made up).
    const ORDER_TRADE: &str = r#"{"e":"ORDER_TRADE_UPDATE","T":1568879465651,"E":1568879465651,
        "o":{"s":"BTCUSDT","c":"eTESTCLIENTID","S":"SELL","o":"TRAILING_STOP_MARKET","f":"GTC",
        "q":"0.001","p":"0","ap":"0","sp":"7103.04","x":"TRADE","X":"PARTIALLY_FILLED","i":8886774,
        "l":"0.0005","z":"0.0005","L":"7100.50","N":"USDT","n":"0.0001","T":1568879465650,
        "t":12345,"b":"0","a":"9.91","m":false,"R":true,"wt":"CONTRACT_PRICE","ot":"TRAILING_STOP_MARKET",
        "ps":"LONG","cp":false,"rp":"2.5","pP":false,"si":0,"ss":0}}"#;

    #[test]
    fn futures_order_update_gives_an_order_and_a_fill() {
        let events = parse_futures(ORDER_TRADE.as_bytes()).unwrap();
        assert_eq!(events.len(), 2);
        let PrivateEvent::Order(o) = &events[0] else {
            panic!()
        };
        assert_eq!(o.symbol, "BTCUSDT");
        assert_eq!(o.client_id.as_deref(), Some("eTESTCLIENTID"));
        assert_eq!(o.order_id.as_deref(), Some("8886774"));
        assert_eq!(o.status, OrderStatus::PartiallyFilled);
        assert_eq!(o.side, Some(crate::trade::Side::Sell));
        assert_eq!(o.reduce_only, Some(true));
        assert_eq!(o.position_side, Some(PositionSide::Long));
        assert_eq!(o.filled_qty, Some(dec("0.0005")));
        let PrivateEvent::Fill(f) = &events[1] else {
            panic!()
        };
        assert_eq!(f.qty, dec("0.0005"));
        assert_eq!(f.price, dec("7100.50"));
        assert_eq!(f.trade_id.as_deref(), Some("12345"));
        assert_eq!(f.realized_pnl, Some(dec("2.5")));
    }

    #[test]
    fn combined_stream_envelope_is_unwrapped_and_its_name_never_kept() {
        let wrapped = format!(r#"{{"stream":"{KEY}","data":{ORDER_TRADE}}}"#);
        let events = parse_futures(wrapped.as_bytes()).unwrap();
        assert_eq!(events.len(), 2);
        assert!(!format!("{events:?}").contains(KEY));
    }

    #[test]
    fn futures_account_update_carries_the_reason_balances_and_positions() {
        let frame = r#"{"e":"ACCOUNT_UPDATE","E":1564745798939,"T":1564745798938,"a":{"m":"DEPOSIT",
            "B":[{"a":"USDT","wb":"122624.12345678","cw":"100.12345678","bc":"50.12345678"}],
            "P":[{"s":"BTCUSDT","pa":"-20","ep":"6563.66500","cr":"0","up":"0","mt":"isolated","iw":"0","ps":"BOTH"}]}}"#;
        let events = parse_futures(frame.as_bytes()).unwrap();
        assert_eq!(events[0], PrivateEvent::Account(AccountReason::Deposit));
        assert!(matches!(&events[1], PrivateEvent::Balance(b) if b.asset == "USDT"));
        let PrivateEvent::Position(p) = &events[2] else {
            panic!()
        };
        assert_eq!(p.qty, dec("-20"));
        assert_eq!(p.position_side, PositionSide::Both);
        assert!(AccountReason::Deposit.is_transfer());
        assert!(!AccountReason::Order.is_transfer());
    }

    #[test]
    fn futures_algo_update_is_an_algo_order() {
        let frame = r#"{"e":"ALGO_UPDATE","T":1750515742297,"E":1750515742303,"o":{"caid":"eALGOCLIENT",
            "aid":2148719,"at":"CONDITIONAL","o":"TAKE_PROFIT","s":"BNBUSDT","S":"SELL","ps":"BOTH",
            "f":"GTC","q":"0.01","X":"TRIGGERED","ai":"","ap":"0.00000","aq":"0.00000","tp":"750","p":"750","R":false}}"#;
        let events = parse_futures(frame.as_bytes()).unwrap();
        let PrivateEvent::Order(o) = &events[0] else {
            panic!()
        };
        assert_eq!(o.category, OrderCategory::Algo);
        assert_eq!(o.client_id.as_deref(), Some("eALGOCLIENT"));
        assert_eq!(o.order_id.as_deref(), Some("2148719"));
        assert_eq!(o.status, OrderStatus::Triggered);
    }

    #[test]
    fn listen_key_expired_is_an_event_and_does_not_keep_the_key() {
        let frame = format!(r#"{{"e":"listenKeyExpired","E":1736996475556,"listenKey":"{KEY}"}}"#);
        let events = parse_futures(frame.as_bytes()).unwrap();
        assert_eq!(events, vec![PrivateEvent::ListenKeyExpired]);
        assert!(!format!("{events:?}").contains(KEY));
    }

    #[test]
    fn unknown_status_asks_for_a_resync_and_unknown_events_are_ignored() {
        let frame = ORDER_TRADE.replace("PARTIALLY_FILLED", "SOMETHING_NEW");
        assert_eq!(
            parse_futures(frame.as_bytes()).unwrap(),
            vec![PrivateEvent::ResyncRequired(
                ResyncReason::UnknownOrderStatus
            )]
        );
        assert_eq!(
            parse_futures(br#"{"e":"MARGIN_CALL","E":1}"#).unwrap(),
            Vec::new()
        );
        assert_eq!(
            parse_futures(br#"{"result":null,"id":1}"#).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn malformed_futures_frames_are_errors_not_panics() {
        let cases: [&[u8]; 11] = [
            b"",
            b"not json",
            b"[]",
            b"null",
            br#"{"e":"ORDER_TRADE_UPDATE"}"#,
            br#"{"e":"ORDER_TRADE_UPDATE","o":{}}"#,
            br#"{"e":"ORDER_TRADE_UPDATE","o":{"s":"BTC USDT","X":"NEW"}}"#,
            br#"{"e":"ORDER_TRADE_UPDATE","o":{"s":"BTCUSDT","X":5}}"#,
            br#"{"e":"ORDER_TRADE_UPDATE","o":{"s":"BTCUSDT","X":"NEW","q":1.5}}"#,
            br#"{"e":"ORDER_TRADE_UPDATE","o":{"s":"BTCUSDT","X":"NEW","q":"1e5"}}"#,
            br#"{"e":"ORDER_TRADE_UPDATE","o":{"s":"BTCUSDT","X":"PARTIALLY_FILLED","x":"TRADE","c":"a"}}"#,
        ];
        for c in cases {
            let r = parse_futures(c);
            if c == b"[]" || c == b"null" || c == br#"{"e":"ORDER_TRADE_UPDATE"}"# {
                // No event type, or a missing body.
                assert!(r.is_ok() || r.is_err());
            } else {
                assert!(r.is_err(), "{}", String::from_utf8_lossy(c));
            }
        }
        // One unreadable balance or position is skipped; the rest of the push is kept and one
        // REST read is asked for.
        let events = parse_futures(
            br#"{"e":"ACCOUNT_UPDATE","a":{"m":"ORDER","B":[{"a":1},{"a":"USDT","wb":"5"}],"P":[{"s":"bad symbol","ps":"BOTH","pa":"1"}]}}"#,
        )
        .unwrap();
        assert!(matches!(&events[..], [
            PrivateEvent::Account(_),
            PrivateEvent::Balance(b),
            PrivateEvent::ResyncRequired(ResyncReason::MalformedFrame),
        ] if b.asset == "USDT"));
        // A frame over the cap is refused before it is parsed.
        let big = vec![b' '; super::super::events::MAX_FRAME_BYTES + 1];
        assert_eq!(parse_futures(&big), Err(ParseError::TooLarge));
        // A very long string value does not break anything.
        let long = format!(
            r#"{{"e":"ORDER_TRADE_UPDATE","o":{{"s":"{}","X":"NEW"}}}}"#,
            "A".repeat(10_000)
        );
        assert!(parse_futures(long.as_bytes()).is_err());
        // Multi-byte text in an ignored field is fine.
        assert!(
            parse_futures("{\"e\":\"錯誤\"}".as_bytes())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn urls_only_come_from_the_whitelist_and_hold_the_key_only_in_the_futures_one() {
        let key = ListenKey::parse(KEY).unwrap();
        let live = futures_stream_url(AccountEnv::Live, &key).unwrap();
        assert_eq!(live.scheme(), "wss");
        assert_eq!(live.host_str(), Some("fstream.binance.com"));
        let test = futures_stream_url(AccountEnv::Testnet, &key).unwrap();
        assert_eq!(test.host_str(), Some("fstream.binancefuture.com"));
        let spot = spot_ws_api_url(AccountEnv::Testnet).unwrap();
        assert_eq!(spot.host_str(), Some("ws-api.testnet.binance.vision"));
        assert!(!spot.as_str().contains(KEY));
    }

    #[test]
    fn spot_subscribe_signs_the_parameters_in_alphabetical_order() {
        let frame = spot_subscribe_frame("req-1", "APIKEYTEST", "SECRETTEST", 1770736694138);
        let v: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["method"], "userDataStream.subscribe.signature");
        let p = &v["params"];
        assert_eq!(p["apiKey"], "APIKEYTEST");
        assert_eq!(p["recvWindow"], 5000);
        assert_eq!(p["timestamp"], 1770736694138i64);
        // The signature covers exactly the sorted parameters without the signature itself.
        let payload = "apiKey=APIKEYTEST&recvWindow=5000&timestamp=1770736694138";
        assert_eq!(spot_sign_payload("APIKEYTEST", 1770736694138), payload);
        assert_eq!(p["signature"], binance_sign("SECRETTEST", payload));
        // Golden value, computed independently (Python hmac/sha256).
        assert_eq!(
            p["signature"],
            "dd993c1a3fc76a7ff6c71739fec6437b0fd7bfccbccf9b85bee3bef030d7a255"
        );
        let mut names: Vec<_> = p.as_object().unwrap().keys().cloned().collect();
        names.retain(|n| n != "signature");
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(sorted, ["apiKey", "recvWindow", "timestamp"]);
    }

    #[test]
    fn spot_frames_parse_and_failures_are_classified() {
        assert_eq!(
            parse_spot(br#"{"id":"1","status":200,"result":{"subscriptionId":0}}"#).unwrap(),
            SpotMessage::Subscribed
        );
        let bad_sig =
            parse_spot(br#"{"id":"1","status":400,"error":{"code":-1022,"msg":"x"}}"#).unwrap();
        assert!(bad_sig.is_auth_failure());
        let skew =
            parse_spot(br#"{"id":"1","status":400,"error":{"code":-1021,"msg":"x"}}"#).unwrap();
        assert!(!skew.is_auth_failure());
        let report = r#"{"subscriptionId":0,"event":{"e":"executionReport","E":1,"s":"BTCUSDT",
            "c":"ecancelreq","S":"BUY","o":"LIMIT","q":"1.0","p":"10.0","x":"CANCELED","X":"CANCELED",
            "i":4293153,"l":"0.0","z":"0.0","L":"0.0","C":"eORIGINAL","T":2}}"#;
        let SpotMessage::Events(ev) = parse_spot(report.as_bytes()).unwrap() else {
            panic!()
        };
        let PrivateEvent::Order(o) = &ev[0] else {
            panic!()
        };
        assert_eq!(o.client_id.as_deref(), Some("eORIGINAL"));
        assert_eq!(
            parse_spot(br#"{"event":{"e":"eventStreamTerminated","E":1}}"#).unwrap(),
            SpotMessage::Events(vec![PrivateEvent::StreamTerminated])
        );
        assert!(parse_spot(b"{{").is_err());
    }

    #[test]
    fn the_spot_stream_is_off_until_verified() {
        // The single switch (condition 48, user decision 3). Flipping it needs the testnet record.
        const { assert!(!SPOT_PRIVATE_STREAM_VERIFIED) };
    }
}
