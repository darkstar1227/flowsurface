//! Send-layer tests against a local fake server (no real exchange is contacted). Each test
//! uses its own limiter host name so limiter state is not shared between tests.

use super::binance_futures as bf;
use super::binance_spot as bs;
use super::client::{OpenKind, TradeClient};
use super::client_id::{ClientId, Role};
use super::gate::{
    AutoIntent, AutoPath, ConfirmToken, GateRequest, OrderIntent, RiskHook, RiskRefusal, RiskView,
    SendGate, SubmitError,
};
use super::okx::{self, OkxAlgoType, OkxProtective, OkxTarget, TriggerPxType};
use super::outcome::{Outcome, RejectKind};
use super::request::{BinanceRequest, Exposure, OkxRequest};
use super::risk::{EquityBasis, EquityTracker, RiskLimits};
use super::rules::fixtures::*;
use super::rules::{RulesTable, SymbolRules};
use super::stream::stream_state_for;
use super::types::*;
use super::unknown::{PendingUnknown, Resolution, UnknownRecord};
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

type Handler = dyn Fn(&str, &str, &str) -> (u16, String) + Send + Sync;

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
    serve_q(move |m, p, _| handler(m, p))
}

/// Like [`serve`], and the handler also gets the raw query string.
fn serve_q(handler: impl Fn(&str, &str, &str) -> (u16, String) + Send + Sync + 'static) -> Fake {
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
                let query = target.split_once('?').map_or("", |(_, q)| q).to_string();
                log.lock().unwrap().push(format!("{method} {path}"));
                let (status, body) = handler(&method, &path, &query);
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
    // Most tests are about the send itself, so the stream counts as synchronised; the
    // `stream_gate_*` tests below use the real default.
    super::stream::status::set_for_test(c.account(), super::stream::StreamState::Connected);
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

fn set_stream(c: &TradeClient, st: super::stream::StreamState) {
    super::stream::status::set_for_test(c.account(), st);
}

struct NoJournal;
impl super::journal::UnknownJournal for NoJournal {
    fn record(&self, _: &UnknownRecord) -> Result<(), super::journal::JournalError> {
        Ok(())
    }
    fn clear(&self, _: AccountKey, _: &str) -> Result<(), super::journal::JournalError> {
        Ok(())
    }
}

/// Records what the send path writes ahead, so tests can see it; `fail` makes `record` fail.
#[derive(Default)]
struct TestJournal {
    records: std::sync::Mutex<Vec<UnknownRecord>>,
    cleared: std::sync::Mutex<Vec<String>>,
    fail: std::sync::atomic::AtomicBool,
}
impl super::journal::UnknownJournal for TestJournal {
    fn record(&self, r: &UnknownRecord) -> Result<(), super::journal::JournalError> {
        if self.fail.load(Ordering::Relaxed) {
            return Err(super::journal::JournalError);
        }
        self.records.lock().unwrap().push(r.clone());
        Ok(())
    }
    fn clear(&self, _: AccountKey, id: &str) -> Result<(), super::journal::JournalError> {
        self.cleared.lock().unwrap().push(id.to_string());
        self.records.lock().unwrap().retain(|r| r.client_id != id);
        Ok(())
    }
}

struct NoRisk;
impl super::gate::sealed::Sealed for NoRisk {}
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

fn dup(c: &ClientId) -> ClientId {
    id(c.as_str())
}

fn by_client() -> OrderRef {
    OrderRef::Client(id("eAAAA"))
}

fn futures_limit(rules: &SymbolRules, cid: &ClientId, action: Action) -> BinanceRequest {
    bf::new_order(bf::FuturesEntry {
        rules,
        mode: Some(PositionMode::OneWay),
        action,
        qty: d("0.002"),
        exec: Exec::Limit {
            price: d("60000"),
            tif: TimeInForce::Gtc,
        },
        ref_price: Some(d("60000")),
        client_id: dup(cid),
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
            .submit(
                &OpenGate,
                &NoRisk,
                &NoJournal,
                ConfirmToken::user_confirmed(),
                intent,
            )
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
    let market = bf::new_order(bf::FuturesEntry {
        rules: r,
        mode: Some(PositionMode::OneWay),
        action: Action::Close(Dir::Long),
        qty: d("0.002"),
        exec: Exec::Market,
        ref_price: None,
        client_id: dup(&cid),
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
            client_id: dup(&cid),
        };
        let place = bf::new_algo_order(r, Some(PositionMode::OneWay), p.duplicate()).unwrap();
        let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
        let intent = OrderIntent::binance(c.account(), place, &fresh).unwrap();
        let out = c
            .submit_auto(
                &OpenGate,
                &NoRisk,
                &NoJournal,
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
            .submit(
                &OpenGate,
                &NoRisk,
                &NoJournal,
                ConfirmToken::user_confirmed(),
                cancel,
            )
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
    let place = bs::new_order(bs::SpotEntry {
        rules: r,
        side: Side::Buy,
        qty: d("0.1"),
        exec: Exec::Market,
        ref_price: Some(d("60000")),
        client_id: dup(&cid),
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
    let limit = bs::new_order(bs::SpotEntry {
        rules: r,
        side: Side::Buy,
        qty: d("0.1"),
        exec: Exec::Limit {
            price: d("60000"),
            tif: TimeInForce::Gtc,
        },
        ref_price: Some(d("60000")),
        client_id: dup(&cid),
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
            client_id: dup(&cid),
        };
        binance_row(
            &c,
            &fake,
            &t,
            "BTCUSDT",
            bs::new_protective_order(r, p.duplicate(), &d("0.1")).unwrap(),
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
        take_profit_id: dup(&tp),
        stop_id: dup(&sl),
    };
    let oco = bs::new_oco(
        bs::SpotOco {
            rules: r,
            qty: d("0.1"),
            ref_price: d("60000"),
            exits: exits.duplicate(),
            list_id: dup(&list),
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
        bs::SpotOtoco {
            rules: r,
            qty: d("0.1"),
            entry_price: d("60000"),
            entry_tif: TimeInForce::Gtc,
            exits: exits.duplicate(),
            list_id: dup(&list),
            entry_id: dup(&entry),
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
    c.submit(
        &OpenGate,
        &NoRisk,
        &NoJournal,
        ConfirmToken::user_confirmed(),
        intent,
    )
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
        let place = okx::place_order(okx::OkxEntry {
            rules: sr,
            target: target(Action::Open(Dir::Long)),
            qty: d("2"),
            exec,
            ref_price: Some(d("60000")),
            client_id: dup(&cid),
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
        let place = okx::place_algo(OkxProtective {
            rules: sr,
            target: target(Action::Close(Dir::Long)),
            ord_type,
            size: Size::ClosePosition,
            tp_trigger: tp.map(d),
            sl_trigger: sl.map(d),
            trigger_type: TriggerPxType::Last,
            ref_price: d("60000"),
            client_id: dup(&cid),
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
        let place = okx::place_order(okx::OkxEntry {
            rules: pr,
            target: OkxTarget::Spot { side: Side::Buy },
            qty: d("0.1"),
            exec,
            ref_price: Some(d("60000")),
            client_id: dup(&cid),
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
        let place = okx::place_algo(OkxProtective {
            rules: pr,
            target: OkxTarget::Spot { side: Side::Sell },
            ord_type,
            size: Size::Qty(d("0.1")),
            tp_trigger: tp.map(d),
            sl_trigger: sl.map(d),
            trigger_type: TriggerPxType::Last,
            ref_price: d("60000"),
            client_id: dup(&cid),
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
        dup(&cid),
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
    okx::place_order(okx::OkxEntry {
        rules: r,
        target: OkxTarget::Swap {
            mode: Some(PositionMode::OneWay),
            margin: MarginMode::Cross,
            action,
        },
        qty: d("2"),
        exec: Exec::Market,
        ref_price: Some(d("60000")),
        client_id: id(cid),
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
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            intent,
        )
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
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            again,
        )
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
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            close,
        )
        .await
        .unwrap();
    let posts_before = fake.count("POST /api/v5/trade/order");
    assert_eq!(
        posts_before, 2,
        "entry + reduce only; the locked entry never left"
    );

    let res = c
        .resolve_unknown_with_schedule(pending, &[Duration::from_millis(5); 3])
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
    c.resolve_unknown_with_schedule(close_sub.pending.unwrap(), &[])
        .await;
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
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            intent,
        )
        .await
        .unwrap();
    let res = c
        .resolve_unknown_with_schedule(sub.pending.unwrap(), &[Duration::from_millis(5); 5])
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
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            intent,
        )
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
    let place = okx::place_order(okx::OkxEntry {
        rules: sr,
        target: OkxTarget::Swap {
            mode: Some(PositionMode::OneWay),
            margin: MarginMode::Cross,
            action: Action::Open(Dir::Long),
        },
        qty: d("2"),
        exec: Exec::Market,
        ref_price: Some(d("60000")),
        client_id: id("eAAAA"),
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
        .cancel_all(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            &t,
        )
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
        .cancel_all(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            &t,
        )
        .await
        .unwrap();
    assert!(rep.is_clear());
    assert_eq!(rep.failed, 0);
}

// ── Gate, risk, auto paths ──────────────────────────────────────────────

/// `AutoIntent::new` refuses the risk-close path; the crate's own routine builds it.
fn auto(path: AutoPath, intent: OrderIntent) -> Result<AutoIntent, TradeError> {
    if path == AutoPath::RiskClose {
        AutoIntent::risk_close(intent)
    } else {
        AutoIntent::new(path, intent)
    }
}

#[test]
fn a_caller_cannot_claim_the_risk_close_path() {
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let close = OrderIntent::binance(
        AccountKey::from_bytes([7; 16]),
        futures_limit(r, &id("eAAAA"), Action::Close(Dir::Long)),
        &fresh,
    )
    .unwrap();
    assert_eq!(
        AutoIntent::new(AutoPath::RiskClose, close).unwrap_err(),
        TradeError::Unsupported
    );
}

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
            auto(path, open).unwrap_err(),
            TradeError::AutoPathMustReduce
        );
        let close = OrderIntent::binance(
            acct,
            futures_limit(r, &id("eAAAA"), Action::Close(Dir::Long)),
            &fresh,
        )
        .unwrap();
        assert!(auto(path, close).is_ok());
        let cancel =
            OrderIntent::binance(acct, bf::cancel_order(r, &by_client()).unwrap(), &fresh).unwrap();
        assert!(auto(path, cancel).is_ok());
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
    let tracker = |cap: &str| {
        EquityTracker::new(
            c.account(),
            EquityBasis::BinanceTotalMarginBalance,
            RiskLimits {
                max_order_notional: Some(d(cap)),
                ..RiskLimits::default()
            },
        )
    };
    let small = tracker("100");
    let cap = small.snapshot(NOW);
    assert_eq!(
        c.submit(
            &OpenGate,
            &cap,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            mk()
        )
        .await
        .unwrap_err(),
        SubmitError::Risk(RiskRefusal::SingleOrderCapExceeded)
    );
    assert_eq!(
        c.submit(
            &Deny,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            mk()
        )
        .await
        .unwrap_err(),
        SubmitError::Gate("closed")
    );
    // expired rules
    c.test_elapsed_ms.fetch_add(AGE + 1, Ordering::Relaxed);
    assert_eq!(
        c.submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            mk()
        )
        .await
        .unwrap_err(),
        SubmitError::RulesExpired
    );
    assert_eq!(fake.all().len(), 0, "nothing was sent");
    c.test_elapsed_ms.store(0, Ordering::Relaxed);
    let roomy = tracker("1000");
    let cap = roomy.snapshot(NOW);
    assert!(
        c.submit(
            &OpenGate,
            &cap,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            mk()
        )
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
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            intent,
        )
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
        bs::new_order(bs::SpotEntry {
            rules: sr,
            side,
            qty: d("0.1"),
            exec: Exec::Market,
            ref_price: Some(d("60000")),
            client_id: dup(&cid),
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
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            intent,
        )
        .await
        .unwrap();
    let res = c
        .resolve_unknown_with_schedule(sub.pending.unwrap(), &[Duration::from_millis(5); 2])
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
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            close,
        )
        .await
        .unwrap()
        .pending
        .unwrap();
    c.resolve_unknown_with_schedule(first, &[]).await;
    assert!(
        c.is_locked("BTC-USDT-SWAP"),
        "the second Unknown still holds the lock"
    );
    c.resolve_unknown_with_schedule(second, &[]).await;
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
            &NoJournal,
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
            &NoJournal,
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
            &NoJournal,
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
        bs::new_order(bs::SpotEntry {
            rules: r,
            side: Side::Buy,
            qty: d("0.1"),
            exec: Exec::Market,
            ref_price: Some(d("60000")),
            client_id: dup(&cid),
        })
        .unwrap()
    };
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    // entries wait until the limits are known
    let intent = OrderIntent::binance(c.account(), place(), &fresh).unwrap();
    let out = c
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            intent,
        )
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
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            intent,
        )
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
        .resolve_unknown_with_schedule(pending, &super::unknown::DEFAULT_SCHEDULE)
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
    let place = bs::new_order(bs::SpotEntry {
        rules: r,
        side: Side::Buy,
        qty: d("0.1"),
        exec: Exec::Market,
        ref_price: Some(d("60000")),
        client_id: id("eAAAA"),
    })
    .unwrap();
    let intent = OrderIntent::binance(c.account(), place, &fresh).unwrap();
    let sub = c
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            intent,
        )
        .await
        .unwrap();
    assert!(matches!(sub.outcome, Outcome::Unknown { .. }));
    c.resolve_unknown_with_schedule(sub.pending.unwrap(), &super::unknown::DEFAULT_SCHEDULE)
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
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            intent,
        )
        .await
        .unwrap();
    let res = c
        .resolve_unknown_with_schedule(sub.pending.unwrap(), &super::unknown::DEFAULT_SCHEDULE)
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
        .resolve_unknown_with_schedule(pending, &super::unknown::DEFAULT_SCHEDULE)
        .await;
    assert!(matches!(res, Resolution::Unresolved(_)), "{res:?}");
    assert!(c.is_locked("BTC-USDT-SWAP"));
}

#[tokio::test]
async fn an_empty_schedule_cannot_settle_on_one_early_look() {
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    let res = c.resolve_unknown_with_schedule(pending, &[]).await;
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
    let res = c.resolve_unknown_with_schedule(pending, &[]).await;
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
    let res = c.resolve_unknown_with_schedule(pending, &[]).await;
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
    let res = c.resolve_unknown_with_schedule(pending, &[]).await;
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
    let res = c.resolve_unknown_with_schedule(pending, &[]).await;
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
    c.submit(
        &rec,
        &NoRisk,
        &NoJournal,
        ConfirmToken::user_confirmed(),
        mk("eAAAA"),
    )
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
            &NoJournal,
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
        .submit(
            &rec,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            intent,
        )
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
    let place = bs::new_order(bs::SpotEntry {
        rules: sr,
        side: Side::Buy,
        qty: d("0.1"),
        exec: Exec::Market,
        ref_price: Some(d("60000")),
        client_id: id("eAAAA"),
    })
    .unwrap();
    let intent = OrderIntent::binance(c.account(), place, &sfresh).unwrap();
    let err = c
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            intent,
        )
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
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            intent,
        )
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
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            intent,
        )
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
            &NoJournal,
            ConfirmToken::user_confirmed(),
            mk("eAAAA")
        ),
        async {
            // let the first one reach the transport
            tokio::time::sleep(Duration::from_millis(50)).await;
            c.submit(
                &OpenGate,
                &NoRisk,
                &NoJournal,
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
    let sell = okx::place_order(okx::OkxEntry {
        rules: pr,
        target: OkxTarget::Spot { side: Side::Sell },
        qty: d("0.1"),
        exec: Exec::Limit {
            price: d("60000"),
            tif: TimeInForce::Gtc,
        },
        ref_price: Some(d("60000")),
        client_id: id("eAAAA"),
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
        .cancel_all(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            &t,
        )
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
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            intent,
        )
        .await
        .unwrap();
    assert!(
        matches!(sub.outcome, Outcome::Accepted { .. }),
        "{:?}",
        sub.outcome
    );
}

// ── Risk control (segment e) ────────────────────────────────────────────

fn tripped_tracker(account: AccountKey, basis: EquityBasis) -> EquityTracker {
    use super::risk::{EquityObservation, EquitySource, Observed};
    let mut t = EquityTracker::new(
        account,
        basis,
        RiskLimits {
            daily_loss_limit: Some(d("100")),
            confirm_observations: 1,
            ..RiskLimits::default()
        },
    );
    let at = |at_ms, equity: &str| EquityObservation {
        at_ms,
        equity: d(equity),
        basis,
        // The private stream can flag transfers; a REST reading cannot, so only this source
        // may start an automatic close.
        source: EquitySource::PrivateStream,
        is_transfer: false,
    };
    t.observe(at(NOW - 2, "1000"));
    assert!(matches!(
        t.observe(at(NOW - 1, "800")),
        Observed::Tripped(_)
    ));
    t
}

#[tokio::test]
async fn risk_close_cancels_everything_flattens_and_reports_after_requery() {
    let closed = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::clone(&closed);
    let fake = serve(move |m, p| {
        let done = c2.load(Ordering::Relaxed) > 0;
        match (m, p) {
            ("GET", "/fapi/v1/openOrders") | ("GET", "/fapi/v1/openAlgoOrders") => {
                if done {
                    (200, "[]".to_string())
                } else if p.ends_with("openOrders") {
                    (200, open_order_body())
                } else {
                    (200, json!([{"symbol": "BTCUSDT", "algoId": 5}]).to_string())
                }
            }
            ("GET", "/fapi/v3/positionRisk") => {
                if done {
                    (200, "[]".to_string())
                } else {
                    (
                        200,
                        json!([{"symbol": "BTCUSDT", "positionAmt": "0.002", "positionSide": "BOTH",
                                "marginType": "cross", "leverage": "10"}])
                        .to_string(),
                    )
                }
            }
            ("DELETE", _) => (200, json!({"code": 200, "msg": "done"}).to_string()),
            ("POST", "/fapi/v1/order") => {
                c2.fetch_add(1, Ordering::Relaxed);
                gen_binance(m, p)
            }
            _ => (404, "{}".to_string()),
        }
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let mut tracker = tripped_tracker(c.account(), EquityBasis::BinanceTotalMarginBalance);

    // While locked, a new entry is refused before anything is sent.
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let r = t.lookup("BTCUSDT").unwrap();
    let entry = OrderIntent::binance(
        c.account(),
        futures_limit(r, &id("eAAAA"), Action::Open(Dir::Long)),
        &fresh,
    )
    .unwrap();
    let snap = tracker.snapshot(NOW);
    assert_eq!(
        c.submit(
            &OpenGate,
            &snap,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            entry
        )
        .await
        .unwrap_err(),
        SubmitError::Risk(RiskRefusal::Locked)
    );
    assert_eq!(fake.count("POST "), 0);

    let super::risk::AutoCloseDecision::Run(grant) = tracker.begin_auto_close(NOW, true) else {
        panic!("expected the automatic close to run");
    };
    let snap = tracker.snapshot(NOW);
    let rep = c
        .auto_close(&OpenGate, &snap, &NoJournal, grant, &t, MarginMode::Cross)
        .await;
    assert!(rep.is_clear(), "{rep:?}");
    assert_eq!(fake.count("DELETE /fapi/v1/allOpenOrders"), 1);
    assert_eq!(fake.count("DELETE /fapi/v1/algoOpenOrders"), 1);
    assert_eq!(fake.count("POST /fapi/v1/order"), 1);
    // Orders and positions were read before the sends and once more after them.
    assert_eq!(fake.count("GET /fapi/v1/openOrders"), 2);
    assert_eq!(fake.count("GET /fapi/v3/positionRisk"), 2);
}

#[tokio::test]
async fn risk_close_does_not_report_success_when_a_position_remains() {
    let fake = serve(|m, p| match (m, p) {
        ("GET", "/fapi/v1/openOrders") | ("GET", "/fapi/v1/openAlgoOrders") => {
            (200, "[]".to_string())
        }
        // The exchange still shows the position after the close.
        ("GET", "/fapi/v3/positionRisk") => (
            200,
            json!([{"symbol": "BTCUSDT", "positionAmt": "0.002", "positionSide": "BOTH",
                    "marginType": "cross", "leverage": "10"}])
            .to_string(),
        ),
        ("POST", "/fapi/v1/order") => gen_binance(m, p),
        _ => (404, "{}".to_string()),
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let mut tracker = tripped_tracker(c.account(), EquityBasis::BinanceTotalMarginBalance);
    let super::risk::AutoCloseDecision::Run(grant) = tracker.begin_auto_close(NOW, true) else {
        panic!("expected the automatic close to run");
    };
    let rep = c
        .auto_close(
            &OpenGate,
            &tracker.snapshot(NOW),
            &NoJournal,
            grant,
            &futures(),
            MarginMode::Cross,
        )
        .await;
    assert!(!rep.is_clear());
    assert_eq!(rep.remaining_positions(), Some(1));
    assert!(!rep.reread_failed());
}

#[tokio::test]
async fn risk_close_for_another_account_is_refused_without_a_request() {
    let fake = serve(|_, _| (404, "{}".to_string()));
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let mut tracker = tripped_tracker(
        AccountKey::from_bytes([200; 16]),
        EquityBasis::BinanceTotalMarginBalance,
    );
    let super::risk::AutoCloseDecision::Run(grant) = tracker.begin_auto_close(NOW, true) else {
        panic!("expected the automatic close to run");
    };
    let rep = c
        .auto_close(
            &OpenGate,
            &tracker.snapshot(NOW),
            &NoJournal,
            grant,
            &futures(),
            MarginMode::Cross,
        )
        .await;
    assert!(!rep.is_clear());
    assert!(rep.reread_failed());
    assert_eq!(fake.all().len(), 0);
}

#[tokio::test]
async fn cancel_all_reads_every_okx_algo_type_and_lists_the_ones_it_cannot_cancel() {
    use super::okx::OkxAlgoType;
    let cancelled = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::clone(&cancelled);
    let fake = serve_q(move |m, p, q| match (m, p) {
        ("GET", "/api/v5/trade/orders-pending") | ("GET", "/api/v5/account/positions") => {
            (200, json!({"code": "0", "data": []}).to_string())
        }
        ("GET", "/api/v5/trade/orders-algo-pending") => {
            let row = |id: &str| json!({"code": "0", "data": [{"instId": "BTC-USDT-SWAP", "algoId": id}]});
            let body = if q.contains("ordType=conditional,oco") && c2.load(Ordering::Relaxed) == 0 {
                json!({"code": "0", "data": [{"instId": "BTC-USDT-SWAP", "algoId": "1001",
                    "ordType": "conditional"}]})
            } else if q.contains("ordType=iceberg") {
                row("1002")
            } else if q.contains("ordType=move_order_stop") {
                row("1003")
            } else {
                json!({"code": "0", "data": []})
            };
            (200, body.to_string())
        }
        ("POST", "/api/v5/trade/cancel-algos") => {
            c2.fetch_add(1, Ordering::Relaxed);
            gen_okx(m, p)
        }
        _ => (404, "{}".to_string()),
    });
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let t = okx_swap_table();
    let mut tracker = tripped_tracker(c.account(), EquityBasis::OkxTotalEq);
    let super::risk::AutoCloseDecision::Run(grant) = tracker.begin_auto_close(NOW, true) else {
        panic!("expected the automatic close to run");
    };
    let rep = c
        .auto_close(
            &OpenGate,
            &tracker.snapshot(NOW),
            &NoJournal,
            grant,
            &t,
            MarginMode::Cross,
        )
        .await;
    // Only the conditional order was cancelled; iceberg and trailing stop were not tried.
    assert_eq!(fake.count("POST /api/v5/trade/cancel-algos"), 1);
    assert!(!rep.is_clear());
    let mut left: Vec<_> = rep.uncancellable().iter().map(|o| o.id.clone()).collect();
    left.sort();
    assert_eq!(left, ["1002", "1003"]);
    assert_eq!(rep.remaining_orders(), Some(2));
    // Eight order types are read in seven requests (conditional and oco share one), in each
    // of two rounds: before the cancel and in the final read.
    assert_eq!(fake.count("GET /api/v5/trade/orders-algo-pending"), 14);
    assert_eq!(OkxAlgoType::ALL_PENDING.len(), 8);
    assert_eq!(
        OkxAlgoType::PENDING_READS
            .iter()
            .map(|g| g.len())
            .sum::<usize>(),
        8
    );
}

/// An OKX server with one open conditional order. `fail` is a substring of the query of an
/// `orders-algo-pending` read that answers with an error.
fn okx_one_algo_server(fail: &'static str) -> Fake {
    serve_q(move |m, p, q| match (m, p) {
        ("GET", "/api/v5/trade/orders-pending") => {
            (200, json!({"code": "0", "data": []}).to_string())
        }
        ("GET", "/api/v5/trade/orders-algo-pending") => {
            if !fail.is_empty() && q.contains(fail) {
                (
                    200,
                    json!({"code": "50013", "msg": "busy", "data": []}).to_string(),
                )
            } else if q.contains("ordType=conditional,oco") {
                (
                    200,
                    json!({"code": "0", "data": [{"instId": "BTC-USDT-SWAP", "algoId": "1001",
                        "ordType": "conditional"}]})
                    .to_string(),
                )
            } else {
                (200, json!({"code": "0", "data": []}).to_string())
            }
        }
        ("POST", "/api/v5/trade/cancel-algos") => gen_okx(m, p),
        _ => (404, "{}".to_string()),
    })
}

#[tokio::test]
async fn cancel_all_cancels_what_it_read_when_one_algo_type_cannot_be_read() {
    use super::okx::OkxAlgoType;
    crate::rate_limit::okx_reset_for_tests();
    let fake = okx_one_algo_server("ordType=trigger");
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let t = okx_swap_table();
    let rep = c
        .cancel_all(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            &t,
        )
        .await
        .expect("a failed algo read does not abort");
    // The conditional order that was read is cancelled; the failed type is listed.
    assert_eq!(fake.count("POST /api/v5/trade/cancel-algos"), 1);
    assert_eq!(rep.sent, 1);
    assert_eq!(rep.unknown_algo_types, [OkxAlgoType::Trigger]);
    // The final read fails on the same type, and the report is never clean.
    assert!(rep.reread_failed);
    assert!(!rep.is_clear());
}

#[tokio::test]
async fn a_report_with_an_unread_type_is_not_clean_even_when_the_final_read_is_empty() {
    use super::okx::OkxAlgoType;
    crate::rate_limit::okx_reset_for_tests();
    // The trigger read fails once (the read before the cancel) and works afterwards.
    let seen = Arc::new(AtomicUsize::new(0));
    let s2 = Arc::clone(&seen);
    let fake = serve_q(move |m, p, q| match (m, p) {
        ("GET", "/api/v5/trade/orders-pending") => {
            (200, json!({"code": "0", "data": []}).to_string())
        }
        ("GET", "/api/v5/trade/orders-algo-pending") => {
            if q.contains("ordType=trigger") && s2.fetch_add(1, Ordering::Relaxed) == 0 {
                (
                    200,
                    json!({"code": "50013", "msg": "busy", "data": []}).to_string(),
                )
            } else {
                (200, json!({"code": "0", "data": []}).to_string())
            }
        }
        _ => (404, "{}".to_string()),
    });
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let rep = c
        .cancel_all(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            &okx_swap_table(),
        )
        .await
        .unwrap();
    assert_eq!(rep.unknown_algo_types, [OkxAlgoType::Trigger]);
    assert!(!rep.reread_failed);
    assert!(rep.remaining.is_empty());
    assert!(!rep.is_clear());
}

#[tokio::test]
async fn pressing_cancel_all_twice_still_cancels_the_second_time() {
    crate::rate_limit::okx_reset_for_tests();
    let fake = okx_one_algo_server("");
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let t = okx_swap_table();
    let first = c
        .cancel_all(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            &t,
        )
        .await
        .unwrap();
    assert!(first.unknown_algo_types.is_empty());
    // 14 algo reads went out already. The second round's reads use the budget kept for
    // cancels (20 per 2 s), where the query cap (18) would have run out before the cancel.
    let second = c
        .cancel_all(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            &t,
        )
        .await
        .expect("the second round is not stopped by the read budget");
    assert_eq!(fake.count("POST /api/v5/trade/cancel-algos"), 2);
    assert_eq!(second.sent, 1);
    // Reads that did not fit are listed as unknown, never as clean.
    assert!(!second.unknown_algo_types.is_empty());
    assert!(!second.is_clear());
}

#[test]
fn conditional_and_oco_are_one_read() {
    use super::okx::OkxAlgoType;
    let req =
        super::queries::okx_pending_algos(&[OkxAlgoType::Conditional, OkxAlgoType::Oco]).unwrap();
    assert_eq!(
        req.path(),
        "/api/v5/trade/orders-algo-pending?ordType=conditional,oco"
    );
    assert!(super::queries::okx_pending_algos(&[]).is_err());
}

// ── Equity read, and the close on OKX and in hedge mode ─────────────────

#[tokio::test]
async fn equity_is_read_from_the_documented_fields_and_never_defaults_to_zero() {
    use super::risk::{EquityBasis, EquitySource};
    // Binance USDⓈ-M: totalMarginBalance of /fapi/v3/account.
    let fake = serve(|m, p| match (m, p) {
        ("GET", "/fapi/v3/account") => (
            200,
            json!({"totalMarginBalance": "126.72469206", "totalWalletBalance": "1"}).to_string(),
        ),
        _ => (404, "{}".to_string()),
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let o = c.equity().await.unwrap();
    assert_eq!(o.equity, d("126.72469206"));
    assert_eq!(o.basis, EquityBasis::BinanceTotalMarginBalance);
    assert_eq!(o.source, EquitySource::Rest);
    assert_eq!(o.at_ms, NOW, "the client's clock, not the wall clock");
    assert!(!o.is_transfer);

    // OKX: totalEq of data[0] in account/balance.
    let fake = serve(|m, p| match (m, p) {
        ("GET", "/api/v5/account/balance") => (
            200,
            json!({"code": "0", "data": [{"totalEq": "55837.43556134779", "details": []}]})
                .to_string(),
        ),
        _ => (404, "{}".to_string()),
    });
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let o = c.equity().await.unwrap();
    assert_eq!(o.equity, d("55837.43556134779"));
    assert_eq!(o.basis, EquityBasis::OkxTotalEq);

    // Spot has no source and sends nothing.
    let none = serve(|_, _| (404, "{}".to_string()));
    let c = client(AccountVenue::Binance, AccountProduct::Spot, &none);
    assert!(matches!(
        c.equity().await,
        Err(super::client::ReadError::Trade(TradeError::Unsupported))
    ));
    assert_eq!(none.all().len(), 0);
}

#[tokio::test]
async fn a_malformed_equity_body_is_a_parse_error() {
    for body in [
        json!({}),
        json!({"totalMarginBalance": ""}),
        json!({"totalMarginBalance": "abc"}),
        json!({"totalMarginBalance": 126.7}),
        json!({"totalMarginBalance": "1e3"}),
    ] {
        let b = body.to_string();
        let fake = serve(move |_, _| (200, b.clone()));
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        assert!(
            matches!(c.equity().await, Err(super::client::ReadError::Parse)),
            "{body}"
        );
    }
    for data in [
        json!([]),
        json!([{}]),
        json!([{"totalEq": ""}]),
        json!([{"totalEq": "x"}]),
    ] {
        let b = json!({"code": "0", "data": data}).to_string();
        let fake = serve(move |_, _| (200, b.clone()));
        let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
        assert!(matches!(
            c.equity().await,
            Err(super::client::ReadError::Parse)
        ));
    }
}

#[tokio::test]
async fn risk_close_flattens_an_okx_position_with_close_position() {
    let closed = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::clone(&closed);
    let fake = serve(move |m, p| match (m, p) {
        ("GET", "/api/v5/account/positions") => {
            if c2.load(Ordering::Relaxed) > 0 {
                (200, json!({"code": "0", "data": []}).to_string())
            } else {
                (
                    200,
                    json!({"code": "0", "data": [{"instId": "BTC-USDT-SWAP", "pos": "2",
                        "posSide": "net", "mgnMode": "cross", "lever": "10"}]})
                    .to_string(),
                )
            }
        }
        ("GET", "/api/v5/trade/orders-pending") | ("GET", "/api/v5/trade/orders-algo-pending") => {
            (200, json!({"code": "0", "data": []}).to_string())
        }
        ("POST", "/api/v5/trade/close-position") => {
            c2.fetch_add(1, Ordering::Relaxed);
            gen_okx(m, p)
        }
        _ => (404, "{}".to_string()),
    });
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let mut tracker = tripped_tracker(c.account(), EquityBasis::OkxTotalEq);
    let super::risk::AutoCloseDecision::Run(grant) = c.begin_auto_close(&mut tracker, true) else {
        panic!("expected the automatic close to run");
    };
    let rep = c
        .auto_close(
            &OpenGate,
            &c.risk_snapshot(&tracker),
            &NoJournal,
            grant,
            &okx_swap_table(),
            MarginMode::Cross,
        )
        .await;
    assert_eq!(fake.count("POST /api/v5/trade/close-position"), 1);
    let flatten = rep.flatten.as_ref().unwrap().as_ref().unwrap();
    assert_eq!((flatten.sent, flatten.failed), (1, 0));
    assert!(rep.is_clear(), "{rep:?}");
}

#[tokio::test]
async fn risk_close_closes_both_sides_of_a_hedge_account() {
    let closed = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::clone(&closed);
    let fake = serve(move |m, p| match (m, p) {
        ("GET", "/fapi/v3/positionRisk") => {
            if c2.load(Ordering::Relaxed) >= 2 {
                (200, "[]".to_string())
            } else {
                (
                    200,
                    json!([
                        {"symbol": "BTCUSDT", "positionAmt": "0.002", "positionSide": "LONG",
                         "marginType": "cross", "leverage": "10"},
                        {"symbol": "BTCUSDT", "positionAmt": "-0.003", "positionSide": "SHORT",
                         "marginType": "cross", "leverage": "10"}
                    ])
                    .to_string(),
                )
            }
        }
        ("GET", "/fapi/v1/openOrders") | ("GET", "/fapi/v1/openAlgoOrders") => {
            (200, "[]".to_string())
        }
        ("POST", "/fapi/v1/order") => {
            c2.fetch_add(1, Ordering::Relaxed);
            gen_binance(m, p)
        }
        _ => (404, "{}".to_string()),
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let mut tracker = tripped_tracker(c.account(), EquityBasis::BinanceTotalMarginBalance);
    let super::risk::AutoCloseDecision::Run(grant) = c.begin_auto_close(&mut tracker, true) else {
        panic!("expected the automatic close to run");
    };
    let rep = c
        .auto_close(
            &OpenGate,
            &c.risk_snapshot(&tracker),
            &NoJournal,
            grant,
            &futures(),
            MarginMode::Cross,
        )
        .await;
    assert_eq!(fake.count("POST /fapi/v1/order"), 2, "one close per side");
    let flatten = rep.flatten.as_ref().unwrap().as_ref().unwrap();
    assert_eq!((flatten.sent, flatten.failed), (2, 0));
    assert!(rep.is_clear(), "{rep:?}");
}

// ── Stream state in the gate (condition 51) ─────────────────────────────

async fn submit_futures(
    c: &TradeClient,
    action: Action,
) -> Result<super::client::Submitted, SubmitError<()>> {
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let intent = OrderIntent::binance(
        c.account(),
        futures_limit(r, &ClientId::generate(Role::Entry), action),
        &fresh,
    )
    .unwrap();
    c.submit(
        &OpenGate,
        &NoRisk,
        &NoJournal,
        ConfirmToken::user_confirmed(),
        intent,
    )
    .await
}

#[tokio::test]
async fn stream_gate_refuses_entries_unless_connected_but_lets_closes_through() {
    use super::stream::StreamState;
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    for st in [
        StreamState::Reconnecting,
        StreamState::Resyncing,
        StreamState::AuthFailed,
        StreamState::Unavailable,
    ] {
        set_stream(&c, st);
        assert!(
            matches!(
                submit_futures(&c, Action::Open(Dir::Long)).await,
                Err(SubmitError::StreamNotReady(s)) if s == st
            ),
            "{st:?}"
        );
        assert!(
            submit_futures(&c, Action::Close(Dir::Long)).await.is_ok(),
            "close must pass in {st:?}"
        );
    }
    assert_eq!(fake.count("POST /fapi/v1/order"), 4);
    set_stream(&c, StreamState::Connected);
    assert!(submit_futures(&c, Action::Open(Dir::Long)).await.is_ok());
}

#[tokio::test]
async fn stream_gate_starts_closed_for_an_unwired_client() {
    use super::stream::StreamState;
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    // A fresh client (before any stream output) is not connected.
    let fresh = TradeClient::new(
        reqwest::Client::new(),
        AccountVenue::Binance,
        AccountEnv::Live,
        AccountProduct::UsdM,
        unique_account(),
        ApiCredentials::new("k".into(), "s".into(), None),
        ProxyState::Direct,
        Arc::new(|| ProxyState::Direct),
    );
    assert_eq!(
        stream_state_for(fresh.account(), AccountVenue::Binance, AccountProduct::UsdM),
        StreamState::Reconnecting
    );
    let spot = TradeClient::new(
        reqwest::Client::new(),
        AccountVenue::Binance,
        AccountEnv::Live,
        AccountProduct::Spot,
        unique_account(),
        ApiCredentials::new("k".into(), "s".into(), None),
        ProxyState::Direct,
        Arc::new(|| ProxyState::Direct),
    );
    // Binance spot has no private stream: spot trading is off (user decision 3).
    assert_eq!(
        stream_state_for(spot.account(), AccountVenue::Binance, AccountProduct::Spot),
        StreamState::Unavailable
    );
    drop(c);
}

// ── Persisted unknowns: restore and manual release (review N2) ──────────

#[tokio::test]
async fn a_saved_unknown_is_restored_locked_and_resolved_with_the_default_schedule() {
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    let record = c.unknown_record(&pending);
    assert_eq!(record.kind, "okx_order");
    // "Restart": the lock table is dropped by resolving it here, then the record loads again.
    let t = okx_swap_table();
    let sr = t.lookup("BTC-USDT-SWAP").unwrap();
    drop(pending);
    assert!(
        c.is_locked("BTC-USDT-SWAP"),
        "dropping the value never unlocks"
    );
    let restored = c.restore_unknown(&record, sr).unwrap();
    assert!(c.is_locked("BTC-USDT-SWAP"));
    // Restoring twice does not count the same order twice.
    let _again = c.restore_unknown(&record, sr).unwrap();
    // The public entry point uses DEFAULT_SCHEDULE: 6 looks plus the wait for the settle
    // floor, never fewer than the default (the test clock makes the waits free).
    let res = c.resolve_unknown(restored, &NoJournal).await;
    assert!(matches!(res, Resolution::NotPlacedPresumed), "{res:?}");
    assert!(fake.count("GET /api/v5/trade/order") > super::unknown::DEFAULT_SCHEDULE.len());
    assert!(!c.is_locked("BTC-USDT-SWAP"));
}

#[tokio::test]
async fn restore_refuses_a_record_of_another_account_or_a_bad_one() {
    use super::unknown::RestoreError;
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    let record = c.unknown_record(&pending);
    let t = okx_swap_table();
    let sr = t.lookup("BTC-USDT-SWAP").unwrap();
    let mut other = record.clone();
    other.account = unique_account();
    assert_eq!(
        c.restore_unknown(&other, sr).unwrap_err(),
        RestoreError::WrongAccount
    );
    let mut kind = record.clone();
    kind.kind = "nonsense".into();
    assert_eq!(
        c.restore_unknown(&kind, sr).unwrap_err(),
        RestoreError::UnknownKind
    );
    let mut fut = record.clone();
    fut.kind = "futures_order".into();
    assert_eq!(
        c.restore_unknown(&fut, sr).unwrap_err(),
        RestoreError::RulesMismatch
    );
    let mut sym = record.clone();
    sym.symbol = "ETH-USDT-SWAP".into();
    assert_eq!(
        c.restore_unknown(&sym, sr).unwrap_err(),
        RestoreError::RulesMismatch
    );
    let mut bad = record;
    bad.client_id = "not an id".into();
    assert_eq!(
        c.restore_unknown(&bad, sr).unwrap_err(),
        RestoreError::BadClientId
    );
}

#[tokio::test]
async fn manual_release_needs_the_typed_phrase_and_unlocks_without_resending() {
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    let posts = fake.count("POST /api/v5/trade/order");
    let challenge = pending.begin_manual_release();
    let phrase = challenge.phrase().to_string();
    assert!(phrase.starts_with("BTC-USDT-SWAP "));
    // A wrong phrase gives the challenge back and nothing is unlocked.
    let challenge = challenge.confirm("yes").unwrap_err();
    assert!(c.is_locked("BTC-USDT-SWAP"));
    let token = challenge.confirm(&phrase).unwrap();
    let released = c.release_unknown_manually(token).unwrap();
    assert_eq!(released.symbol, "BTC-USDT-SWAP");
    assert!(!c.is_locked("BTC-USDT-SWAP"));
    assert_eq!(
        fake.count("POST /api/v5/trade/order"),
        posts,
        "nothing resent"
    );
}

#[test]
fn every_lookup_kind_name_round_trips() {
    use super::okx::OkxAlgoType;
    use super::request::LookupKind;
    use super::unknown::UnknownRecord;
    let mut kinds = vec![
        LookupKind::FuturesOrder,
        LookupKind::FuturesAlgo,
        LookupKind::SpotOrder,
        LookupKind::SpotList,
        LookupKind::OkxOrder,
    ];
    kinds.extend(OkxAlgoType::ALL_PENDING.map(LookupKind::OkxAlgo));
    for k in kinds {
        let name = UnknownRecord::kind_name(k);
        assert_eq!(UnknownRecord::parse_kind(&name), Some(k), "{name}");
    }
}

// ── Write-ahead journal (B4) ────────────────────────────────────────────

fn okx_entry_intent(c: &TradeClient, cid: &str) -> OrderIntent {
    let t = okx_swap_table();
    let sr = t.lookup("BTC-USDT-SWAP").unwrap();
    let fresh = t.fresh_lookup("BTC-USDT-SWAP", NOW, AGE).unwrap();
    OrderIntent::okx(
        c.account(),
        okx_swap_order(sr, Action::Open(Dir::Long), cid),
        &fresh,
    )
    .unwrap()
}

#[tokio::test]
async fn a_failed_journal_write_sends_nothing_and_leaves_no_lock() {
    let fake = serve(gen_okx);
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let journal = TestJournal::default();
    journal.fail.store(true, Ordering::Relaxed);
    let err = c
        .submit(
            &OpenGate,
            &NoRisk,
            &journal,
            ConfirmToken::user_confirmed(),
            okx_entry_intent(&c, "eAAAA"),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SubmitError::JournalUnavailable), "{err:?}");
    assert_eq!(
        fake.count("POST"),
        0,
        "nothing may be sent without a record"
    );
    assert!(!c.is_locked("BTC-USDT-SWAP"));
    // The in-flight mark is released too: the next try goes through once the journal works.
    journal.fail.store(false, Ordering::Relaxed);
    let out = c
        .submit(
            &OpenGate,
            &NoRisk,
            &journal,
            ConfirmToken::user_confirmed(),
            okx_entry_intent(&c, "eBBBB"),
        )
        .await
        .unwrap();
    assert!(
        matches!(out.outcome, Outcome::Accepted { .. }),
        "{:?}",
        out.outcome
    );
    assert_eq!(fake.count("POST"), 1);
}

#[tokio::test]
async fn a_known_outcome_clears_the_record_it_wrote_ahead() {
    let fake = serve(gen_okx);
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let journal = TestJournal::default();
    let out = c
        .submit(
            &OpenGate,
            &NoRisk,
            &journal,
            ConfirmToken::user_confirmed(),
            okx_entry_intent(&c, "eAAAA"),
        )
        .await
        .unwrap();
    assert!(matches!(out.outcome, Outcome::Accepted { .. }));
    assert_eq!(journal.cleared.lock().unwrap().as_slice(), ["eAAAA"]);
    assert!(journal.records.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_unknown_outcome_keeps_the_record() {
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let journal = TestJournal::default();
    let out = c
        .submit(
            &OpenGate,
            &NoRisk,
            &journal,
            ConfirmToken::user_confirmed(),
            okx_entry_intent(&c, "eAAAA"),
        )
        .await
        .unwrap();
    assert!(matches!(out.outcome, Outcome::Unknown { .. }));
    assert!(journal.cleared.lock().unwrap().is_empty());
    assert_eq!(journal.records.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_send_dropped_after_the_post_arrived_leaves_its_record() {
    // The server tells the test when the POST arrived and then never answers (until the test
    // ends); the caller's future is dropped in between. No timeout, no sleep.
    let (arrived_tx, arrived_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let arrived_tx = Mutex::new(arrived_tx);
    let release_rx = Mutex::new(release_rx);
    let fake = serve(move |_, _| {
        let _ = arrived_tx.lock().unwrap().send(());
        let _ = release_rx.lock().unwrap().recv();
        (200, "{}".to_string())
    });
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let journal = TestJournal::default();
    {
        let send = c.submit(
            &OpenGate,
            &NoRisk,
            &journal,
            ConfirmToken::user_confirmed(),
            okx_entry_intent(&c, "eAAAA"),
        );
        tokio::pin!(send);
        let arrived = tokio::task::spawn_blocking(move || arrived_rx.recv());
        tokio::select! {
            _ = &mut send => panic!("the server never answers"),
            got = arrived => assert!(got.unwrap().is_ok()),
        }
    }
    let _ = release_tx.send(());
    assert_eq!(journal.records.lock().unwrap().len(), 1, "the record stays");
    assert!(journal.cleared.lock().unwrap().is_empty());
    assert!(c.is_locked("BTC-USDT-SWAP"), "the dropped send still locks");
}

// ── Stream registry seen through real clients (B3) ──────────────────────

#[tokio::test]
async fn two_clients_of_one_account_share_the_registry_and_a_retired_stream_refuses() {
    use super::stream::status;
    let fake = serve(gen_okx);
    let a = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let account = a.account();
    // A second client built later for the same account (the app builds one per send).
    let mut b = TradeClient::new(
        reqwest::Client::new(),
        AccountVenue::Okx,
        AccountEnv::Live,
        AccountProduct::UsdM,
        account,
        ApiCredentials::new("k".into(), "s".into(), Some("p".into())),
        ProxyState::Direct,
        Arc::new(|| ProxyState::Direct),
    );
    b.test_now_ms = Some(NOW);
    b.seam = a.seam.clone();
    // Replace the test entry with a registered stream, the way `run` does.
    status::retire(account, 0);
    let token = status::register(account);
    status::publish(account, token, super::stream::StreamState::Connected);
    for (client, cid) in [(&a, "eAAAA"), (&b, "eBBBB")] {
        let out = client
            .submit(
                &OpenGate,
                &NoRisk,
                &NoJournal,
                ConfirmToken::user_confirmed(),
                okx_entry_intent(client, cid),
            )
            .await
            .unwrap();
        assert!(matches!(out.outcome, Outcome::Accepted { .. }), "{cid}");
    }
    // The stream ends: the next entry is refused and nothing is sent.
    status::retire(account, token);
    let posts = fake.count("POST");
    let err = a
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            okx_entry_intent(&a, "eCCCC"),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SubmitError::StreamNotReady(_)), "{err:?}");
    assert_eq!(fake.count("POST"), posts);
}

// ── Release and restore edge cases (L-a, L-b) ───────────────────────────

#[tokio::test]
async fn a_release_token_of_another_account_is_refused_and_unlocks_nothing() {
    use super::unknown::ReleaseError;
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    let other_fake = serve(gen_okx);
    let other = client(AccountVenue::Okx, AccountProduct::UsdM, &other_fake);
    let challenge = pending.begin_manual_release();
    let phrase = challenge.phrase().to_string();
    let token = challenge.confirm(&phrase).unwrap();
    assert_eq!(
        other.release_unknown_manually(token).unwrap_err(),
        ReleaseError::WrongAccount
    );
    assert!(c.is_locked("BTC-USDT-SWAP"));
}

#[tokio::test]
async fn releasing_an_id_that_is_not_locked_says_so() {
    use super::unknown::ReleaseError;
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    let record = c.unknown_record(&pending);
    let challenge = pending.begin_manual_release();
    let phrase = challenge.phrase().to_string();
    let first = challenge.confirm(&phrase).unwrap();
    c.release_unknown_manually(first).unwrap();
    // The same release a second time: the id is gone from the lock table.
    let again = c.begin_manual_release_of_record(&record).unwrap();
    let phrase = again.phrase().to_string();
    let token = again.confirm(&phrase).unwrap();
    assert_eq!(
        c.release_unknown_manually(token).unwrap_err(),
        ReleaseError::NotLocked
    );
}

#[tokio::test]
async fn a_record_that_fails_the_rules_check_still_locks_and_can_be_released() {
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    let record = c.unknown_record(&pending);
    let challenge = pending.begin_manual_release();
    let phrase = challenge.phrase().to_string();
    c.release_unknown_manually(challenge.confirm(&phrase).unwrap())
        .unwrap();
    assert!(!c.is_locked("BTC-USDT-SWAP"));
    // After a restart the rules are missing: the lock still comes back from the record alone.
    c.lock_from_record(&record).unwrap();
    assert!(c.is_locked("BTC-USDT-SWAP"));
    // The release goes through the record, since no pending value exists.
    let challenge = c.begin_manual_release_of_record(&record).unwrap();
    let phrase = challenge.phrase().to_string();
    let released = c
        .release_unknown_manually(challenge.confirm(&phrase).unwrap())
        .unwrap();
    assert_eq!(released.symbol, "BTC-USDT-SWAP");
    assert!(!c.is_locked("BTC-USDT-SWAP"));
    // A record of another account is refused for both steps.
    let mut other = record;
    other.account = unique_account();
    assert!(c.lock_from_record(&other).is_err());
    assert!(c.begin_manual_release_of_record(&other).is_err());
}

#[tokio::test]
async fn a_record_with_a_bad_client_id_blocks_every_entry_of_the_account() {
    use super::unknown::RestoreError;
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    let mut bad = c.unknown_record(&pending);
    let challenge = pending.begin_manual_release();
    let phrase = challenge.phrase().to_string();
    c.release_unknown_manually(challenge.confirm(&phrase).unwrap())
        .unwrap();
    bad.client_id = "not an id".into();
    assert_eq!(
        c.lock_from_record(&bad).unwrap_err(),
        RestoreError::BadClientId
    );
    // Not even another symbol may open while an unreadable record is around.
    assert!(c.is_locked("BTC-USDT-SWAP"));
    assert!(c.is_locked("ETH-USDT-SWAP"));
}

#[tokio::test]
async fn debug_output_of_the_unknown_types_hides_the_client_id() {
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    let cid = pending.client_id().to_string();
    let record = c.unknown_record(&pending);
    let mut shown = vec![format!("{pending:?}"), format!("{record:?}")];
    let challenge = pending.begin_manual_release();
    shown.push(format!("{challenge:?}"));
    let phrase = challenge.phrase().to_string();
    let token = challenge.confirm(&phrase).unwrap();
    shown.push(format!("{token:?}"));
    let released = c.release_unknown_manually(token).unwrap();
    shown.push(format!("{released:?}"));
    for text in shown {
        assert!(!text.contains(&cid), "{text}");
        assert!(!text.contains(&phrase), "{text}");
    }
}

// ── Journal failure never stops a reduction (B5) ────────────────────────

fn failing_journal() -> TestJournal {
    let j = TestJournal::default();
    j.fail.store(true, Ordering::Relaxed);
    j
}

async fn okx_submit_with(
    c: &TradeClient,
    journal: &TestJournal,
    req: OkxRequest,
) -> Result<super::client::Submitted, SubmitError<()>> {
    let t = okx_swap_table();
    let fresh = t.fresh_lookup("BTC-USDT-SWAP", NOW, AGE).unwrap();
    let intent = OrderIntent::okx(c.account(), req, &fresh).unwrap();
    c.submit(
        &OpenGate,
        &NoRisk,
        journal,
        ConfirmToken::user_confirmed(),
        intent,
    )
    .await
}

#[tokio::test]
async fn a_failing_journal_does_not_stop_a_binance_reduce_only_close() {
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let intent = OrderIntent::binance(
        c.account(),
        futures_limit(
            r,
            &ClientId::generate(Role::Entry),
            Action::Close(Dir::Long),
        ),
        &fresh,
    )
    .unwrap();
    let out = c
        .submit(
            &OpenGate,
            &NoRisk,
            &failing_journal(),
            ConfirmToken::user_confirmed(),
            intent,
        )
        .await;
    assert!(out.is_ok(), "{:?}", out.err());
    assert_eq!(fake.count("POST /fapi/v1/order"), 1);
}

#[tokio::test]
async fn a_failing_journal_does_not_stop_okx_close_position_or_a_protective_algo() {
    let fake = serve(gen_okx);
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let t = okx_swap_table();
    let sr = t.lookup("BTC-USDT-SWAP").unwrap();
    let journal = failing_journal();
    let close = okx::close_position(
        sr,
        Some(PositionMode::OneWay),
        MarginMode::Cross,
        Dir::Long,
        ClientId::generate(Role::Entry),
    )
    .unwrap();
    assert!(okx_submit_with(&c, &journal, close).await.is_ok());
    assert_eq!(fake.count("POST /api/v5/trade/close-position"), 1);
    let algo = okx::place_algo(OkxProtective {
        rules: sr,
        target: OkxTarget::Swap {
            mode: Some(PositionMode::OneWay),
            margin: MarginMode::Cross,
            action: Action::Close(Dir::Long),
        },
        ord_type: OkxAlgoType::Conditional,
        size: Size::ClosePosition,
        tp_trigger: None,
        sl_trigger: Some(d("59000")),
        trigger_type: TriggerPxType::Last,
        ref_price: d("60000"),
        client_id: ClientId::generate(Role::Entry),
    })
    .unwrap();
    assert!(okx_submit_with(&c, &journal, algo).await.is_ok());
    assert_eq!(fake.count("POST /api/v5/trade/order-algo"), 1);
}

#[tokio::test]
async fn a_failing_journal_does_not_stop_flatten_or_auto_close() {
    let positions = |m: &str, p: &str| match (m, p) {
        ("GET", "/api/v5/account/positions") => Some(
            json!({"code": "0", "data": [{"instId": "BTC-USDT-SWAP", "pos": "2",
                "posSide": "net", "mgnMode": "cross", "lever": "10"}]})
            .to_string(),
        ),
        ("GET", "/api/v5/trade/orders-pending") | ("GET", "/api/v5/trade/orders-algo-pending") => {
            Some(json!({"code": "0", "data": []}).to_string())
        }
        _ => None,
    };
    let fake = serve(move |m, p| match positions(m, p) {
        Some(b) => (200, b),
        None if p == "/api/v5/trade/close-position" => gen_okx(m, p),
        None => (404, "{}".to_string()),
    });
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let journal = failing_journal();
    let rep = c
        .flatten(
            &OpenGate,
            &NoRisk,
            &journal,
            ConfirmToken::user_confirmed(),
            &okx_swap_table(),
            MarginMode::Cross,
        )
        .await
        .unwrap();
    assert_eq!((rep.sent, rep.failed), (1, 0), "{rep:?}");
    assert_eq!(fake.count("POST /api/v5/trade/close-position"), 1);
    let mut tracker = tripped_tracker(c.account(), EquityBasis::OkxTotalEq);
    let super::risk::AutoCloseDecision::Run(grant) = c.begin_auto_close(&mut tracker, true) else {
        panic!("expected the automatic close to run");
    };
    let _ = c
        .auto_close(
            &OpenGate,
            &c.risk_snapshot(&tracker),
            &journal,
            grant,
            &okx_swap_table(),
            MarginMode::Cross,
        )
        .await;
    assert_eq!(
        fake.count("POST /api/v5/trade/close-position"),
        2,
        "the automatic close went out too"
    );
}

#[tokio::test]
async fn a_failing_journal_still_refuses_a_new_entry_without_a_post() {
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let intent = OrderIntent::binance(
        c.account(),
        futures_limit(r, &ClientId::generate(Role::Entry), Action::Open(Dir::Long)),
        &fresh,
    )
    .unwrap();
    let err = c
        .submit(
            &OpenGate,
            &NoRisk,
            &failing_journal(),
            ConfirmToken::user_confirmed(),
            intent,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SubmitError::JournalUnavailable), "{err:?}");
    assert_eq!(fake.count("POST"), 0);
}

// ── Locking without a client, and blocking everything (B6) ──────────────

#[tokio::test]
async fn a_record_locks_before_any_client_exists() {
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    let record = c.unknown_record(&pending);
    let account = c.account();
    // A restart: the old lock is gone, and the only thing in hand is the saved record.
    let challenge = pending.begin_manual_release();
    let phrase = challenge.phrase().to_string();
    c.release_unknown_manually(challenge.confirm(&phrase).unwrap())
        .unwrap();
    assert!(!c.is_locked("BTC-USDT-SWAP"));
    super::unknown::lock_from_record(account, &record).unwrap();
    // A client built afterwards (as the send path does every time) finds the lock.
    let fake2 = serve(gen_okx);
    let mut c2 = client(AccountVenue::Okx, AccountProduct::UsdM, &fake2);
    c2.account = account;
    super::stream::status::set_for_test(account, super::stream::StreamState::Connected);
    let err = c2
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::user_confirmed(),
            okx_entry_intent(&c2, "eCCCC"),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SubmitError::Locked), "{err:?}");
    assert_eq!(fake2.count("POST"), 0);
}

#[tokio::test]
async fn block_all_entries_refuses_every_account_but_not_closes() {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            super::locks::unblock_all_for_test();
        }
    }
    let _reset = Reset;
    super::unknown::block_all_entries();
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    assert!(matches!(
        submit_futures(&c, Action::Open(Dir::Long)).await,
        Err(SubmitError::Locked)
    ));
    assert_eq!(fake.count("POST"), 0);
    let other = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    assert!(matches!(
        submit_futures(&other, Action::Open(Dir::Short)).await,
        Err(SubmitError::Locked)
    ));
    assert!(submit_futures(&c, Action::Close(Dir::Long)).await.is_ok());
    assert_eq!(fake.count("POST /fapi/v1/order"), 1);
}
