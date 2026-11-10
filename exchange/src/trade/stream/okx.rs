//! OKX private streams: two logged-in connections, `private` (orders, positions, account) and
//! `business` (algo orders), condition 47 and 50.
//!
//! The login frame holds the api key, passphrase and signature. It is built right before it is
//! written, from credentials loaded for this connection only, and is never logged.

use super::events::{
    BalanceUpdate, EquityUpdate, Fill, OrderCategory, OrderStatus, OrderUpdate, ParseError,
    PositionSide, PositionUpdate, PrivateEvent, ResyncReason, clean_code, decimal_opt, decimal_req,
    decode, id_opt, int_opt, loggable_name, side_of, str_of, symbol_req,
};
use crate::account::{
    AccountEnv, AccountError, AccountProduct, AccountVenue, ApiCredentials, okx_sign, wss_endpoint,
};
use secrecy::ExposeSecret;
use serde_json::{Value, json};

/// The two connections OKX needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OkxLeg {
    Private,
    Business,
}

impl OkxLeg {
    pub const ALL: [OkxLeg; 2] = [OkxLeg::Private, OkxLeg::Business];

    fn path(self) -> &'static str {
        match self {
            OkxLeg::Private => "private",
            OkxLeg::Business => "business",
        }
    }
}

/// `https://…` is never used here: only `wss`, port 443, from the whitelist (condition 40).
/// The URL holds no secret.
pub(crate) fn ws_url(
    env: AccountEnv,
    product: AccountProduct,
    leg: OkxLeg,
) -> Result<reqwest::Url, AccountError> {
    let host = match env {
        AccountEnv::Live => "ws.okx.com",
        AccountEnv::Testnet => "wspap.okx.com",
    };
    wss_endpoint(
        AccountVenue::Okx,
        env,
        product,
        &format!("wss://{host}/ws/v5/{}", leg.path()),
    )
}

/// Unix time in **seconds** (REST uses ISO milliseconds; WebSocket login does not).
pub(crate) fn okx_ws_timestamp(now_ms: i64) -> String {
    (now_ms.div_euclid(1000)).to_string()
}

/// The `login` frame. The prehash is `timestamp + "GET" + "/users/self/verify"`. Returns `None`
/// when the credentials hold no passphrase.
pub(crate) fn login_frame(creds: &ApiCredentials, timestamp: &str) -> Option<String> {
    let passphrase = creds.passphrase()?;
    let sign = okx_sign(
        creds.secret().expose_secret(),
        timestamp,
        "GET",
        "/users/self/verify",
        "",
    );
    Some(
        json!({
            "op": "login",
            "args": [{
                "apiKey": creds.key().expose_secret(),
                "passphrase": passphrase.expose_secret(),
                "timestamp": timestamp,
                "sign": sign,
            }]
        })
        .to_string(),
    )
}

/// The channels a leg subscribes to; the leg is up only once each one is confirmed.
pub(crate) fn leg_channels(leg: OkxLeg, product: AccountProduct) -> &'static [&'static str] {
    match (leg, product) {
        (OkxLeg::Private, AccountProduct::UsdM) => &["orders", "positions", "account"],
        (OkxLeg::Private, AccountProduct::Spot) => &["orders", "account"],
        (OkxLeg::Business, _) => &["orders-algo", "algo-advance"],
    }
}

/// The subscribe frame for a leg. Holds no secret.
pub(crate) fn subscribe_frame(leg: OkxLeg, product: AccountProduct) -> String {
    let inst_type = match product {
        AccountProduct::Spot => "SPOT",
        AccountProduct::UsdM => "SWAP",
    };
    let channels = leg_channels(leg, product);
    let args: Vec<Value> = channels
        .iter()
        .map(|c| match *c {
            "account" => json!({ "channel": c }),
            _ => json!({ "channel": c, "instType": inst_type }),
        })
        .collect();
    json!({ "id": "1", "op": "subscribe", "args": args }).to_string()
}

/// What an OKX frame is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OkxMessage {
    /// The answer to our text `ping`.
    Pong,
    LoginOk,
    /// An `error` event or a failed login. `code` is cleaned (`[-0-9A-Za-z]`, ≤16).
    Error {
        code: String,
    },
    /// A `notice` (the server will close the connection soon): reconnect.
    Notice,
    /// A `subscribe` confirmation. `channel` is cleaned and may be empty.
    Subscribed {
        channel: String,
    },
    /// `unsubscribe` confirmations and other events with no data.
    Ack,
    Events(Vec<PrivateEvent>),
}

/// Login failures that mean the credentials are wrong or revoked, as opposed to a timestamp
/// that is off (`60004`, `60006`: fix the clock and retry after the backoff). Anything else
/// that arrives as a login error is treated as an authentication failure too (fail closed).
pub fn is_clock_error(code: &str) -> bool {
    matches!(code, "60004" | "60006")
}

pub fn parse_okx(frame: &[u8]) -> Result<OkxMessage, ParseError> {
    if frame == b"pong" {
        return Ok(OkxMessage::Pong);
    }
    let v = decode(frame)?;
    if let Some(event) = str_of(&v, "event") {
        return Ok(match event {
            "login" => {
                if str_of(&v, "code") == Some("0") {
                    OkxMessage::LoginOk
                } else {
                    OkxMessage::Error {
                        code: clean_code(str_of(&v, "code").unwrap_or("?")),
                    }
                }
            }
            "error" => OkxMessage::Error {
                code: clean_code(str_of(&v, "code").unwrap_or("?")),
            },
            "notice" => OkxMessage::Notice,
            "subscribe" => OkxMessage::Subscribed {
                channel: v
                    .get("arg")
                    .and_then(|a| str_of(a, "channel"))
                    .map(clean_code)
                    .unwrap_or_default(),
            },
            _ => OkxMessage::Ack,
        });
    }
    let Some(arg) = v.get("arg") else {
        return Ok(OkxMessage::Ack);
    };
    let channel = str_of(arg, "channel").ok_or(ParseError::Shape)?;
    let data = v.get("data").and_then(Value::as_array);
    let events = match channel {
        "orders" => per_item(channel, data, order_event)?,
        "orders-algo" | "algo-advance" => per_item(channel, data, algo_event)?,
        "positions" => per_item(channel, data, position_event)?,
        "account" => per_item(channel, data, account_event)?,
        other => {
            log::debug!(
                "okx private stream: ignoring channel {}",
                loggable_name(other)
            );
            Vec::new()
        }
    };
    Ok(OkxMessage::Events(events))
}

/// One item that cannot be read is skipped (only its kind is logged) and does not fail the
/// push; the push then also asks for one REST read, as local state may have missed it. The
/// read is rate-limited by the machine, so a standing bad item cannot flood REST.
fn per_item(
    channel: &str,
    data: Option<&Vec<Value>>,
    f: impl Fn(&Value) -> Result<Vec<PrivateEvent>, ParseError>,
) -> Result<Vec<PrivateEvent>, ParseError> {
    let mut out = Vec::new();
    let mut skipped = false;
    for item in data.ok_or(ParseError::Shape)? {
        match f(item) {
            Ok(events) => out.extend(events),
            Err(e) => {
                log::warn!(
                    "okx private stream: skipped an unreadable {} item ({e})",
                    loggable_name(channel)
                );
                skipped = true;
            }
        }
    }
    if skipped {
        out.push(PrivateEvent::ResyncRequired(ResyncReason::MalformedFrame));
    }
    Ok(out)
}

fn order_status(raw: &str) -> Option<OrderStatus> {
    Some(match raw {
        "live" => OrderStatus::New,
        "partially_filled" => OrderStatus::PartiallyFilled,
        "filled" => OrderStatus::Filled,
        "canceled" | "mmp_canceled" => OrderStatus::Canceled,
        _ => return None,
    })
}

fn algo_status(raw: &str) -> Option<OrderStatus> {
    Some(match raw {
        "live" => OrderStatus::New,
        "partially_effective" | "effective" => OrderStatus::Triggered,
        "canceled" => OrderStatus::Canceled,
        "order_failed" => OrderStatus::Rejected,
        _ => return None,
    })
}

fn pos_side(raw: Option<&str>) -> Option<PositionSide> {
    match raw? {
        "net" => Some(PositionSide::Both),
        "long" => Some(PositionSide::Long),
        "short" => Some(PositionSide::Short),
        _ => None,
    }
}

fn bool_text(v: &Value, key: &str) -> Option<bool> {
    match str_of(v, key)? {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn order_event(d: &Value) -> Result<Vec<PrivateEvent>, ParseError> {
    let symbol = symbol_req(d, "instId")?;
    let state = str_of(d, "state").ok_or(ParseError::Shape)?;
    let Some(status) = order_status(state) else {
        return Ok(vec![PrivateEvent::ResyncRequired(
            ResyncReason::UnknownOrderStatus,
        )]);
    };
    let client_id = id_opt(d, "clOrdId");
    let order_id = id_opt(d, "ordId");
    let side = side_of(str_of(d, "side"));
    let time_ms = int_opt(d, "uTime");
    let mut out = vec![PrivateEvent::Order(OrderUpdate {
        symbol: symbol.clone(),
        client_id: client_id.clone(),
        order_id: order_id.clone(),
        category: OrderCategory::Plain,
        side,
        status,
        price: decimal_opt(d, "px")?,
        qty: decimal_opt(d, "sz")?,
        filled_qty: decimal_opt(d, "accFillSz")?,
        reduce_only: bool_text(d, "reduceOnly"),
        position_side: pos_side(str_of(d, "posSide")),
        time_ms,
    })];
    // A fill is a push that carries a trade id and a non-zero last fill size.
    if let (Some(trade_id), Some(qty)) = (id_opt(d, "tradeId"), decimal_opt(d, "fillSz")?)
        && !qty.is_zero()
    {
        out.push(PrivateEvent::Fill(Fill {
            symbol,
            client_id,
            order_id,
            trade_id: Some(trade_id),
            side,
            qty,
            price: decimal_req(d, "fillPx")?,
            fee: decimal_opt(d, "fillFee")?,
            fee_asset: str_of(d, "feeCcy")
                .map(str::to_string)
                .filter(|a| a.len() <= 16),
            realized_pnl: decimal_opt(d, "pnl")?,
            time_ms: int_opt(d, "fillTime").or(time_ms),
        }));
    }
    Ok(out)
}

fn algo_event(d: &Value) -> Result<Vec<PrivateEvent>, ParseError> {
    let symbol = symbol_req(d, "instId")?;
    let state = str_of(d, "state").ok_or(ParseError::Shape)?;
    let Some(status) = algo_status(state) else {
        return Ok(vec![PrivateEvent::ResyncRequired(
            ResyncReason::UnknownOrderStatus,
        )]);
    };
    Ok(vec![PrivateEvent::Order(OrderUpdate {
        symbol,
        client_id: id_opt(d, "algoClOrdId"),
        order_id: id_opt(d, "algoId"),
        category: OrderCategory::Algo,
        side: side_of(str_of(d, "side")),
        status,
        price: decimal_opt(d, "ordPx")?,
        qty: decimal_opt(d, "sz")?,
        filled_qty: None,
        reduce_only: bool_text(d, "reduceOnly"),
        position_side: pos_side(str_of(d, "posSide")),
        time_ms: int_opt(d, "uTime"),
    })])
}

fn position_event(d: &Value) -> Result<Vec<PrivateEvent>, ParseError> {
    Ok(vec![PrivateEvent::Position(PositionUpdate {
        symbol: symbol_req(d, "instId")?,
        position_side: pos_side(str_of(d, "posSide")).ok_or(ParseError::Shape)?,
        qty: decimal_req(d, "pos")?,
        entry_price: decimal_opt(d, "avgPx")?,
    })])
}

fn account_event(d: &Value) -> Result<Vec<PrivateEvent>, ParseError> {
    let mut out = Vec::new();
    if let Some(total_equity) = decimal_opt(d, "totalEq")? {
        out.push(PrivateEvent::Equity(EquityUpdate { total_equity }));
    }
    for b in d
        .get("details")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let asset = str_of(b, "ccy").ok_or(ParseError::Shape)?;
        if asset.is_empty() || asset.len() > 16 {
            return Err(ParseError::Shape);
        }
        out.push(PrivateEvent::Balance(BalanceUpdate {
            asset: asset.to_string(),
            free: decimal_opt(b, "availBal")?,
            total: decimal_opt(b, "eq")?,
        }));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> ApiCredentials {
        ApiCredentials::new(
            "OKXKEYTEST".into(),
            "OKXSECRETTEST".into(),
            Some("OKXPASSTEST".into()),
        )
    }

    fn dec(s: &str) -> crate::trade::Decimal {
        crate::trade::Decimal::parse(s).unwrap()
    }

    #[test]
    fn timestamp_is_unix_seconds() {
        assert_eq!(okx_ws_timestamp(1_538_054_050_999), "1538054050");
        assert_eq!(okx_ws_timestamp(0), "0");
    }

    #[test]
    fn login_signs_timestamp_get_verify_path() {
        // Golden value: base64(HMAC-SHA256("OKXSECRETTEST", "1538054050GET/users/self/verify")),
        // computed independently (Python hmac/hashlib/base64). The timestamp is the docs example.
        let frame = login_frame(&creds(), "1538054050").unwrap();
        let v: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["op"], "login");
        let a = &v["args"][0];
        assert_eq!(a["apiKey"], "OKXKEYTEST");
        assert_eq!(a["passphrase"], "OKXPASSTEST");
        assert_eq!(a["timestamp"], "1538054050");
        assert_eq!(a["sign"], "SY1P06f0rvOAbwONhlgrO1xWHalkOSd+li0yb2j+MhU=");
        assert_eq!(v["args"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn login_needs_a_passphrase() {
        let none = ApiCredentials::new("k".into(), "s".into(), None);
        assert!(login_frame(&none, "1").is_none());
    }

    #[test]
    fn urls_come_from_the_whitelist_by_environment() {
        let live = ws_url(AccountEnv::Live, AccountProduct::UsdM, OkxLeg::Private).unwrap();
        assert_eq!(live.as_str(), "wss://ws.okx.com/ws/v5/private");
        let test = ws_url(AccountEnv::Testnet, AccountProduct::UsdM, OkxLeg::Business).unwrap();
        assert_eq!(test.as_str(), "wss://wspap.okx.com/ws/v5/business");
    }

    #[test]
    fn subscribe_frames_hold_no_secret_and_name_the_right_channels() {
        let p = subscribe_frame(OkxLeg::Private, AccountProduct::UsdM);
        let b = subscribe_frame(OkxLeg::Business, AccountProduct::UsdM);
        for c in ["\"orders\"", "\"positions\"", "\"account\""] {
            assert!(p.contains(c));
        }
        for c in ["\"orders-algo\"", "\"algo-advance\""] {
            assert!(b.contains(c));
        }
        assert!(!p.contains("OKXKEY") && !b.contains("OKXKEY"));
    }

    #[test]
    fn control_frames_are_classified() {
        assert_eq!(parse_okx(b"pong").unwrap(), OkxMessage::Pong);
        assert_eq!(
            parse_okx(br#"{"event":"login","code":"0","msg":"","connId":"a"}"#).unwrap(),
            OkxMessage::LoginOk
        );
        assert_eq!(
            parse_okx(br#"{"event":"error","code":"60009","msg":"Login failed.","connId":"a"}"#)
                .unwrap(),
            OkxMessage::Error {
                code: "60009".into()
            }
        );
        // A hostile code is replaced, not copied.
        assert_eq!(
            parse_okx(br#"{"event":"error","code":"60009 apiKey=SECRET","msg":""}"#).unwrap(),
            OkxMessage::Error { code: "?".into() }
        );
        assert_eq!(
            parse_okx(br#"{"event":"subscribe","arg":{"channel":"orders"}}"#).unwrap(),
            OkxMessage::Subscribed {
                channel: "orders".into()
            }
        );
        assert_eq!(
            parse_okx(br#"{"event":"unsubscribe","arg":{"channel":"orders"}}"#).unwrap(),
            OkxMessage::Ack
        );
        assert_eq!(
            parse_okx(br#"{"event":"notice","code":"64008","msg":"x"}"#).unwrap(),
            OkxMessage::Notice
        );
        assert!(is_clock_error("60006") && !is_clock_error("60009"));
    }

    #[test]
    fn order_push_gives_an_order_and_a_fill() {
        let frame = r#"{"arg":{"channel":"orders","instType":"SWAP","uid":"1"},"data":[{
            "instType":"SWAP","instId":"BTC-USDT-SWAP","ordId":"312269865356374016","clOrdId":"eOKXCLIENT",
            "px":"2.15","sz":"2","ordType":"limit","side":"buy","posSide":"long","reduceOnly":"false",
            "state":"partially_filled","fillSz":"1","fillPx":"2.15","tradeId":"5","accFillSz":"1",
            "fillFee":"-0.001","feeCcy":"USDT","pnl":"0","uTime":"1597026383085","fillTime":"1597026383084"}]}"#;
        let OkxMessage::Events(ev) = parse_okx(frame.as_bytes()).unwrap() else {
            panic!()
        };
        assert_eq!(ev.len(), 2);
        let PrivateEvent::Order(o) = &ev[0] else {
            panic!()
        };
        assert_eq!(o.symbol, "BTC-USDT-SWAP");
        assert_eq!(o.client_id.as_deref(), Some("eOKXCLIENT"));
        assert_eq!(o.status, OrderStatus::PartiallyFilled);
        assert_eq!(o.position_side, Some(PositionSide::Long));
        assert_eq!(o.reduce_only, Some(false));
        let PrivateEvent::Fill(f) = &ev[1] else {
            panic!()
        };
        assert_eq!(f.qty, dec("1"));
        assert_eq!(f.fee, Some(dec("-0.001")));
        assert_eq!(f.trade_id.as_deref(), Some("5"));
    }

    #[test]
    fn position_account_and_algo_pushes() {
        let pos =
            br#"{"arg":{"channel":"positions","instType":"SWAP"},"data":[{"instId":"BTC-USDT-SWAP",
            "posSide":"net","pos":"-3","avgPx":"20000.5","mgnMode":"cross"}]}"#;
        let OkxMessage::Events(ev) = parse_okx(pos).unwrap() else {
            panic!()
        };
        let PrivateEvent::Position(p) = &ev[0] else {
            panic!()
        };
        assert_eq!(p.qty, dec("-3"));
        assert_eq!(p.position_side, PositionSide::Both);

        let acc =
            br#"{"arg":{"channel":"account"},"data":[{"totalEq":"91884.8","uTime":"1","details":[
            {"ccy":"USDT","availBal":"100","cashBal":"120","eq":"121"}]}]}"#;
        let OkxMessage::Events(ev) = parse_okx(acc).unwrap() else {
            panic!()
        };
        assert_eq!(
            ev[0],
            PrivateEvent::Equity(EquityUpdate {
                total_equity: dec("91884.8")
            })
        );
        assert!(matches!(&ev[1], PrivateEvent::Balance(b) if b.free == Some(dec("100"))));

        let algo = br#"{"arg":{"channel":"orders-algo","instType":"SWAP"},"data":[{"instId":"BTC-USDT-SWAP",
            "algoId":"1","algoClOrdId":"eALGO","state":"effective","side":"sell","posSide":"net","sz":"1"}]}"#;
        let OkxMessage::Events(ev) = parse_okx(algo).unwrap() else {
            panic!()
        };
        let PrivateEvent::Order(o) = &ev[0] else {
            panic!()
        };
        assert_eq!(o.category, OrderCategory::Algo);
        assert_eq!(o.status, OrderStatus::Triggered);
        assert_eq!(o.client_id.as_deref(), Some("eALGO"));
    }

    #[test]
    fn unknown_state_resyncs_and_unknown_channels_are_ignored() {
        let frame =
            br#"{"arg":{"channel":"orders"},"data":[{"instId":"BTC-USDT","state":"brand_new"}]}"#;
        let OkxMessage::Events(ev) = parse_okx(frame).unwrap() else {
            panic!()
        };
        assert_eq!(
            ev,
            vec![PrivateEvent::ResyncRequired(
                ResyncReason::UnknownOrderStatus
            )]
        );
        let other = br#"{"arg":{"channel":"liquidation-warning"},"data":[]}"#;
        assert_eq!(parse_okx(other).unwrap(), OkxMessage::Events(Vec::new()));
    }

    #[test]
    fn malformed_okx_frames_are_errors_not_panics() {
        let cases: [&[u8]; 5] = [
            b"",
            b"ping",
            b"{",
            br#"{"arg":{"channel":"orders"}}"#,
            br#"{"arg":{"channel":"orders"},"data":{}}"#,
        ];
        for c in cases {
            assert!(parse_okx(c).is_err(), "{}", String::from_utf8_lossy(c));
        }
        // One unreadable item is skipped; the push is not lost and asks for a REST read.
        let items: [&[u8]; 5] = [
            br#"{"arg":{"channel":"orders"},"data":[{"instId":"BTC-USDT"}]}"#,
            br#"{"arg":{"channel":"orders"},"data":[{"instId":"btc usdt","state":"live"}]}"#,
            br#"{"arg":{"channel":"orders"},"data":[{"instId":"BTC-USDT","state":"live","sz":2}]}"#,
            br#"{"arg":{"channel":"positions"},"data":[{"instId":"BTC-USDT","posSide":"x","pos":"1"}]}"#,
            br#"{"arg":{"channel":"account"},"data":[{"totalEq":"abc"}]}"#,
        ];
        for c in items {
            assert_eq!(
                parse_okx(c).unwrap(),
                OkxMessage::Events(vec![PrivateEvent::ResyncRequired(
                    ResyncReason::MalformedFrame
                )]),
                "{}",
                String::from_utf8_lossy(c)
            );
        }
        let big = vec![b'x'; super::super::events::MAX_FRAME_BYTES + 1];
        assert_eq!(parse_okx(&big), Err(ParseError::TooLarge));
    }
}
