//! Send-layer tests against a local fake server (no real exchange is contacted). Each test
//! uses its own limiter host name so limiter state is not shared between tests.

use super::binance_futures as bf;
use super::binance_spot as bs;
use super::client::{OpenKind, TradeClient};
use super::client_id::{ClientId, Role};
use super::gate::{
    AutoIntent, AutoPath, ConfirmToken, GateRequest, OrderIntent, RiskHook, RiskRefusal, RiskView,
    SendGate, SingleOrderCap, SubmitError,
};
use super::okx::{self, OkxAlgoType, OkxProtective, OkxTarget, TriggerPxType};
use super::outcome::{Outcome, RejectKind};
use super::request::{BinanceRequest, Exposure, OkxRequest};
use super::rules::fixtures::*;
use super::rules::{RulesTable, SymbolRules};
use super::types::*;
use super::unknown::{PendingUnknown, Resolution};
use crate::account::{
    AccountEnv, AccountKey, AccountProduct, AccountVenue, ApiCredentials, PositionMode, ProxyState,
};
use crate::rate_limit::apply_exchange_info_limits;
use serde_json::json;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ── Fake server ─────────────────────────────────────────────────────────

type Handler = dyn Fn(&str, &str) -> (u16, String) + Send + Sync;

struct Fake {
    port: u16,
    log: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    fn count(&self, needle: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.contains(needle))
            .count()
    }

    fn all(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

/// Serve every connection on a throw-away local port. The handler gets `(method, path)`
/// (without the query) and returns `(status, json body)`. Requests are logged as
/// `"METHOD path"`.
fn serve(handler: impl Fn(&str, &str) -> (u16, String) + Send + Sync + 'static) -> Fake {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log = Arc::new(Mutex::new(Vec::new()));
    let handler: Arc<Handler> = Arc::new(handler);
    let log2 = Arc::clone(&log);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { break };
            let handler = Arc::clone(&handler);
            let log = Arc::clone(&log2);
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = conn.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let want: usize = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse().ok())
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + want {
                            break;
                        }
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let mut parts = head.lines().next().unwrap_or("").split(' ');
                let method = parts.next().unwrap_or("").to_string();
                let target = parts.next().unwrap_or("").to_string();
                let path = target.split('?').next().unwrap_or("").to_string();
                log.lock().unwrap().push(format!("{method} {path}"));
                let (status, body) = handler(&method, &path);
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(reply.as_bytes());
            });
        }
    });
    Fake { port, log }
}

static HOSTS: AtomicUsize = AtomicUsize::new(0);

fn gen_binance(_m: &str, _p: &str) -> (u16, String) {
    (
        200,
        json!({"orderId": 11, "orderListId": 12, "algoId": 13, "clientOrderId": "x"}).to_string(),
    )
}

fn gen_okx(_m: &str, _p: &str) -> (u16, String) {
    (
        200,
        json!({"code": "0", "msg": "", "data": [
            {"sCode": "0", "sMsg": "", "ordId": "1", "algoId": "2", "clOrdId": "x"}
        ]})
        .to_string(),
    )
}

static ACCOUNTS: AtomicUsize = AtomicUsize::new(1);

fn unique_account() -> AccountKey {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&(ACCOUNTS.fetch_add(1, Ordering::Relaxed) as u64).to_be_bytes());
    AccountKey::from_bytes(b)
}

fn client(venue: AccountVenue, product: AccountProduct, fake: &Fake) -> TradeClient {
    client_with(venue, product, fake, true)
}

fn client_with(
    venue: AccountVenue,
    product: AccountProduct,
    fake: &Fake,
    known_limits: bool,
) -> TradeClient {
    let host = format!("t{}.test", HOSTS.fetch_add(1, Ordering::Relaxed));
    let mut c = TradeClient::new(
        reqwest::Client::new(),
        venue,
        AccountEnv::Live,
        product,
        // One account per client: the lock table is shared by account, and tests run in parallel.
        unique_account(),
        ApiCredentials::new(
            "test-key".into(),
            "test-secret".into(),
            (venue == AccountVenue::Okx).then(|| "test-pass".to_string()),
        ),
        ProxyState::Direct,
        Arc::new(|| ProxyState::Direct),
    );
    c.test_now_ms = Some(NOW);
    c.seam = Some(super::client::Seam {
        base: format!("http://127.0.0.1:{}", fake.port),
        limit_host: host.clone(),
    });
    if known_limits {
        // Order limits come from exchangeInfo; entries are refused until they are known.
        let info = json!({"rateLimits": [
            {"rateLimitType": "ORDERS", "interval": "SECOND", "intervalNum": 10, "limit": 300},
            {"rateLimitType": "ORDERS", "interval": "MINUTE", "intervalNum": 1, "limit": 1200}
        ]});
        apply_exchange_info_limits(&host, &info);
    }
    c
}

struct OpenGate;
impl SendGate for OpenGate {
    type Refusal = ();
    fn check(&self, _: GateRequest) -> Result<(), ()> {
        Ok(())
    }
}

struct NoRisk;
impl RiskHook for NoRisk {
    fn check(&self, _: &RiskView<'_>) -> Result<(), RiskRefusal> {
        Ok(())
    }
}

const NOW: i64 = 2_000;
const AGE: i64 = 60_000;

fn id(s: &str) -> ClientId {
    ClientId::parse(s).unwrap()
}

fn by_client() -> OrderRef {
    OrderRef::Client(id("eAAAA"))
}

fn futures_limit(rules: &SymbolRules, cid: &ClientId, action: Action) -> BinanceRequest {
    bf::new_order(&bf::FuturesEntry {
        rules,
        mode: Some(PositionMode::OneWay),
        action,
        qty: d("0.002"),
        exec: Exec::Limit {
            price: d("60000"),
            tif: TimeInForce::Gtc,
        },
        ref_price: Some(d("60000")),
        client_id: cid,
    })
    .unwrap()
}

/// place → cancel → query for one Binance row, through the gate and the signed transport.
async fn binance_row(
    c: &TradeClient,
    fake: &Fake,
    table: &RulesTable,
    symbol: &str,
    place: BinanceRequest,
    cancel: BinanceRequest,
    query: BinanceRequest,
    paths: [&str; 3],
) {
    let fresh = table.fresh_lookup(symbol, NOW, AGE).unwrap();
    for (req, method, path, accepted) in [
        (place, "POST", paths[0], true),
        (cancel, "DELETE", paths[1], true),
    ] {
        let before = fake.count(&format!("{method} {path}"));
        let intent = OrderIntent::binance(c.account(), req, &fresh).unwrap();
        let out = c
            .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), intent)
            .await
            .unwrap();
        assert_eq!(
            matches!(out.outcome, Outcome::Accepted { .. }),
            accepted,
            "{method} {path}: {:?}",
            out.outcome
        );
        assert_eq!(fake.count(&format!("{method} {path}")), before + 1);
    }
    let before = fake.count(&format!("GET {}", paths[2]));
    let v = c.read_binance(&query).await.unwrap();
    assert!(v.is_object());
    assert_eq!(fake.count(&format!("GET {}", paths[2])), before + 1);
}

// ── Condition 67: place, cancel, query for every opened type ────────────

#[tokio::test]
async fn matrix_binance_futures_send_paths() {
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let cid = id("eAAAA");
    binance_row(
        &c,
        &fake,
        &t,
        "BTCUSDT",
        futures_limit(r, &cid, Action::Open(Dir::Long)),
        bf::cancel_order(r, &by_client()).unwrap(),
        bf::query_order(r, &by_client()).unwrap(),
        ["/fapi/v1/order"; 3],
    )
    .await;
    // MARKET close (reducing)
    let market = bf::new_order(&bf::FuturesEntry {
        rules: r,
        mode: Some(PositionMode::OneWay),
        action: Action::Close(Dir::Long),
        qty: d("0.002"),
        exec: Exec::Market,
        ref_price: None,
        client_id: &cid,
    })
    .unwrap();
    binance_row(
        &c,
        &fake,
        &t,
        "BTCUSDT",
        market,
        bf::cancel_order(r, &by_client()).unwrap(),
        bf::query_order(r, &by_client()).unwrap(),
        ["/fapi/v1/order"; 3],
    )
    .await;
    // algo orders (all one-way kinds)
    for kind in [
        ProtectiveKind::StopMarket,
        ProtectiveKind::TakeProfitMarket,
        ProtectiveKind::StopLimit,
        ProtectiveKind::TakeProfitLimit,
        ProtectiveKind::Trailing,
    ] {
        let p = Protective {
            kind,
            dir: Dir::Long,
            size: Size::Qty(d("0.002")),
            trigger_price: (kind != ProtectiveKind::Trailing).then(|| {
                d(if kind.is_stop_side() {
                    "59000"
                } else {
                    "61000"
                })
            }),
            limit_price: matches!(
                kind,
                ProtectiveKind::StopLimit | ProtectiveKind::TakeProfitLimit
            )
            .then(|| d("59000")),
            tif: TimeInForce::Gtc,
            working_type: WorkingType::MarkPrice,
            trailing: (kind == ProtectiveKind::Trailing).then_some(Trailing::CallbackRate {
                percent: d("1"),
                activate_price: None,
            }),
            ref_price: d("60000"),
            client_id: &cid,
        };
        let place = bf::new_algo_order(r, Some(PositionMode::OneWay), &p).unwrap();
        let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
        let intent = OrderIntent::binance(c.account(), place, &fresh).unwrap();
        let out = c
            .submit_auto(
                &OpenGate,
                &NoRisk,
                AutoIntent::new(AutoPath::AttachProtective, intent).unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(out.outcome, Outcome::Accepted { .. }), "{kind:?}");
        assert!(
            c.read_binance(&bf::query_algo_order(&by_client()).unwrap())
                .await
                .is_ok()
        );
        // cancel by id (the endpoint has no symbol; the gate binds it to the rules' symbol)
        let cancel = OrderIntent::binance(
            c.account(),
            bf::cancel_algo_order(&by_client()).unwrap(),
            &fresh,
        )
        .unwrap();
        let out = c
            .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), cancel)
            .await
            .unwrap();
        assert!(
            matches!(out.outcome, Outcome::Accepted { .. }),
            "{kind:?} cancel"
        );
    }
    assert!(fake.count("POST /fapi/v1/algoOrder") == 5);
    assert!(fake.count("GET /fapi/v1/algoOrder") == 5);
    assert_eq!(fake.count("DELETE /fapi/v1/algoOrder"), 5);
}

#[tokio::test]
async fn matrix_binance_spot_send_paths() {
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::Spot, &fake);
    let t = spot();
    let r = t.lookup("BTCUSDT").unwrap();
    let cid = id("eAAAA");
    let place = bs::new_order(&bs::SpotEntry {
        rules: r,
        side: Side::Buy,
        qty: d("0.1"),
        exec: Exec::Market,
        ref_price: Some(d("60000")),
        client_id: &cid,
    })
    .unwrap();
    binance_row(
        &c,
        &fake,
        &t,
        "BTCUSDT",
        place,
        bs::cancel_order(r, &by_client()).unwrap(),
        bs::query_order(r, &by_client()).unwrap(),
        ["/api/v3/order"; 3],
    )
    .await;
    let limit = bs::new_order(&bs::SpotEntry {
        rules: r,
        side: Side::Buy,
        qty: d("0.1"),
        exec: Exec::Limit {
            price: d("60000"),
            tif: TimeInForce::Gtc,
        },
        ref_price: Some(d("60000")),
        client_id: &cid,
    })
    .unwrap();
    binance_row(
        &c,
        &fake,
        &t,
        "BTCUSDT",
        limit,
        bs::cancel_order(r, &by_client()).unwrap(),
        bs::query_order(r, &by_client()).unwrap(),
        ["/api/v3/order"; 3],
    )
    .await;
    for kind in [
        ProtectiveKind::StopMarket,
        ProtectiveKind::TakeProfitMarket,
        ProtectiveKind::StopLimit,
        ProtectiveKind::TakeProfitLimit,
        ProtectiveKind::Trailing,
    ] {
        let p = Protective {
            kind,
            dir: Dir::Long,
            size: Size::Qty(d("0.1")),
            trigger_price: (kind != ProtectiveKind::Trailing).then(|| {
                d(if kind.is_stop_side() {
                    "59000"
                } else {
                    "61000"
                })
            }),
            limit_price: matches!(
                kind,
                ProtectiveKind::StopLimit | ProtectiveKind::TakeProfitLimit
            )
            .then(|| d("59000")),
            tif: TimeInForce::Gtc,
            working_type: WorkingType::ContractPrice,
            trailing: (kind == ProtectiveKind::Trailing).then_some(Trailing::DeltaBips(100)),
            ref_price: d("60000"),
            client_id: &cid,
        };
        binance_row(
            &c,
            &fake,
            &t,
            "BTCUSDT",
            bs::new_protective_order(r, &p, &d("0.1")).unwrap(),
            bs::cancel_order(r, &by_client()).unwrap(),
            bs::query_order(r, &by_client()).unwrap(),
            ["/api/v3/order"; 3],
        )
        .await;
    }
    let (list, tp, sl, entry) = (id("lAAAA"), id("tAAAA"), id("sAAAA"), id("eBBBB"));
    let exits = bs::SpotExits {
        take_profit_price: d("61000"),
        stop_trigger: d("59000"),
        stop_limit_price: None,
        stop_tif: TimeInForce::Gtc,
        take_profit_id: &tp,
        stop_id: &sl,
    };
    let oco = bs::new_oco(
        &bs::SpotOco {
            rules: r,
            qty: d("0.1"),
            ref_price: d("60000"),
            exits,
            list_id: &list,
        },
        &d("0.1"),
    )
    .unwrap();
    binance_row(
        &c,
        &fake,
        &t,
        "BTCUSDT",
        oco,
        bs::cancel_order_list(r, &by_client()).unwrap(),
        bs::query_order_list(&by_client()).unwrap(),
        [
            "/api/v3/orderList/oco",
            "/api/v3/orderList",
            "/api/v3/orderList",
        ],
    )
    .await;
    let otoco = bs::new_otoco(
        &bs::SpotOtoco {
            rules: r,
            qty: d("0.1"),
            entry_price: d("60000"),
            entry_tif: TimeInForce::Gtc,
            exits,
            list_id: &list,
            entry_id: &entry,
        },
        10,
    )
    .unwrap();
    binance_row(
        &c,
        &fake,
        &t,
        "BTCUSDT",
        otoco,
        bs::cancel_order_list(r, &by_client()).unwrap(),
        bs::query_order_list(&by_client()).unwrap(),
        [
            "/api/v3/orderList/otoco",
            "/api/v3/orderList",
            "/api/v3/orderList",
        ],
    )
    .await;
}

async fn okx_submit(c: &TradeClient, t: &RulesTable, sym: &str, req: OkxRequest) -> Outcome {
    let fresh = t.fresh_lookup(sym, NOW, AGE).unwrap();
    let intent = OrderIntent::okx(c.account(), req, &fresh).unwrap();
    c.submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), intent)
        .await
        .unwrap()
        .outcome
}

#[tokio::test]
async fn matrix_okx_send_paths() {
    let fake = serve(gen_okx);
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let st = okx_swap_table();
    let sr = st.lookup("BTC-USDT-SWAP").unwrap();
    let cid = id("eAAAA");
    let target = |action| OkxTarget::Swap {
        mode: Some(PositionMode::OneWay),
        margin: MarginMode::Cross,
        action,
    };
    // entries
    for exec in [
        Exec::Limit {
            price: d("60000"),
            tif: TimeInForce::Gtc,
        },
        Exec::Market,
    ] {
        let place = okx::place_order(&okx::OkxEntry {
            rules: sr,
            target: target(Action::Open(Dir::Long)),
            qty: d("2"),
            exec,
            ref_price: Some(d("60000")),
            client_id: &cid,
            attach: None,
        })
        .unwrap();
        assert!(matches!(
            okx_submit(&c, &st, "BTC-USDT-SWAP", place).await,
            Outcome::Accepted { .. }
        ));
    }
    assert!(matches!(
        okx_submit(
            &c,
            &st,
            "BTC-USDT-SWAP",
            okx::cancel_order(sr, &by_client()).unwrap()
        )
        .await,
        Outcome::Accepted { .. }
    ));
    assert!(
        !c.read_okx(&okx::query_order(sr, &by_client()).unwrap())
            .await
            .unwrap()
            .is_empty()
    );
    // algo types
    for (ord_type, tp, sl) in [
        (OkxAlgoType::Conditional, None, Some("59000")),
        (OkxAlgoType::Conditional, Some("61000"), None),
        (OkxAlgoType::Oco, Some("61000"), Some("59000")),
    ] {
        let place = okx::place_algo(&OkxProtective {
            rules: sr,
            target: target(Action::Close(Dir::Long)),
            ord_type,
            size: Size::ClosePosition,
            tp_trigger: tp.map(d),
            sl_trigger: sl.map(d),
            trigger_type: TriggerPxType::Last,
            ref_price: d("60000"),
            client_id: &cid,
        })
        .unwrap();
        assert!(matches!(
            okx_submit(&c, &st, "BTC-USDT-SWAP", place).await,
            Outcome::Accepted { .. }
        ));
        let cancel = okx::cancel_algos(sr, ord_type, std::slice::from_ref(&by_client())).unwrap();
        assert!(matches!(
            okx_submit(&c, &st, "BTC-USDT-SWAP", cancel).await,
            Outcome::Accepted { .. }
        ));
        assert!(
            !c.read_okx(&okx::query_algo(ord_type, &by_client()).unwrap())
                .await
                .unwrap()
                .is_empty()
        );
    }
    // spot: entries, conditional and oco, cancel, query
    let cs = client(AccountVenue::Okx, AccountProduct::Spot, &fake);
    let pt = okx_spot_table();
    let pr = pt.lookup("BTC-USDT").unwrap();
    for exec in [
        Exec::Limit {
            price: d("60000"),
            tif: TimeInForce::Gtc,
        },
        Exec::Market,
    ] {
        let place = okx::place_order(&okx::OkxEntry {
            rules: pr,
            target: OkxTarget::Spot { side: Side::Buy },
            qty: d("0.1"),
            exec,
            ref_price: Some(d("60000")),
            client_id: &cid,
            attach: None,
        })
        .unwrap();
        assert!(matches!(
            okx_submit(&cs, &pt, "BTC-USDT", place).await,
            Outcome::Accepted { .. }
        ));
    }
    assert!(matches!(
        okx_submit(
            &cs,
            &pt,
            "BTC-USDT",
            okx::cancel_order(pr, &by_client()).unwrap()
        )
        .await,
        Outcome::Accepted { .. }
    ));
    assert!(
        !cs.read_okx(&okx::query_order(pr, &by_client()).unwrap())
            .await
            .unwrap()
            .is_empty()
    );
    for (ord_type, tp, sl) in [
        (OkxAlgoType::Conditional, None, Some("59000")),
        (OkxAlgoType::Oco, Some("61000"), Some("59000")),
    ] {
        let place = okx::place_algo(&OkxProtective {
            rules: pr,
            target: OkxTarget::Spot { side: Side::Sell },
            ord_type,
            size: Size::Qty(d("0.1")),
            tp_trigger: tp.map(d),
            sl_trigger: sl.map(d),
            trigger_type: TriggerPxType::Last,
            ref_price: d("60000"),
            client_id: &cid,
        })
        .unwrap();
        assert!(matches!(
            okx_submit(&cs, &pt, "BTC-USDT", place).await,
            Outcome::Accepted { .. }
        ));
        let cancel = okx::cancel_algos(pr, ord_type, std::slice::from_ref(&by_client())).unwrap();
        assert!(matches!(
            okx_submit(&cs, &pt, "BTC-USDT", cancel).await,
            Outcome::Accepted { .. }
        ));
        assert!(
            !cs.read_okx(&okx::query_algo(ord_type, &by_client()).unwrap())
                .await
                .unwrap()
                .is_empty()
        );
    }
    // close-position (flatten)
    let close = okx::close_position(
        sr,
        Some(PositionMode::OneWay),
        MarginMode::Cross,
        Dir::Long,
        &cid,
    )
    .unwrap();
    assert!(matches!(
        okx_submit(&c, &st, "BTC-USDT-SWAP", close).await,
        Outcome::Accepted { .. }
    ));
    assert_eq!(fake.count("POST /api/v5/trade/order-algo"), 5);
    assert_eq!(fake.count("POST /api/v5/trade/cancel-algos"), 5);
    assert_eq!(fake.count("POST /api/v5/trade/close-position"), 1);
    assert_eq!(fake.count("GET /api/v5/trade/order-algo"), 5);
}

// ── Unknown never resends (conditions 39, 60) ───────────────────────────

fn not_found(_m: &str, p: &str) -> (u16, String) {
    if p == "/fapi/v1/order" && _m == "POST" {
        // 200 without an id: the result cannot be read, so it is Unknown.
        (200, "{}".to_string())
    } else {
        (
            400,
            json!({"code": -2013, "msg": "Order does not exist."}).to_string(),
        )
    }
}

/// An OKX swap order (net mode, market).
fn okx_swap_order(r: &SymbolRules, action: Action, cid: &str) -> OkxRequest {
    okx::place_order(&okx::OkxEntry {
        rules: r,
        target: OkxTarget::Swap {
            mode: Some(PositionMode::OneWay),
            margin: MarginMode::Cross,
            action,
        },
        qty: d("2"),
        exec: Exec::Market,
        ref_price: Some(d("60000")),
        client_id: &id(cid),
        attach: None,
    })
    .unwrap()
}

/// An OKX server whose order placing times out (`50004`, so the result is Unknown) and whose
/// order lookup answers with `lookup(n)` (the n-th look, from 0).
fn okx_unknown_server(lookup: impl Fn(usize) -> (u16, String) + Send + Sync + 'static) -> Fake {
    let looks = AtomicUsize::new(0);
    serve(move |m, p| match (m, p) {
        ("POST", "/api/v5/trade/order") => (
            400,
            json!({"code": "50004", "msg": "timeout", "data": []}).to_string(),
        ),
        ("GET", "/api/v5/trade/order") => lookup(looks.fetch_add(1, Ordering::Relaxed)),
        _ => (404, "{}".to_string()),
    })
}

fn okx_no_such_order() -> (u16, String) {
    (
        200,
        json!({"code": "51603", "msg": "Order does not exist.", "data": []}).to_string(),
    )
}

/// Place one OKX swap entry that comes back Unknown.
async fn okx_unknown(fake: &Fake) -> (TradeClient, PendingUnknown) {
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, fake);
    let t = okx_swap_table();
    let sr = t.lookup("BTC-USDT-SWAP").unwrap();
    let fresh = t.fresh_lookup("BTC-USDT-SWAP", NOW, AGE).unwrap();
    let intent = OrderIntent::okx(
        c.account(),
        okx_swap_order(sr, Action::Open(Dir::Long), "eAAAA"),
        &fresh,
    )
    .unwrap();
    let sub = c
        .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), intent)
        .await
        .unwrap();
    assert!(
        matches!(sub.outcome, Outcome::Unknown { .. }),
        "{:?}",
        sub.outcome
    );
    (c, sub.pending.unwrap())
}

#[tokio::test]
async fn unknown_is_never_resent_and_locks_the_symbol() {
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    assert!(c.is_locked("BTC-USDT-SWAP"));
    let t = okx_swap_table();
    let sr = t.lookup("BTC-USDT-SWAP").unwrap();
    let fresh = t.fresh_lookup("BTC-USDT-SWAP", NOW, AGE).unwrap();

    // A second entry is refused while the first is unresolved, and nothing is sent.
    let again = OrderIntent::okx(
        c.account(),
        okx_swap_order(sr, Action::Open(Dir::Long), "eBBBB"),
        &fresh,
    )
    .unwrap();
    let err = c
        .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), again)
        .await
        .unwrap_err();
    assert_eq!(err, SubmitError::Locked);

    // A reducing order is still allowed, and its own Unknown adds to the lock.
    let close = OrderIntent::okx(
        c.account(),
        okx_swap_order(sr, Action::Close(Dir::Long), "eCCCC"),
        &fresh,
    )
    .unwrap();
    let close_sub = c
        .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), close)
        .await
        .unwrap();
    let posts_before = fake.count("POST /api/v5/trade/order");
    assert_eq!(
        posts_before, 2,
        "entry + reduce only; the locked entry never left"
    );

    let res = c
        .resolve_unknown(pending, &[Duration::from_millis(5); 3])
        .await;
    assert!(matches!(res, Resolution::NotPlacedPresumed), "{res:?}");
    assert_eq!(
        fake.count("POST /api/v5/trade/order"),
        posts_before,
        "no resend"
    );
    assert_eq!(
        fake.count("GET /api/v5/trade/order"),
        5,
        "one look, three retries (all inside the settle window), then one look after it"
    );
    // the reduce's own Unknown still holds the lock until it is resolved too
    assert!(c.is_locked("BTC-USDT-SWAP"));
    c.resolve_unknown(close_sub.pending.unwrap(), &[]).await;
    assert!(!c.is_locked("BTC-USDT-SWAP"));
}

#[tokio::test]
async fn unknown_found_by_lookup() {
    let polls = Arc::new(AtomicUsize::new(0));
    let p2 = Arc::clone(&polls);
    let fake = serve(move |m, p| {
        if m == "GET" && p == "/fapi/v1/order" {
            // found on the second look
            if p2.fetch_add(1, Ordering::Relaxed) >= 1 {
                return (200, json!({"orderId": 5, "status": "NEW"}).to_string());
            }
            return (400, json!({"code": -2013, "msg": "x"}).to_string());
        }
        not_found(m, p)
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let intent = OrderIntent::binance(
        c.account(),
        futures_limit(r, &id("eAAAA"), Action::Open(Dir::Long)),
        &fresh,
    )
    .unwrap();
    let sub = c
        .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), intent)
        .await
        .unwrap();
    let res = c
        .resolve_unknown(sub.pending.unwrap(), &[Duration::from_millis(5); 5])
        .await;
    assert!(matches!(res, Resolution::Found), "{res:?}");
    assert_eq!(fake.count("POST /fapi/v1/order"), 1);
    assert_eq!(fake.count("GET /fapi/v1/order"), 2);
}

// ── ClockSkew: rejected, no resend (condition 76) ───────────────────────

#[tokio::test]
async fn clock_skew_is_rejected_without_resend() {
    let fake = serve(|_, _| {
        (
            400,
            json!({"code": -1021, "msg": "Timestamp outside of the recvWindow."}).to_string(),
        )
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let intent = OrderIntent::binance(
        c.account(),
        futures_limit(r, &id("eAAAA"), Action::Open(Dir::Long)),
        &fresh,
    )
    .unwrap();
    let sub = c
        .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), intent)
        .await
        .unwrap();
    assert!(matches!(
        sub.outcome,
        Outcome::Rejected {
            kind: RejectKind::ClockSkew,
            ..
        }
    ));
    assert!(sub.pending.is_none());
    assert!(!c.is_locked("BTCUSDT"));
    assert_eq!(
        fake.count("POST /fapi/v1/order"),
        1,
        "sent once, never resent"
    );
}

#[tokio::test]
async fn okx_clock_skew_item_code_is_rejected_without_resend() {
    let fake = serve(|_, _| {
        (
            200,
            json!({"code": "1", "msg": "", "data": [{"sCode": "50102", "sMsg": "Timestamp request expired"}]})
                .to_string(),
        )
    });
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let t = okx_swap_table();
    let sr = t.lookup("BTC-USDT-SWAP").unwrap();
    let place = okx::place_order(&okx::OkxEntry {
        rules: sr,
        target: OkxTarget::Swap {
            mode: Some(PositionMode::OneWay),
            margin: MarginMode::Cross,
            action: Action::Open(Dir::Long),
        },
        qty: d("2"),
        exec: Exec::Market,
        ref_price: Some(d("60000")),
        client_id: &id("eAAAA"),
        attach: None,
    })
    .unwrap();
    let out = okx_submit(&c, &t, "BTC-USDT-SWAP", place).await;
    assert!(matches!(
        out,
        Outcome::Rejected {
            kind: RejectKind::ClockSkew,
            ..
        }
    ));
    assert_eq!(fake.count("POST /api/v5/trade/order"), 1);
}

// ── Cancel-all (condition 68) ───────────────────────────────────────────

fn open_order_body() -> String {
    json!([{"symbol": "BTCUSDT", "orderId": 1}]).to_string()
}

#[tokio::test]
async fn cancel_all_reports_leftovers() {
    // The exchange keeps returning the order after the cancel.
    let fake = serve(|m, p| match (m, p) {
        ("GET", "/fapi/v1/openOrders") => (200, open_order_body()),
        ("GET", "/fapi/v1/openAlgoOrders") => (200, "[]".to_string()),
        ("DELETE", _) => (
            200,
            json!({"code": 200, "msg": "The operation of cancel all open order is done."})
                .to_string(),
        ),
        _ => (404, "{}".to_string()),
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let rep = c
        .cancel_all(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), &t)
        .await
        .unwrap();
    assert!(!rep.is_clear());
    assert_eq!(rep.remaining.len(), 1);
    assert_eq!(rep.remaining[0].symbol, "BTCUSDT");
    assert_eq!(rep.remaining[0].kind, OpenKind::Plain);
    assert_eq!(fake.count("DELETE /fapi/v1/allOpenOrders"), 1);
    assert_eq!(fake.count("DELETE /fapi/v1/algoOpenOrders"), 1);
    // read before and after
    assert_eq!(fake.count("GET /fapi/v1/openOrders"), 2);
}

#[tokio::test]
async fn cancel_all_succeeds_only_when_the_requery_is_empty() {
    let cancelled = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::clone(&cancelled);
    let fake = serve(move |m, p| match (m, p) {
        ("GET", "/fapi/v1/openOrders") => {
            if c2.load(Ordering::Relaxed) > 0 {
                (200, "[]".to_string())
            } else {
                (200, open_order_body())
            }
        }
        ("GET", "/fapi/v1/openAlgoOrders") => (200, "[]".to_string()),
        ("DELETE", _) => {
            c2.fetch_add(1, Ordering::Relaxed);
            (200, json!({"code": 200}).to_string())
        }
        _ => (404, "{}".to_string()),
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let rep = c
        .cancel_all(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), &t)
        .await
        .unwrap();
    assert!(rep.is_clear());
    assert_eq!(rep.failed, 0);
}

// ── Gate, risk, auto paths ──────────────────────────────────────────────

#[tokio::test]
async fn auto_paths_cannot_add_exposure() {
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let acct = AccountKey::from_bytes([7; 16]);
    for path in [
        AutoPath::RiskClose,
        AutoPath::AttachProtective,
        AutoPath::CancelSibling,
    ] {
        let open = OrderIntent::binance(
            acct,
            futures_limit(r, &id("eAAAA"), Action::Open(Dir::Long)),
            &fresh,
        )
        .unwrap();
        assert_eq!(open.exposure().clone(), open.exposure());
        assert!(matches!(open.exposure(), Exposure::Adds { .. }));
        assert_eq!(
            AutoIntent::new(path, open).unwrap_err(),
            TradeError::AutoPathMustReduce
        );
        let close = OrderIntent::binance(
            acct,
            futures_limit(r, &id("eAAAA"), Action::Close(Dir::Long)),
            &fresh,
        )
        .unwrap();
        assert!(AutoIntent::new(path, close).is_ok());
        let cancel =
            OrderIntent::binance(acct, bf::cancel_order(r, &by_client()).unwrap(), &fresh).unwrap();
        assert!(AutoIntent::new(path, cancel).is_ok());
    }
}

#[tokio::test]
async fn single_order_cap_and_gate_refusals_stop_the_send() {
    struct Deny;
    impl SendGate for Deny {
        type Refusal = &'static str;
        fn check(&self, _: GateRequest) -> Result<(), &'static str> {
            Err("closed")
        }
    }
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let mk = || {
        OrderIntent::binance(
            c.account(),
            futures_limit(r, &id("eAAAA"), Action::Open(Dir::Long)),
            &fresh,
        )
        .unwrap()
    };
    // 0.002 BTC at 60000 = 120 USDT
    let cap = SingleOrderCap {
        max_notional: d("100"),
    };
    assert_eq!(
        c.submit(&OpenGate, &cap, ConfirmToken::user_confirmed(), mk())
            .await
            .unwrap_err(),
        SubmitError::Risk(RiskRefusal::SingleOrderCapExceeded)
    );
    assert_eq!(
        c.submit(&Deny, &NoRisk, ConfirmToken::user_confirmed(), mk())
            .await
            .unwrap_err(),
        SubmitError::Gate("closed")
    );
    // expired rules
    c.test_elapsed_ms.fetch_add(AGE + 1, Ordering::Relaxed);
    assert_eq!(
        c.submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), mk())
            .await
            .unwrap_err(),
        SubmitError::RulesExpired
    );
    assert_eq!(fake.all().len(), 0, "nothing was sent");
    c.test_elapsed_ms.store(0, Ordering::Relaxed);
    let cap = SingleOrderCap {
        max_notional: d("1000"),
    };
    assert!(
        c.submit(&OpenGate, &cap, ConfirmToken::user_confirmed(), mk())
            .await
            .is_ok()
    );
    assert_eq!(fake.count("POST /fapi/v1/order"), 1);
}

#[tokio::test]
async fn a_changed_proxy_stops_every_request() {
    let fake = serve(gen_binance);
    let host = format!("t{}.test", HOSTS.fetch_add(1, Ordering::Relaxed));
    let mut c = TradeClient::new(
        reqwest::Client::new(),
        AccountVenue::Binance,
        AccountEnv::Live,
        AccountProduct::UsdM,
        AccountKey::from_bytes([1; 16]),
        ApiCredentials::new("k".into(), "s".into(), None),
        ProxyState::Direct,
        Arc::new(|| ProxyState::Broken),
    );
    c.test_now_ms = Some(NOW);
    c.seam = Some(super::client::Seam {
        base: format!("http://127.0.0.1:{}", fake.port),
        limit_host: host,
    });
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let intent = OrderIntent::binance(
        c.account(),
        futures_limit(r, &id("eAAAA"), Action::Close(Dir::Long)),
        &fresh,
    )
    .unwrap();
    let out = c
        .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), intent)
        .await
        .unwrap();
    assert!(matches!(out.outcome, Outcome::NotSent(_)));
    assert_eq!(fake.all().len(), 0);
}

#[tokio::test]
async fn role_ids_of_a_flatten_are_valid_for_both_venues() {
    let cid = ClientId::generate(Role::Entry);
    assert!(cid.check(super::client_id::IdField::Okx).is_ok());
    assert!(cid.check(super::client_id::IdField::BinanceFutures).is_ok());
}

#[tokio::test]
async fn builders_mark_exposure_themselves() {
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let cid = id("eAAAA");
    // 0.002 BTC at 60000
    assert_eq!(
        futures_limit(r, &cid, Action::Open(Dir::Long)).exposure(),
        Exposure::Adds {
            notional: Some(d("120"))
        }
    );
    assert_eq!(
        futures_limit(r, &cid, Action::Close(Dir::Long)).exposure(),
        Exposure::Reduces
    );
    assert_eq!(
        bf::cancel_order(r, &by_client()).unwrap().exposure(),
        Exposure::Neutral
    );
    assert_eq!(
        bf::query_order(r, &by_client()).unwrap().exposure(),
        Exposure::Neutral
    );
    let st = spot();
    let sr = st.lookup("BTCUSDT").unwrap();
    let side = |side| {
        bs::new_order(&bs::SpotEntry {
            rules: sr,
            side,
            qty: d("0.1"),
            exec: Exec::Market,
            ref_price: Some(d("60000")),
            client_id: &cid,
        })
        .unwrap()
        .exposure()
    };
    assert!(matches!(side(Side::Buy), Exposure::Adds { .. }));
    // A plain spot sell is not reduce-only (F4): it counts as adding, so it is held back by
    // the Unknown lock and the single-order cap, and no automatic path may send it.
    assert!(matches!(side(Side::Sell), Exposure::Adds { .. }));
}

#[tokio::test]
async fn silent_exchange_leaves_the_unknown_unresolved_and_locked() {
    // Every lookup fails with a 500: the exchange never answers, so nothing may be assumed.
    let fake = serve(|m, p| {
        if m == "POST" && p == "/fapi/v1/order" {
            (200, "{}".to_string())
        } else {
            (500, "{}".to_string())
        }
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let intent = OrderIntent::binance(
        c.account(),
        futures_limit(r, &id("eAAAA"), Action::Open(Dir::Long)),
        &fresh,
    )
    .unwrap();
    let sub = c
        .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), intent)
        .await
        .unwrap();
    let res = c
        .resolve_unknown(sub.pending.unwrap(), &[Duration::from_millis(5); 2])
        .await;
    let Resolution::Unresolved(pending) = res else {
        panic!("a silent exchange must not be read as 'not placed': {res:?}");
    };
    assert!(c.is_locked("BTCUSDT"));
    assert_eq!(fake.count("POST /fapi/v1/order"), 1);
    assert_eq!(pending.client_id(), "eAAAA");
}

#[tokio::test]
async fn two_unknowns_on_one_symbol_need_two_resolutions() {
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, first) = okx_unknown(&fake).await;
    let t = okx_swap_table();
    let sr = t.lookup("BTC-USDT-SWAP").unwrap();
    let fresh = t.fresh_lookup("BTC-USDT-SWAP", NOW, AGE).unwrap();
    let close = OrderIntent::okx(
        c.account(),
        okx_swap_order(sr, Action::Close(Dir::Long), "eBBBB"),
        &fresh,
    )
    .unwrap();
    let second = c
        .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), close)
        .await
        .unwrap()
        .pending
        .unwrap();
    c.resolve_unknown(first, &[]).await;
    assert!(
        c.is_locked("BTC-USDT-SWAP"),
        "the second Unknown still holds the lock"
    );
    c.resolve_unknown(second, &[]).await;
    assert!(!c.is_locked("BTC-USDT-SWAP"));
}

#[tokio::test]
async fn query_order_is_public_and_reads_by_client_id() {
    let fake = serve(|_, p| {
        if p == "/fapi/v1/order" {
            (200, json!({"orderId": 5}).to_string())
        } else {
            (400, json!({"code": -2013, "msg": "x"}).to_string())
        }
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    assert!(
        c.query_order(
            &fresh,
            super::request::LookupKind::FuturesOrder,
            &id("eAAAA")
        )
        .await
        .unwrap()
    );
    // `-2013` is only a verified "no such order" for spot, so on USDⓈ-M it stays an error
    assert!(
        c.query_order(
            &fresh,
            super::request::LookupKind::FuturesAlgo,
            &id("eAAAA")
        )
        .await
        .is_err()
    );
    // a verified answer is Ok(false)
    let sfake = serve(|_, _| (400, json!({"code": -2013, "msg": "x"}).to_string()));
    let sc = client(AccountVenue::Binance, AccountProduct::Spot, &sfake);
    let st = spot();
    let sfresh = st.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    assert!(
        !sc.query_order(&sfresh, super::request::LookupKind::SpotOrder, &id("eAAAA"))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn flatten_closes_every_position_and_requeries() {
    let closed = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::clone(&closed);
    let fake = serve(move |m, p| match (m, p) {
        ("GET", "/fapi/v3/positionRisk") => {
            if c2.load(Ordering::Relaxed) > 0 {
                (200, "[]".to_string())
            } else {
                (
                    200,
                    json!([{"symbol": "BTCUSDT", "positionAmt": "-0.002", "positionSide": "BOTH",
                            "marginType": "cross", "leverage": "10"}])
                    .to_string(),
                )
            }
        }
        ("POST", "/fapi/v1/order") => {
            c2.fetch_add(1, Ordering::Relaxed);
            gen_binance(m, p)
        }
        _ => (404, "{}".to_string()),
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let rep = c
        .flatten(
            &OpenGate,
            &NoRisk,
            ConfirmToken::user_confirmed(),
            &t,
            MarginMode::Cross,
        )
        .await
        .unwrap();
    assert!(rep.is_clear(), "{rep:?}");
    assert_eq!(rep.sent, 1);
    assert_eq!(fake.count("POST /fapi/v1/order"), 1);
    assert_eq!(
        fake.count("GET /fapi/v3/positionRisk"),
        2,
        "read before and after"
    );
    // spot has nothing to flatten
    let sfake = serve(gen_binance);
    let sc = client(AccountVenue::Binance, AccountProduct::Spot, &sfake);
    assert!(
        sc.flatten(
            &OpenGate,
            &NoRisk,
            ConfirmToken::user_confirmed(),
            &spot(),
            MarginMode::Cross,
        )
        .await
        .is_err()
    );
    assert_eq!(sfake.all().len(), 0);
}

#[tokio::test]
async fn cancel_all_keeps_going_after_one_symbol_fails_and_still_requeries() {
    // ETHUSDT is not in the rules table: it is counted as failed, BTCUSDT is still cancelled,
    // and the final read lists what is left.
    let fake = serve(|m, p| match (m, p) {
        ("GET", "/fapi/v1/openOrders") => (
            200,
            json!([{"symbol": "ETHUSDT", "orderId": 2}, {"symbol": "BTCUSDT", "orderId": 1}])
                .to_string(),
        ),
        ("GET", "/fapi/v1/openAlgoOrders") => (200, "[]".to_string()),
        ("DELETE", _) => (200, json!({"code": 200}).to_string()),
        _ => (404, "{}".to_string()),
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let rep = c
        .cancel_all(
            &OpenGate,
            &NoRisk,
            ConfirmToken::user_confirmed(),
            &futures(),
        )
        .await
        .unwrap();
    assert_eq!(rep.failed, 1);
    assert!(!rep.is_clear());
    assert_eq!(fake.count("DELETE /fapi/v1/allOpenOrders"), 1);
    assert_eq!(fake.count("GET /fapi/v1/openOrders"), 2);
}

#[tokio::test]
async fn fetch_rules_builds_the_table_and_loads_the_spot_order_limits() {
    // Spot lists a 10-second and a daily ORDERS window, no per-minute one.
    let mut info = spot_info();
    info["rateLimits"] = json!([
        {"rateLimitType": "ORDERS", "interval": "SECOND", "intervalNum": 10, "limit": 50},
        {"rateLimitType": "ORDERS", "interval": "DAY", "intervalNum": 1, "limit": 160000}
    ]);
    let fake = serve(move |m, p| match (m, p) {
        ("GET", "/api/v3/exchangeInfo") => (200, info.to_string()),
        _ => gen_binance(m, p),
    });
    let c = client_with(AccountVenue::Binance, AccountProduct::Spot, &fake, false);
    let t = spot();
    let r = t.lookup("BTCUSDT").unwrap();
    let cid = id("eAAAA");
    let place = || {
        bs::new_order(&bs::SpotEntry {
            rules: r,
            side: Side::Buy,
            qty: d("0.1"),
            exec: Exec::Market,
            ref_price: Some(d("60000")),
            client_id: &cid,
        })
        .unwrap()
    };
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    // entries wait until the limits are known
    let intent = OrderIntent::binance(c.account(), place(), &fresh).unwrap();
    let out = c
        .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), intent)
        .await
        .unwrap();
    assert!(
        matches!(out.outcome, Outcome::NotSent(_)),
        "{:?}",
        out.outcome
    );
    assert_eq!(fake.count("POST /api/v3/order"), 0);

    let table = c
        .fetch_rules(super::rules::TradeProduct::BinanceSpot)
        .await
        .unwrap();
    assert!(table.lookup("BTCUSDT").is_ok());
    let intent = OrderIntent::binance(c.account(), place(), &fresh).unwrap();
    let out = c
        .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), intent)
        .await
        .unwrap();
    assert!(
        matches!(out.outcome, Outcome::Accepted { .. }),
        "{:?}",
        out.outcome
    );
    // a table for the wrong product is refused before anything is sent
    assert!(
        c.fetch_rules(super::rules::TradeProduct::OkxSpot)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn fetch_rules_reads_okx_instruments() {
    let fake = serve(|_, p| {
        if p == "/api/v5/public/instruments" {
            (200, okx_swap().to_string())
        } else {
            (404, "{}".to_string())
        }
    });
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let table = c
        .fetch_rules(super::rules::TradeProduct::OkxSwap)
        .await
        .unwrap();
    assert!(table.lookup("BTC-USDT-SWAP").is_ok());
}

#[tokio::test]
async fn sync_clock_measures_the_server_offset() {
    let fake = serve(|_, p| {
        if p == "/fapi/v1/time" {
            let ahead = chrono::Utc::now().timestamp_millis() + 5_000;
            (200, json!({"serverTime": ahead}).to_string())
        } else {
            (404, "{}".to_string())
        }
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    assert_eq!(c.clock_offset_ms(), 0);
    let off = c.sync_clock().await.unwrap();
    assert!((4_500..=5_500).contains(&off), "{off}");
    assert_eq!(c.clock_offset_ms(), off);
}

// ── B1: what counts as "not found", and when ─────────────────────────────

/// Resolve an OKX swap Unknown whose lookups all answer `reply`.
async fn okx_resolve_with(reply: (u16, String)) -> (Resolution, TradeClient) {
    // Each scenario starts with a fresh limiter: a 429 in the previous one paused this
    // instrument's bucket (orders and their lookups share it).
    crate::rate_limit::okx_reset_for_tests();
    let fake = okx_unknown_server(move |_| reply.clone());
    let (c, pending) = okx_unknown(&fake).await;
    let res = c
        .resolve_unknown(pending, &super::unknown::DEFAULT_SCHEDULE)
        .await;
    (res, c)
}

/// Resolve a spot Unknown whose lookups all answer `reply`.
async fn spot_resolve_with(reply: (u16, String)) -> Resolution {
    let fake = serve(move |m, p| match (m, p) {
        ("POST", "/api/v3/order") => (200, "{}".to_string()),
        ("GET", "/api/v3/order") => reply.clone(),
        _ => (404, "{}".to_string()),
    });
    let c = client(AccountVenue::Binance, AccountProduct::Spot, &fake);
    let t = spot();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let place = bs::new_order(&bs::SpotEntry {
        rules: r,
        side: Side::Buy,
        qty: d("0.1"),
        exec: Exec::Market,
        ref_price: Some(d("60000")),
        client_id: &id("eAAAA"),
    })
    .unwrap();
    let intent = OrderIntent::binance(c.account(), place, &fresh).unwrap();
    let sub = c
        .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), intent)
        .await
        .unwrap();
    assert!(matches!(sub.outcome, Outcome::Unknown { .. }));
    c.resolve_unknown(sub.pending.unwrap(), &super::unknown::DEFAULT_SCHEDULE)
        .await
}

#[tokio::test]
async fn only_verified_codes_mean_no_such_order() {
    let okx = |status: u16, code: &str| {
        (
            status,
            json!({"code": code, "msg": "x", "data": []}).to_string(),
        )
    };
    // OKX: the verified code and an empty answer are "not found"
    for reply in [
        okx(200, "51603"),
        (200, json!({"code": "0", "msg": "", "data": []}).to_string()),
    ] {
        let (res, c) = okx_resolve_with(reply.clone()).await;
        assert!(
            matches!(res, Resolution::NotPlacedPresumed),
            "{reply:?}: {res:?}"
        );
        assert!(!c.is_locked("BTC-USDT-SWAP"));
    }
    // OKX: everything else says nothing about the order
    for (status, code) in [
        (400, "50004"), // the request timed out: "does not mean it failed or succeeded"
        (200, "50011"), // rate limit
        (429, "50011"),
        (200, "50013"),
        (200, "50007"),
        (200, "50009"),
        (401, "50101"),
        (200, "51000"),
        (500, "50000"),
    ] {
        let (res, c) = okx_resolve_with(okx(status, code)).await;
        assert!(
            matches!(res, Resolution::Unresolved(_)),
            "OKX {status} {code} must not unlock: {res:?}"
        );
        assert!(c.is_locked("BTC-USDT-SWAP"));
    }
    // Binance spot: -2013 only
    let binance = |status: u16, code: i64| (status, json!({"code": code, "msg": "x"}).to_string());
    let res = spot_resolve_with(binance(400, -2013)).await;
    assert!(matches!(res, Resolution::NotPlacedPresumed), "{res:?}");
    for (status, code) in [
        (401, -2015),
        (400, -1001),
        (503, -1008),
        (400, -1008),
        (400, -1022),
        (400, -2011),
        (429, -1003),
        (400, -1002),
    ] {
        let res = spot_resolve_with(binance(status, code)).await;
        assert!(
            matches!(res, Resolution::Unresolved(_)),
            "spot {status} {code} must not unlock: {res:?}"
        );
    }
    // USDⓈ-M: its not-found code is unverified, so nothing is "not found" yet
    let fake = serve(not_found);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let intent = OrderIntent::binance(
        c.account(),
        futures_limit(r, &id("eAAAA"), Action::Open(Dir::Long)),
        &fresh,
    )
    .unwrap();
    let sub = c
        .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), intent)
        .await
        .unwrap();
    let res = c
        .resolve_unknown(sub.pending.unwrap(), &super::unknown::DEFAULT_SCHEDULE)
        .await;
    assert!(matches!(res, Resolution::Unresolved(_)), "{res:?}");
    assert!(c.is_locked("BTCUSDT"));
}

#[tokio::test]
async fn an_early_not_found_followed_by_silence_stays_unresolved() {
    // The first look (inside the recvWindow) says "no such order", every later one times out.
    let fake = okx_unknown_server(|n| {
        if n == 0 {
            okx_no_such_order()
        } else {
            (
                400,
                json!({"code": "50004", "msg": "timeout", "data": []}).to_string(),
            )
        }
    });
    let (c, pending) = okx_unknown(&fake).await;
    let res = c
        .resolve_unknown(pending, &super::unknown::DEFAULT_SCHEDULE)
        .await;
    assert!(matches!(res, Resolution::Unresolved(_)), "{res:?}");
    assert!(c.is_locked("BTC-USDT-SWAP"));
}

#[tokio::test]
async fn an_empty_schedule_cannot_settle_on_one_early_look() {
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    let res = c.resolve_unknown(pending, &[]).await;
    assert!(matches!(res, Resolution::NotPlacedPresumed), "{res:?}");
    assert_eq!(
        fake.count("GET /api/v5/trade/order"),
        2,
        "the first look is too early (inside recvWindow + buffer): one more after it"
    );
    // The extra look is the last answered one and is late enough; if it times out instead the
    // early "not found" is not enough.
    let fake = okx_unknown_server(|n| {
        if n == 0 {
            okx_no_such_order()
        } else {
            (500, "{}".to_string())
        }
    });
    let (c, pending) = okx_unknown(&fake).await;
    let res = c.resolve_unknown(pending, &[]).await;
    assert!(matches!(res, Resolution::Unresolved(_)), "{res:?}");
}

#[tokio::test]
async fn a_late_first_look_is_enough_but_an_early_one_is_not() {
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    // time has passed since the send, past the OKX floor
    c.test_elapsed_ms.fetch_add(
        super::unknown::OKX_SETTLE_FLOOR_MS + 1_000,
        Ordering::Relaxed,
    );
    let res = c.resolve_unknown(pending, &[]).await;
    assert!(matches!(res, Resolution::NotPlacedPresumed), "{res:?}");
    assert_eq!(fake.count("GET /api/v5/trade/order"), 1);
}

#[tokio::test]
async fn okx_needs_its_own_longer_floor_than_recv_window_plus_buffer() {
    // 16 s is past Binance's recvWindow + buffer, but not past the OKX floor: the first look
    // is too early, so one more is made at the floor.
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    c.test_elapsed_ms.fetch_add(16_000, Ordering::Relaxed);
    let res = c.resolve_unknown(pending, &[]).await;
    assert!(matches!(res, Resolution::NotPlacedPresumed), "{res:?}");
    assert_eq!(fake.count("GET /api/v5/trade/order"), 2);
    // And if only the early look was answered, it does not count.
    let fake = okx_unknown_server(|n| {
        if n == 0 {
            okx_no_such_order()
        } else {
            (500, "{}".to_string())
        }
    });
    let (c, pending) = okx_unknown(&fake).await;
    c.test_elapsed_ms.fetch_add(16_000, Ordering::Relaxed);
    let res = c.resolve_unknown(pending, &[]).await;
    assert!(matches!(res, Resolution::Unresolved(_)), "{res:?}");
}

#[test]
fn an_entry_always_locks_on_unknown_even_without_a_lookup() {
    use super::client::lock_on_unknown;
    // (is_unknown, adds, has_lookup)
    assert!(lock_on_unknown(true, true, true));
    assert!(
        lock_on_unknown(true, true, false),
        "an entry without lookup still locks"
    );
    assert!(lock_on_unknown(true, false, true));
    assert!(
        !lock_on_unknown(true, false, false),
        "a cancel has nothing to resolve"
    );
    assert!(!lock_on_unknown(false, true, true));
    assert!(!lock_on_unknown(false, true, false));
}

#[test]
fn okx_order_queries_are_counted_per_inst_id() {
    use super::client::{okx_endpoint, okx_inst_id};
    use crate::rate_limit::OkxEndpoint;
    let t = okx_swap_table();
    let sr = t.lookup("BTC-USDT-SWAP").unwrap();
    let get = okx::query_order(sr, &by_client()).unwrap();
    assert_eq!(okx_inst_id(&get), Some("BTC-USDT-SWAP"));
    assert_eq!(
        okx_endpoint(&get),
        OkxEndpoint::Order("BTC-USDT-SWAP".to_string())
    );
    let post = okx_swap_order(sr, Action::Open(Dir::Long), "eAAAA");
    assert_eq!(
        okx_endpoint(&post),
        OkxEndpoint::Order("BTC-USDT-SWAP".to_string())
    );
}

#[tokio::test]
async fn the_monotonic_clock_never_goes_back() {
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let a = c.mono_ms();
    let b = c.mono_ms();
    assert!(b >= a);
}

// ── B2: the gate judges the account and environment that will sign ──────

struct Recorder(Mutex<Vec<GateRequest>>);
impl SendGate for Recorder {
    type Refusal = ();
    fn check(&self, r: GateRequest) -> Result<(), ()> {
        self.0.lock().unwrap().push(r);
        Ok(())
    }
}

/// Like the app's gate for one Testnet account: refuses anything signed for another
/// environment, venue or product.
struct TestnetOnly;
impl SendGate for TestnetOnly {
    type Refusal = &'static str;
    fn check(&self, r: GateRequest) -> Result<(), &'static str> {
        if (r.venue, r.env, r.product)
            == (
                AccountVenue::Binance,
                AccountEnv::Testnet,
                AccountProduct::UsdM,
            )
        {
            Ok(())
        } else {
            Err("not the testnet account")
        }
    }
}

#[tokio::test]
async fn the_gate_sees_what_the_client_will_sign_for() {
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let mk = |cid: &str| {
        OrderIntent::binance(
            c.account(),
            futures_limit(r, &id(cid), Action::Open(Dir::Long)),
            &fresh,
        )
        .unwrap()
    };
    let rec = Recorder(Mutex::new(Vec::new()));
    c.submit(&rec, &NoRisk, ConfirmToken::user_confirmed(), mk("eAAAA"))
        .await
        .unwrap();
    let seen = rec.0.lock().unwrap()[0];
    assert_eq!(
        (seen.account, seen.venue, seen.env, seen.product),
        (
            c.account(),
            AccountVenue::Binance,
            AccountEnv::Live,
            AccountProduct::UsdM
        )
    );
    // A gate that only opens for Testnet does not let this Live client send.
    let err = c
        .submit(
            &TestnetOnly,
            &NoRisk,
            ConfirmToken::user_confirmed(),
            mk("eBBBB"),
        )
        .await
        .unwrap_err();
    assert_eq!(err, SubmitError::Gate("not the testnet account"));
    assert_eq!(
        fake.count("POST /fapi/v1/order"),
        1,
        "only the first order left"
    );
}

#[tokio::test]
async fn an_intent_for_another_account_or_product_is_not_sent() {
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let other = AccountKey::from_bytes([0xEE; 16]);
    let intent = OrderIntent::binance(
        other,
        futures_limit(r, &id("eAAAA"), Action::Open(Dir::Long)),
        &fresh,
    )
    .unwrap();
    let rec = Recorder(Mutex::new(Vec::new()));
    let err = c
        .submit(&rec, &NoRisk, ConfirmToken::user_confirmed(), intent)
        .await
        .unwrap_err();
    assert_eq!(err, SubmitError::AccountMismatch);
    assert!(
        rec.0.lock().unwrap().is_empty(),
        "the gate was not even asked"
    );
    // the same for an intent built with spot rules, handed to a USDⓈ-M client
    let st = spot();
    let sr = st.lookup("BTCUSDT").unwrap();
    let sfresh = st.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let place = bs::new_order(&bs::SpotEntry {
        rules: sr,
        side: Side::Buy,
        qty: d("0.1"),
        exec: Exec::Market,
        ref_price: Some(d("60000")),
        client_id: &id("eAAAA"),
    })
    .unwrap();
    let intent = OrderIntent::binance(c.account(), place, &sfresh).unwrap();
    let err = c
        .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), intent)
        .await
        .unwrap_err();
    assert_eq!(err, SubmitError::Malformed(TradeError::ProductMismatch));
    assert_eq!(fake.all().len(), 0, "nothing was sent");
}

// ── F1, F2, F4, F5, F11 ──────────────────────────────────────────────────

#[test]
fn only_placing_requests_have_a_lookup() {
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let cid = id("eAAAA");
    assert!(
        futures_limit(r, &cid, Action::Open(Dir::Long))
            .lookup()
            .is_some()
    );
    assert!(
        bf::cancel_order(r, &by_client())
            .unwrap()
            .lookup()
            .is_none()
    );
    assert!(
        bf::cancel_algo_order(&by_client())
            .unwrap()
            .lookup()
            .is_none()
    );
}

#[tokio::test]
async fn an_unknown_cancel_has_nothing_to_resolve_by_lookup() {
    let fake = serve(|_, _| (200, "{}".to_string()));
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let intent = OrderIntent::binance(
        c.account(),
        bf::cancel_order(r, &by_client()).unwrap(),
        &fresh,
    )
    .unwrap();
    let sub = c
        .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), intent)
        .await
        .unwrap();
    assert!(sub.pending.is_none(), "{:?}", sub.outcome);
    assert!(!c.is_locked("BTCUSDT"));
}

#[tokio::test]
async fn the_lock_outlives_the_client_and_is_taken_before_sending() {
    // F2: a second client for the same account (the Keychain item is re-read, so a client is
    // built per order) still sees the unresolved Unknown.
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, _pending) = okx_unknown(&fake).await;
    let mut c2 = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    c2.account = c.account();
    let t = okx_swap_table();
    let sr = t.lookup("BTC-USDT-SWAP").unwrap();
    let fresh = t.fresh_lookup("BTC-USDT-SWAP", NOW, AGE).unwrap();
    let intent = OrderIntent::okx(
        c2.account(),
        okx_swap_order(sr, Action::Open(Dir::Long), "eBBBB"),
        &fresh,
    )
    .unwrap();
    let err = c2
        .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), intent)
        .await
        .unwrap_err();
    assert_eq!(err, SubmitError::Locked);
    drop(c);
    assert!(c2.is_locked("BTC-USDT-SWAP"));

    // Two concurrent entries on a fresh symbol: the second is refused while the first is in
    // flight, even though the first has not returned yet.
    let slow = serve(|m, p| {
        if m == "POST" {
            std::thread::sleep(Duration::from_millis(300));
        }
        gen_binance(m, p)
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &slow);
    let ft = futures();
    let r = ft.lookup("BTCUSDT").unwrap();
    let ffresh = ft.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let mk = |cid: &str| {
        OrderIntent::binance(
            c.account(),
            futures_limit(r, &id(cid), Action::Open(Dir::Long)),
            &ffresh,
        )
        .unwrap()
    };
    let (a, b) = tokio::join!(
        c.submit(
            &OpenGate,
            &NoRisk,
            ConfirmToken::user_confirmed(),
            mk("eAAAA")
        ),
        async {
            // let the first one reach the transport
            tokio::time::sleep(Duration::from_millis(50)).await;
            c.submit(
                &OpenGate,
                &NoRisk,
                ConfirmToken::user_confirmed(),
                mk("eBBBB"),
            )
            .await
        }
    );
    assert!(a.is_ok());
    assert_eq!(b.unwrap_err(), SubmitError::Locked);
    assert_eq!(slow.count("POST /fapi/v1/order"), 1);
    assert!(
        !c.is_locked("BTCUSDT"),
        "a normal result releases the symbol"
    );
}

#[test]
fn a_plain_okx_spot_sell_counts_as_adding() {
    let pt = okx_spot_table();
    let pr = pt.lookup("BTC-USDT").unwrap();
    let sell = okx::place_order(&okx::OkxEntry {
        rules: pr,
        target: OkxTarget::Spot { side: Side::Sell },
        qty: d("0.1"),
        exec: Exec::Limit {
            price: d("60000"),
            tif: TimeInForce::Gtc,
        },
        ref_price: Some(d("60000")),
        client_id: &id("eAAAA"),
        attach: None,
    })
    .unwrap();
    assert!(matches!(sell.exposure(), Exposure::Adds { .. }));
}

#[tokio::test]
async fn the_rules_age_is_judged_by_the_clients_clock() {
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    assert!(c.fresh(&t, "BTCUSDT").is_ok());
    c.test_elapsed_ms
        .fetch_add(super::client::MAX_RULES_AGE_MS + 1, Ordering::Relaxed);
    assert!(c.fresh(&t, "BTCUSDT").is_err());
}

#[tokio::test]
async fn cancel_all_still_reports_when_the_final_read_fails() {
    let reads = Arc::new(AtomicUsize::new(0));
    let r2 = Arc::clone(&reads);
    let fake = serve(move |m, p| match (m, p) {
        ("GET", "/fapi/v1/openOrders") => {
            if r2.fetch_add(1, Ordering::Relaxed) == 0 {
                (200, open_order_body())
            } else {
                (500, "{}".to_string())
            }
        }
        ("GET", "/fapi/v1/openAlgoOrders") => (200, "[]".to_string()),
        ("DELETE", _) => (200, json!({"code": 200}).to_string()),
        _ => (404, "{}".to_string()),
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let rep = c
        .cancel_all(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), &t)
        .await
        .unwrap();
    assert!(rep.reread_failed);
    assert!(!rep.is_clear(), "an unreadable account is not a clear one");
    assert_eq!(rep.sent, 2);
}

#[tokio::test]
async fn a_418_set_on_another_thread_does_not_stop_this_send() {
    // The ban state is per test thread, so a 418 simulated by a parallel test (the cause of
    // an earlier flaky `cancel_all` test) cannot reach a send here.
    std::thread::spawn(|| {
        crate::adapter::limiter::binance_set_ban(
            AccountEnv::Live,
            std::time::Instant::now(),
            Some(Duration::from_secs(60)),
        );
    })
    .join()
    .unwrap();
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let intent = OrderIntent::binance(
        c.account(),
        futures_limit(r, &id("eAAAA"), Action::Open(Dir::Long)),
        &fresh,
    )
    .unwrap();
    let sub = c
        .submit(&OpenGate, &NoRisk, ConfirmToken::user_confirmed(), intent)
        .await
        .unwrap();
    assert!(
        matches!(sub.outcome, Outcome::Accepted { .. }),
        "{:?}",
        sub.outcome
    );
}
