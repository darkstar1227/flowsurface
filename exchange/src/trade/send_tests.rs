//! Send-layer tests against a local fake server (no real exchange is contacted). Each test
//! uses its own limiter host name so limiter state is not shared between tests.

use super::binance_futures as bf;
use super::binance_spot as bs;
use super::client::{OpenKind, TradeClient};
use super::client_id::{ClientId, Role};
use super::gate::{
    AutoIntent, AutoPath, BulkAction, ConfirmToken, GateRequest, OrderIntent, RiskHook,
    RiskRefusal, RiskView, SendGate, SubmitError,
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
use crate::signed_http::SignedHttp;
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
    /// `(method, target with query, body)` of every request, as received.
    raw: Arc<Mutex<Vec<(String, String, String)>>>,
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

    fn raw(&self) -> Vec<(String, String, String)> {
        self.raw.lock().unwrap().clone()
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
    let raw = Arc::new(Mutex::new(Vec::new()));
    let raw2 = Arc::clone(&raw);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { break };
            let handler = Arc::clone(&handler);
            let log = Arc::clone(&log2);
            let raw = Arc::clone(&raw2);
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
                let sent_body = head
                    .split_once("\r\n\r\n")
                    .map_or("", |(_, b)| b)
                    .to_string();
                raw.lock()
                    .unwrap()
                    .push((method.clone(), target.clone(), sent_body));
                let (status, body) = handler(&method, &path, &query);
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(reply.as_bytes());
            });
        }
    });
    Fake { port, log, raw }
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
        SignedHttp::for_test(reqwest::Client::new(), ProxyState::Direct),
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

struct ClosedGate;
impl SendGate for ClosedGate {
    type Refusal = &'static str;
    fn check(&self, _: GateRequest) -> Result<(), &'static str> {
        Err("closed")
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
                ConfirmToken::for_preview(&intent.preview()),
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
            .submit_auto_unbound(
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
                ConfirmToken::for_preview(&cancel.preview()),
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
        ConfirmToken::for_preview(&intent.preview()),
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
            ConfirmToken::for_preview(&intent.preview()),
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
            ConfirmToken::for_preview(&again.preview()),
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
            ConfirmToken::for_preview(&close.preview()),
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
            ConfirmToken::for_preview(&intent.preview()),
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
            ConfirmToken::for_preview(&intent.preview()),
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
            ConfirmToken::for_bulk(c.account(), BulkAction::CancelAll),
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
            ConfirmToken::for_bulk(c.account(), BulkAction::CancelAll),
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
            ConfirmToken::for_preview(&mk().preview()),
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
            ConfirmToken::for_preview(&mk().preview()),
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
            ConfirmToken::for_preview(&mk().preview()),
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
            ConfirmToken::for_preview(&mk().preview()),
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
        SignedHttp::for_test(reqwest::Client::new(), ProxyState::Direct),
        AccountVenue::Binance,
        AccountEnv::Live,
        AccountProduct::UsdM,
        AccountKey::from_bytes([1; 16]),
        ApiCredentials::new("k".into(), "s".into(), None),
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
            ConfirmToken::for_preview(&intent.preview()),
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
            ConfirmToken::for_preview(&intent.preview()),
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
            ConfirmToken::for_preview(&close.preview()),
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
            ConfirmToken::for_bulk(c.account(), BulkAction::Flatten),
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
            ConfirmToken::for_bulk(sc.account(), BulkAction::Flatten),
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
            ConfirmToken::for_bulk(c.account(), BulkAction::CancelAll),
            &futures(),
        )
        .await
        .unwrap();
    assert_eq!(rep.failed, 1);
    assert!(!rep.is_clear());
    assert_eq!(fake.count("DELETE /fapi/v1/allOpenOrders"), 1);
    assert_eq!(fake.count("GET /fapi/v1/openOrders"), 2);
}

/// A gate that refuses every request: cancel all writes nothing and counts each cancel as failed.
#[tokio::test]
async fn cancel_all_under_a_closed_gate_writes_nothing_and_reports_failed() {
    let fake = serve(|m, p| match (m, p) {
        ("GET", "/fapi/v1/openOrders") => (
            200,
            json!([{"symbol": "BTCUSDT", "orderId": 1}]).to_string(),
        ),
        ("GET", "/fapi/v1/openAlgoOrders") => (200, "[]".to_string()),
        _ => (404, "{}".to_string()),
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let rep = c
        .cancel_all(
            &ClosedGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::for_bulk(c.account(), BulkAction::CancelAll),
            &futures(),
        )
        .await
        .unwrap();
    assert!(rep.failed >= 1, "{rep:?}");
    assert_eq!(rep.sent, 0);
    assert!(!rep.is_clear());
    assert_eq!(fake.count("DELETE /fapi/v1/allOpenOrders"), 0);
    assert!(
        fake.all().iter().all(|r| r.starts_with("GET ")),
        "only reads: {:?}",
        fake.all()
    );
}

#[tokio::test]
async fn flatten_under_a_closed_gate_writes_nothing_and_reports_failed() {
    let fake = serve(|m, p| match (m, p) {
        ("GET", "/fapi/v3/positionRisk") => (
            200,
            json!([{"symbol": "BTCUSDT", "positionAmt": "-0.002", "positionSide": "BOTH",
                    "marginType": "cross", "leverage": "10"}])
            .to_string(),
        ),
        _ => (404, "{}".to_string()),
    });
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let rep = c
        .flatten(
            &ClosedGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::for_bulk(c.account(), BulkAction::Flatten),
            &futures(),
            MarginMode::Cross,
        )
        .await
        .unwrap();
    assert_eq!(rep.failed, 1, "{rep:?}");
    assert_eq!(rep.sent, 0);
    assert!(!rep.is_clear());
    assert_eq!(fake.count("POST /fapi/v1/order"), 0);
    assert!(
        fake.all().iter().all(|r| r.starts_with("GET ")),
        "only reads: {:?}",
        fake.all()
    );
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
            ConfirmToken::for_preview(&intent.preview()),
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
            ConfirmToken::for_preview(&intent.preview()),
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
            ConfirmToken::for_preview(&intent.preview()),
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
            ConfirmToken::for_preview(&intent.preview()),
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
        ConfirmToken::for_preview(&mk("eAAAA").preview()),
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
            ConfirmToken::for_preview(&mk("eBBBB").preview()),
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
            ConfirmToken::for_preview(&intent.preview()),
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
            ConfirmToken::for_preview(&intent.preview()),
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
            ConfirmToken::for_preview(&intent.preview()),
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
            ConfirmToken::for_preview(&intent.preview()),
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
            ConfirmToken::for_preview(&mk("eAAAA").preview()),
            mk("eAAAA")
        ),
        async {
            // let the first one reach the transport
            tokio::time::sleep(Duration::from_millis(50)).await;
            c.submit(
                &OpenGate,
                &NoRisk,
                &NoJournal,
                ConfirmToken::for_preview(&mk("eBBBB").preview()),
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
            ConfirmToken::for_bulk(c.account(), BulkAction::CancelAll),
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
            ConfirmToken::for_preview(&intent.preview()),
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
            ConfirmToken::for_preview(&entry.preview()),
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
            ConfirmToken::for_bulk(c.account(), BulkAction::CancelAll),
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
            ConfirmToken::for_bulk(c.account(), BulkAction::CancelAll),
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
            ConfirmToken::for_bulk(c.account(), BulkAction::CancelAll),
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
            ConfirmToken::for_bulk(c.account(), BulkAction::CancelAll),
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
        ConfirmToken::for_preview(&intent.preview()),
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
        let close = submit_futures(&c, Action::Close(Dir::Long)).await;
        if st == StreamState::AuthFailed {
            // The credentials are void: nothing is sent, closes included (condition 57).
            assert!(
                matches!(
                    close,
                    Err(SubmitError::StreamNotReady(StreamState::AuthFailed))
                ),
                "a close must be refused after a failed login"
            );
        } else {
            assert!(close.is_ok(), "close must pass in {st:?}");
        }
    }
    assert_eq!(fake.count("POST /fapi/v1/order"), 3);
    set_stream(&c, StreamState::Connected);
    assert!(submit_futures(&c, Action::Open(Dir::Long)).await.is_ok());
}

#[tokio::test]
async fn after_a_failed_login_every_kind_of_order_is_refused_and_nothing_is_sent() {
    use super::stream::StreamState;
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    set_stream(&c, StreamState::AuthFailed);
    for action in [
        Action::Open(Dir::Long),
        Action::Close(Dir::Long),
        Action::Close(Dir::Short),
    ] {
        let r = submit_futures(&c, action).await;
        assert!(
            matches!(r, Err(SubmitError::StreamNotReady(StreamState::AuthFailed))),
            "{r:?}"
        );
    }
    assert_eq!(fake.count("POST"), 0);
}

/// A new test after a failed login makes a new stream that takes over the status entry: the
/// close that was refused goes through the real send path, and the old stream cannot put the
/// failure back (M5 c2b-2, item 7; the GUI cannot build a real client, so it is checked here).
#[tokio::test]
async fn a_new_stream_after_a_failed_login_lets_the_close_through_the_real_gate() {
    use super::stream::{StreamState, status};
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let account = c.account();
    status::retire(account, 0);
    let old = status::register(account);
    status::publish(account, old, StreamState::AuthFailed);
    let refused = submit_futures(&c, Action::Close(Dir::Long)).await;
    assert!(
        matches!(
            refused,
            Err(SubmitError::StreamNotReady(StreamState::AuthFailed))
        ),
        "{refused:?}"
    );
    assert_eq!(fake.count("POST"), 0);
    // The account is tested again: its new stream takes the entry over.
    let new = status::register(account);
    status::publish(account, new, StreamState::Connected);
    // The old stream's late failure does not touch it.
    status::publish(account, old, StreamState::AuthFailed);
    assert!(submit_futures(&c, Action::Close(Dir::Long)).await.is_ok());
    assert_eq!(fake.count("POST /fapi/v1/order"), 1);
    status::retire(account, new);
}

#[tokio::test]
async fn an_entry_waits_for_the_snapshot_the_tracker_has_not_applied_yet() {
    let snapshot_at = |epoch: u64| crate::trade::stream::Snapshot {
        orders: vec![],
        positions: vec![],
        balances: vec![],
        epoch,
    };
    use super::stream::{StreamState, status, sync_epoch};
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let account = c.account();
    // A real registration, as `run` does it.
    status::retire(account, 0);
    let token = status::register(account);
    let mut tracker = EquityTracker::new(
        account,
        EquityBasis::BinanceTotalMarginBalance,
        RiskLimits::default(),
    );
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let send = async |tracker: &EquityTracker, action: Action, cid: &str| {
        let intent =
            OrderIntent::binance(account, futures_limit(r, &id(cid), action), &fresh).unwrap();
        c.submit(
            &OpenGate,
            &c.risk_snapshot(tracker),
            &NoJournal,
            ConfirmToken::for_preview(&intent.preview()),
            intent,
        )
        .await
    };

    // The first snapshot is sent and the state is `Connected`, but the tracker has not
    // applied it yet.
    let epoch = status::begin_sync(account, token);
    status::publish(account, token, StreamState::Connected);
    assert_eq!(sync_epoch(account), epoch);
    let err = send(&tracker, Action::Open(Dir::Long), "eAAAA")
        .await
        .unwrap_err();
    assert!(
        matches!(err, SubmitError::Risk(RiskRefusal::SnapshotNotApplied)),
        "{err:?}"
    );
    // A close is not an entry.
    assert!(
        send(&tracker, Action::Close(Dir::Long), "eAAAB")
            .await
            .is_ok()
    );
    // Applying the snapshot lets the entry through.
    tracker.snapshot_applied(&snapshot_at(epoch), NOW, []);
    assert!(
        send(&tracker, Action::Open(Dir::Long), "eAAAC")
            .await
            .is_ok()
    );

    // The stream drops and comes back with a new snapshot: back to Connected, not applied.
    status::publish(account, token, StreamState::Reconnecting);
    let again = status::begin_sync(account, token);
    assert_ne!(again, epoch);
    status::publish(account, token, StreamState::Connected);
    let err = send(&tracker, Action::Open(Dir::Long), "eAAAD")
        .await
        .unwrap_err();
    assert!(
        matches!(err, SubmitError::Risk(RiskRefusal::SnapshotNotApplied)),
        "{err:?}"
    );
    // Applying the old epoch again does not help; the new one does.
    tracker.snapshot_applied(&snapshot_at(epoch), NOW, []);
    assert!(
        send(&tracker, Action::Open(Dir::Long), "eAAAE")
            .await
            .is_err()
    );
    tracker.snapshot_applied(&snapshot_at(again), NOW, []);
    assert!(
        send(&tracker, Action::Open(Dir::Long), "eAAAF")
            .await
            .is_ok()
    );
    status::retire(account, token);
}

#[tokio::test]
async fn stream_gate_starts_closed_for_an_unwired_client() {
    use super::stream::StreamState;
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    // A fresh client (before any stream output) is not connected.
    let fresh = TradeClient::new(
        SignedHttp::for_test(reqwest::Client::new(), ProxyState::Direct),
        AccountVenue::Binance,
        AccountEnv::Live,
        AccountProduct::UsdM,
        unique_account(),
        ApiCredentials::new("k".into(), "s".into(), None),
        Arc::new(|| ProxyState::Direct),
    );
    assert_eq!(
        stream_state_for(fresh.account(), AccountVenue::Binance, AccountProduct::UsdM),
        StreamState::Reconnecting
    );
    let spot = TradeClient::new(
        SignedHttp::for_test(reqwest::Client::new(), ProxyState::Direct),
        AccountVenue::Binance,
        AccountEnv::Live,
        AccountProduct::Spot,
        unique_account(),
        ApiCredentials::new("k".into(), "s".into(), None),
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
            ConfirmToken::for_preview(&okx_entry_intent(&c, "eAAAA").preview()),
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
            ConfirmToken::for_preview(&okx_entry_intent(&c, "eBBBB").preview()),
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
            ConfirmToken::for_preview(&okx_entry_intent(&c, "eAAAA").preview()),
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
            ConfirmToken::for_preview(&okx_entry_intent(&c, "eAAAA").preview()),
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
            ConfirmToken::for_preview(&okx_entry_intent(&c, "eAAAA").preview()),
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
        SignedHttp::for_test(reqwest::Client::new(), ProxyState::Direct),
        AccountVenue::Okx,
        AccountEnv::Live,
        AccountProduct::UsdM,
        account,
        ApiCredentials::new("k".into(), "s".into(), Some("p".into())),
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
                ConfirmToken::for_preview(&okx_entry_intent(client, cid).preview()),
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
            ConfirmToken::for_preview(&okx_entry_intent(&a, "eCCCC").preview()),
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
        ConfirmToken::for_preview(&intent.preview()),
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
            ConfirmToken::for_preview(&intent.preview()),
            intent,
        )
        .await;
    assert!(out.is_ok(), "{:?}", out.as_ref().err());
    assert!(
        out.unwrap().journal_failed,
        "the caller is told the write-ahead record is missing"
    );
    assert_eq!(fake.count("POST /fapi/v1/order"), 1);
}

#[tokio::test]
async fn a_working_journal_reports_no_failure() {
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let out = submit_futures(&c, Action::Close(Dir::Long)).await.unwrap();
    assert!(!out.journal_failed);
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
    assert!(
        okx_submit_with(&c, &journal, close)
            .await
            .unwrap()
            .journal_failed
    );
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
    assert!(
        okx_submit_with(&c, &journal, algo)
            .await
            .unwrap()
            .journal_failed
    );
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
            ConfirmToken::for_bulk(c.account(), BulkAction::Flatten),
            &okx_swap_table(),
            MarginMode::Cross,
        )
        .await
        .unwrap();
    assert_eq!((rep.sent, rep.failed), (1, 0), "{rep:?}");
    assert!(rep.journal_failed, "the report carries the journal failure");
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
            ConfirmToken::for_preview(&intent.preview()),
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
    super::unknown::lock_from_record(&record).unwrap();
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
            ConfirmToken::for_preview(&okx_entry_intent(&c2, "eCCCC").preview()),
            okx_entry_intent(&c2, "eCCCC"),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SubmitError::Locked), "{err:?}");
    assert_eq!(fake2.count("POST"), 0);
}

#[tokio::test]
async fn only_a_record_with_an_invalid_id_can_be_discarded_and_only_by_the_typed_phrase() {
    use super::unknown::DiscardError;
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    let good = c.unknown_record(&pending);
    // A live unresolved send is never discarded: it is looked up or released.
    assert_eq!(good.begin_discard().unwrap_err(), DiscardError::StillValid);
    let mut bad = good.clone();
    bad.client_id = "not-an-id-123".into();
    assert!(!bad.has_valid_client_id());
    let challenge = bad.begin_discard().unwrap();
    let phrase = challenge.phrase().to_string();
    assert!(phrase.starts_with("discard BTC-USDT-SWAP "), "{phrase}");
    // A wrong phrase gives the challenge back.
    let challenge = challenge.confirm("yes").unwrap_err();
    let token = challenge.confirm(&phrase).unwrap();
    assert_eq!(token.client_id(), "not-an-id-123");
    assert_eq!(token.account(), bad.account);
    // Nothing printed shows the id or the phrase.
    let shown = format!("{token:?} {:?}", bad.begin_discard().unwrap());
    assert!(
        !shown.contains("not-an-id-123") && !shown.contains(&phrase),
        "{shown}"
    );
}

#[tokio::test]
async fn the_free_function_locks_the_account_the_record_names() {
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    let record = c.unknown_record(&pending);
    let challenge = pending.begin_manual_release();
    let phrase = challenge.phrase().to_string();
    c.release_unknown_manually(challenge.confirm(&phrase).unwrap())
        .unwrap();
    assert!(!c.is_locked("BTC-USDT-SWAP"));
    // There is no account argument to get wrong: the record's own account is locked.
    super::unknown::lock_from_record(&record).unwrap();
    assert!(c.is_locked("BTC-USDT-SWAP"));
    // The same free function on a record of a stranger locks the stranger's account only.
    let mut stranger = record;
    stranger.account = unique_account();
    super::unknown::lock_from_record(&stranger).unwrap();
    assert!(c.is_locked("BTC-USDT-SWAP"));
    assert!(super::locks::is_locked(stranger.account, "BTC-USDT-SWAP"));
}

#[tokio::test]
async fn locking_twice_from_the_same_record_needs_one_release() {
    let fake = okx_unknown_server(|_| okx_no_such_order());
    let (c, pending) = okx_unknown(&fake).await;
    let record = c.unknown_record(&pending);
    let t = okx_swap_table();
    let sr = t.lookup("BTC-USDT-SWAP").unwrap();
    // The free function (startup, before any client) and `restore_unknown` (which locks again)
    // describe the same order: one lock, not two.
    super::unknown::lock_from_record(&record).unwrap();
    super::unknown::lock_from_record(&record).unwrap();
    let restored = c.restore_unknown(&record, sr).unwrap();
    // One manual release unlocks completely.
    let challenge = restored.begin_manual_release();
    let phrase = challenge.phrase().to_string();
    c.release_unknown_manually(challenge.confirm(&phrase).unwrap())
        .unwrap();
    assert!(!c.is_locked("BTC-USDT-SWAP"));

    // Same through the resolver: lock twice, restore, and one answer unlocks it.
    super::unknown::lock_from_record(&record).unwrap();
    let restored = c.restore_unknown(&record, sr).unwrap();
    let res = c.resolve_unknown(restored, &NoJournal).await;
    assert!(matches!(res, Resolution::NotPlacedPresumed), "{res:?}");
    assert!(!c.is_locked("BTC-USDT-SWAP"));
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

// ── Send futures, the preview and the bound confirmation ─────────────────

fn assert_send<T: Send>(_: &T) {}

/// Nothing is run: these only have to compile. The futures are `Send` so they fit in a spawned
/// task, which needs the risk hook and the journal to be `Sync` (`&dyn` of them crosses threads).
#[test]
fn every_send_path_future_is_send() {
    use super::risk::AutoCloseGrant;
    use super::risk::SharedRisk;
    let _: fn(
        &TradeClient,
        &dyn RiskHook,
        &dyn super::journal::UnknownJournal,
        ConfirmToken,
        OrderIntent,
    ) = |c, r, j, t, i| assert_send(&c.submit(&OpenGate, r, j, t, i));
    let _: fn(
        &TradeClient,
        &dyn RiskHook,
        &dyn super::journal::UnknownJournal,
        super::grant::GrantedAuto,
    ) = |c, r, j, a| assert_send(&c.submit_auto(&OpenGate, r, j, a));
    let _: fn(&TradeClient, PendingUnknown, &dyn super::journal::UnknownJournal) =
        |c, p, j| assert_send(&c.resolve_unknown(p, j));
    let _: fn(
        &TradeClient,
        &dyn RiskHook,
        &dyn super::journal::UnknownJournal,
        ConfirmToken,
        &RulesTable,
    ) = |c, r, j, t, tb| assert_send(&c.cancel_all(&OpenGate, r, j, t, tb));
    let _: fn(
        &TradeClient,
        &dyn RiskHook,
        &dyn super::journal::UnknownJournal,
        ConfirmToken,
        &RulesTable,
    ) = |c, r, j, t, tb| assert_send(&c.flatten(&OpenGate, r, j, t, tb, MarginMode::Cross));
    let _: fn(
        &TradeClient,
        &dyn RiskHook,
        &dyn super::journal::UnknownJournal,
        AutoCloseGrant,
        &RulesTable,
    ) = |c, r, j, g, tb| assert_send(&c.auto_close(&OpenGate, r, j, g, tb, MarginMode::Cross));
    // The shared hook can sit in a `'static` task next to the client.
    fn send_static<T: Send + Sync + 'static>() {}
    send_static::<SharedRisk>();
}

fn futures_limit_qty(rules: &SymbolRules, cid: &ClientId, qty: &str) -> BinanceRequest {
    bf::new_order(bf::FuturesEntry {
        rules,
        mode: Some(PositionMode::OneWay),
        action: Action::Open(Dir::Long),
        qty: d(qty),
        exec: Exec::Limit {
            price: d("60000"),
            tif: TimeInForce::Gtc,
        },
        ref_price: Some(d("60000")),
        client_id: dup(cid),
    })
    .unwrap()
}

fn futures_intent(c: &TradeClient, qty: &str) -> OrderIntent {
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    OrderIntent::binance(c.account(), futures_limit_qty(r, &id("eAAAA"), qty), &fresh).unwrap()
}

/// What the preview shows, against what the server received: same names, same order (when
/// `ordered`), same values; only a client id is masked in the preview.
fn assert_preview_matches(
    preview: &super::preview::OrderPreview,
    sent: Vec<(String, String)>,
    ordered: bool,
) {
    assert_eq!(
        preview.fields.len(),
        sent.len(),
        "{:?} vs {sent:?}",
        preview.fields
    );
    let mut shown: Vec<_> = preview.fields.iter().collect();
    let mut sent = sent;
    if !ordered {
        shown.sort_by(|a, b| a.name.cmp(&b.name));
        sent.sort();
    }
    for (f, (name, value)) in shown.iter().zip(&sent) {
        assert_eq!(&f.name, name);
        let lower = name.to_ascii_lowercase();
        let is_id = lower.ends_with("clientorderid")
            || lower.ends_with("clordid")
            || lower == "clientalgoid";
        if is_id {
            let tail: String = value
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            assert_eq!(f.value, format!("…{tail}"), "{name}");
            assert_ne!(&f.value, value, "a client id must be masked");
        } else {
            assert_eq!(&f.value, value, "{name}");
        }
    }
}

#[tokio::test]
async fn the_preview_equals_the_form_the_binance_server_receives() {
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let intent = futures_intent(&c, "0.002");
    let preview = intent.preview();
    assert_eq!(preview.symbol, "BTCUSDT");
    assert_eq!(preview.path, "/fapi/v1/order");
    assert_eq!(preview.field("quantity"), Some("0.002"));
    assert_eq!(preview.field("price"), Some("60000"));
    // No client id in full anywhere in what the window may print.
    assert!(!format!("{preview:?}").contains("eAAAA"));
    c.submit(
        &OpenGate,
        &NoRisk,
        &NoJournal,
        ConfirmToken::for_preview(&preview),
        intent,
    )
    .await
    .unwrap();
    let raw = fake.raw();
    assert_eq!(raw.len(), 1);
    let (method, target, body) = &raw[0];
    assert_eq!(
        (method.as_str(), target.as_str()),
        ("POST", "/fapi/v1/order")
    );
    let sent: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .filter(|(k, _)| !matches!(k.as_str(), "recvWindow" | "timestamp" | "signature"))
        .collect();
    assert_preview_matches(&preview, sent, true);
}

#[tokio::test]
async fn the_preview_equals_the_json_the_okx_server_receives() {
    let fake = serve(gen_okx);
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let intent = okx_entry_intent(&c, "eAAAA");
    let preview = intent.preview();
    assert_eq!(preview.symbol, "BTC-USDT-SWAP");
    assert_eq!(preview.field("sz"), Some("2"));
    assert!(!format!("{preview:?}").contains("eAAAA"));
    c.submit(
        &OpenGate,
        &NoRisk,
        &NoJournal,
        ConfirmToken::for_preview(&preview),
        intent,
    )
    .await
    .unwrap();
    let raw = fake.raw();
    assert_eq!(raw.len(), 1);
    let (method, target, body) = &raw[0];
    assert_eq!(
        (method.as_str(), target.as_str()),
        ("POST", "/api/v5/trade/order")
    );
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_preview_matches(&preview, super::preview::flatten_json(&json), false);
}

/// The entry built by `build_intent` with a protection setting: the preview names the attached
/// TP and SL, and is the JSON the server receives, attachment included (d2).
#[tokio::test]
async fn the_preview_of_an_okx_entry_with_attached_protection_equals_the_json_the_server_receives()
{
    use super::decimal::Decimal;
    use super::manual::{ManualContext, ManualSpec, build_intent};
    use super::protect::{Offset, ProtectSpec, TriggerBasis};
    use super::types::MarginMode;
    let fake = serve(gen_okx);
    let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
    let t = okx_swap_table();
    let fresh = t.fresh_lookup("BTC-USDT-SWAP", NOW, AGE).unwrap();
    let spec = ProtectSpec {
        stop: Some(Offset::Percent(Decimal::parse("2").unwrap())),
        targets: vec![super::protect::TargetSpec {
            offset: Offset::Distance(Decimal::parse("2000").unwrap()),
            share_pct: 100,
        }],
        trigger_price: TriggerBasis::Mark,
        ..ProtectSpec::default()
    };
    let intent = build_intent(
        c.account(),
        &fresh,
        &ManualSpec::Market {
            dir: Dir::Long,
            qty: Decimal::parse("2").unwrap(),
        },
        ManualContext {
            mode: Some(PositionMode::OneWay),
            ref_price: Some(Decimal::parse("50000").unwrap()),
            okx_margin: MarginMode::Cross,
            protect: Some(&spec),
        },
    )
    .unwrap();
    let preview = intent.preview();
    assert_eq!(
        preview.field("attachAlgoOrds[0].slTriggerPx"),
        Some("49000")
    );
    assert_eq!(
        preview.field("attachAlgoOrds[0].tpTriggerPx"),
        Some("52000")
    );
    assert_eq!(
        preview.field("attachAlgoOrds[0].slTriggerPxType"),
        Some("mark")
    );
    c.submit(
        &OpenGate,
        &NoRisk,
        &NoJournal,
        ConfirmToken::for_preview(&preview),
        intent,
    )
    .await
    .unwrap();
    let raw = fake.raw();
    assert_eq!(raw.len(), 1);
    let json: serde_json::Value = serde_json::from_str(&raw[0].2).unwrap();
    assert_preview_matches(&preview, super::preview::flatten_json(&json), false);
}

#[tokio::test]
async fn a_token_for_another_quantity_is_refused_and_nothing_is_sent() {
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let shown = futures_intent(&c, "0.002");
    let old_token = ConfirmToken::for_preview(&shown.preview());
    // The user changed the quantity after confirming: a new request, the old token.
    let changed = futures_intent(&c, "0.003");
    assert_ne!(
        shown.preview().field("quantity"),
        changed.preview().field("quantity")
    );
    let err = c
        .submit(&OpenGate, &NoRisk, &NoJournal, old_token, changed)
        .await
        .unwrap_err();
    assert_eq!(err, SubmitError::ConfirmationMismatch);
    assert_eq!(fake.count("POST"), 0);
    // The same request still goes with its own token, and an old token cannot be reused for
    // it: tokens are single use, so the next one is made from a preview again.
    let ok = c
        .submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::for_preview(&shown.preview()),
            shown,
        )
        .await;
    assert!(ok.is_ok(), "{ok:?}");
    assert_eq!(fake.count("POST /fapi/v1/order"), 1);
}

#[tokio::test]
async fn a_token_for_another_client_id_or_another_kind_of_action_is_refused() {
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
    let a = OrderIntent::binance(
        c.account(),
        futures_limit_qty(r, &id("eAAAA"), "0.002"),
        &fresh,
    )
    .unwrap();
    let b = OrderIntent::binance(
        c.account(),
        futures_limit_qty(r, &id("eBBBB"), "0.002"),
        &fresh,
    )
    .unwrap();
    // Same values, other client id: another request, so another confirmation.
    assert_eq!(
        c.submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::for_preview(&a.preview()),
            b
        )
        .await
        .unwrap_err(),
        SubmitError::ConfirmationMismatch
    );
    // A cancel-all / close-all confirmation does not confirm one order, and the reverse.
    assert_eq!(
        c.submit(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::for_bulk(c.account(), BulkAction::CancelAll),
            a
        )
        .await
        .unwrap_err(),
        SubmitError::ConfirmationMismatch
    );
    let other = futures_intent(&c, "0.002");
    let res = c
        .cancel_all(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::for_preview(&other.preview()),
            &t,
        )
        .await;
    assert!(res.is_err());
    assert!(fake.all().is_empty(), "{:?}", fake.all());
}

#[tokio::test]
async fn a_bulk_token_for_another_account_sends_nothing() {
    let fake = serve(gen_binance);
    let a = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let b = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    assert_ne!(a.account(), b.account());
    let t = futures();
    let token = ConfirmToken::for_bulk(a.account(), BulkAction::CancelAll);
    let res = b
        .cancel_all(&OpenGate, &NoRisk, &NoJournal, token, &t)
        .await;
    assert!(
        matches!(res, Err(crate::trade::BulkError::ConfirmationMismatch)),
        "{res:?}"
    );
    let token = ConfirmToken::for_bulk(a.account(), BulkAction::Flatten);
    let res = b
        .flatten(&OpenGate, &NoRisk, &NoJournal, token, &t, MarginMode::Cross)
        .await;
    assert!(
        matches!(res, Err(crate::trade::BulkError::ConfirmationMismatch)),
        "{res:?}"
    );
    assert!(fake.all().is_empty(), "{:?}", fake.all());
}

#[tokio::test]
async fn a_cancel_all_token_cannot_flatten_and_the_reverse() {
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let t = futures();
    let res = c
        .flatten(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::for_bulk(c.account(), BulkAction::CancelAll),
            &t,
            MarginMode::Cross,
        )
        .await;
    assert!(
        matches!(res, Err(crate::trade::BulkError::ConfirmationMismatch)),
        "{res:?}"
    );
    let res = c
        .cancel_all(
            &OpenGate,
            &NoRisk,
            &NoJournal,
            ConfirmToken::for_bulk(c.account(), BulkAction::Flatten),
            &t,
        )
        .await;
    assert!(
        matches!(res, Err(crate::trade::BulkError::ConfirmationMismatch)),
        "{res:?}"
    );
    assert!(fake.all().is_empty(), "{:?}", fake.all());
}

#[tokio::test]
async fn a_bulk_confirmation_never_covers_an_entry() {
    let fake = serve(gen_binance);
    let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
    let entry = futures_intent(&c, "0.002");
    assert!(matches!(entry.exposure(), Exposure::Adds { .. }));
    for action in [BulkAction::CancelAll, BulkAction::Flatten] {
        let spent = ConfirmToken::for_bulk(c.account(), action)
            .spend_bulk(c.account(), action)
            .unwrap();
        let res = c
            .submit_spent(
                &OpenGate,
                &NoRisk,
                &NoJournal,
                spent,
                futures_intent(&c, "0.002"),
            )
            .await;
        assert_eq!(res.unwrap_err(), SubmitError::ConfirmationMismatch);
    }
    assert!(fake.all().is_empty(), "{:?}", fake.all());
}

#[test]
fn requests_that_differ_only_in_the_query_have_different_preview_hashes() {
    let build = |path: &str| {
        super::preview::OrderPreview::build(
            unique_account(),
            super::rules::TradeProduct::OkxSwap,
            super::request::Method::Get,
            path,
            "BTC-USDT-SWAP",
            Exposure::Neutral,
            vec![],
        )
    };
    let a = build("/api/v5/trade/order?instId=BTC-USDT-SWAP&clOrdId=aaaa");
    let b = build("/api/v5/trade/order?instId=BTC-USDT-SWAP&clOrdId=bbbb");
    // The shown path has no query, the hash still tells the two apart.
    assert_eq!(a.path, "/api/v5/trade/order");
    assert_eq!(a.path, b.path);
    assert_ne!(a.hash, b.hash);
}

// ── The automatic path is bound to the entry confirmation (M5 d3) ───────

mod auto_grant {
    use super::*;
    use crate::trade::grant::{AutoGrant, Basis, GrantRefusal, GrantRoles};

    const ROLES: GrantRoles = GrantRoles {
        stop: true,
        targets: 1,
        trailing: false,
    };

    fn spec() -> crate::trade::protect::ProtectSpec {
        use crate::trade::protect::{Offset, ProtectSpec, TriggerBasis};
        ProtectSpec {
            stop: Some(Offset::Percent(d("1"))),
            targets: vec![crate::trade::protect::TargetSpec {
                offset: Offset::Percent(d("2")),
                share_pct: 100,
            }],
            trailing: None,
            trigger_price: TriggerBasis::Mark,
        }
    }

    fn entry_intent(c: &TradeClient, t: &RulesTable) -> OrderIntent {
        let r = t.lookup("BTCUSDT").unwrap();
        let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
        OrderIntent::binance(
            c.account(),
            futures_limit(r, &ClientId::generate(Role::Entry), Action::Open(Dir::Long)),
            &fresh,
        )
        .unwrap()
    }

    /// A grant for a new entry, the entry's client id and what its placements are based on.
    fn grant_and_basis(c: &TradeClient, t: &RulesTable) -> (AutoGrant, String, Basis) {
        let entry = entry_intent(c, t);
        let id = entry.client_ids()[0].to_string();
        let grant = AutoGrant::from_entry(&entry, ROLES, &spec(), 10_000, NOW).unwrap();
        let basis = Basis::new(&id, &spec());
        (grant, id, basis)
    }

    fn protective(
        c: &TradeClient,
        t: &RulesTable,
        kind: ProtectiveKind,
        size: Size,
        id: &ClientId,
        basis: &Basis,
    ) -> AutoIntent {
        let r = t.lookup("BTCUSDT").unwrap();
        let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
        let p = Protective {
            kind,
            dir: Dir::Long,
            size,
            trigger_price: Some(d(if kind.is_stop_side() {
                "59000"
            } else {
                "61000"
            })),
            limit_price: None,
            tif: TimeInForce::Gtc,
            working_type: WorkingType::MarkPrice,
            trailing: None,
            ref_price: d("60000"),
            client_id: dup(id),
        };
        let place = bf::new_algo_order(r, Some(PositionMode::OneWay), p).unwrap();
        let intent = OrderIntent::binance(c.account(), place, &fresh).unwrap();
        AutoIntent::new(AutoPath::AttachProtective, intent)
            .unwrap()
            .with_basis(basis.clone())
    }

    #[tokio::test]
    async fn only_what_the_grant_covers_reaches_the_server() {
        let fake = serve(gen_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let (mut grant, entry_id, basis) = grant_and_basis(&c, &t);
        assert!(grant.add_fill(&entry_id, d("0.002")));
        let (stop_ref, stop) = grant.issue(Role::StopLoss, NOW).unwrap();
        let (_, tp) = grant.issue(Role::TakeProfit, NOW).unwrap();

        // The stop and the take-profit go out once each.
        for (kind, size, id) in [
            (ProtectiveKind::StopMarket, Size::ClosePosition, &stop),
            (ProtectiveKind::TakeProfitMarket, Size::Qty(d("0.002")), &tp),
        ] {
            let granted = grant
                .authorise(protective(&c, &t, kind, size, id, &basis), NOW)
                .unwrap();
            let out = c
                .submit_auto(&OpenGate, &NoRisk, &NoJournal, granted)
                .await
                .unwrap();
            assert!(matches!(out.outcome, Outcome::Accepted { .. }));
        }
        assert_eq!(fake.count("POST /fapi/v1/algoOrder"), 2);

        // The same stop again, a larger take-profit and a stranger's id never leave the app.
        let again = protective(
            &c,
            &t,
            ProtectiveKind::StopMarket,
            Size::ClosePosition,
            &stop,
            &basis,
        );
        assert_eq!(
            grant.authorise(again, NOW).unwrap_err(),
            GrantRefusal::IdNotAvailable
        );
        let stranger = ClientId::generate(Role::StopLoss);
        let foreign = protective(
            &c,
            &t,
            ProtectiveKind::StopMarket,
            Size::ClosePosition,
            &stranger,
            &basis,
        );
        assert_eq!(
            grant.authorise(foreign, NOW).unwrap_err(),
            GrantRefusal::UnknownId
        );
        assert_eq!(fake.count("POST /fapi/v1/algoOrder"), 2);

        // The sibling cancel: once.
        let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
        let cancel = grant.cancel_intent(stop_ref, &fresh).unwrap();
        let granted = grant.authorise(cancel, NOW).unwrap();
        let out = c
            .submit_auto(&OpenGate, &NoRisk, &NoJournal, granted)
            .await
            .unwrap();
        assert!(matches!(out.outcome, Outcome::Accepted { .. }));
        assert_eq!(fake.count("DELETE /fapi/v1/algoOrder"), 1);
        assert_eq!(
            grant.cancel_intent(stop_ref, &fresh).unwrap_err(),
            GrantRefusal::AlreadyCancelled
        );
        // Nothing reduced, so no request of the run added exposure: the entry never went out.
        assert_eq!(fake.count("POST /fapi/v1/order"), 0);
    }

    #[tokio::test]
    async fn a_closed_gate_stops_a_granted_request_too() {
        let fake = serve(gen_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let (mut grant, entry_id, basis) = grant_and_basis(&c, &t);
        assert!(grant.add_fill(&entry_id, d("0.002")));
        let (_, stop) = grant.issue(Role::StopLoss, NOW).unwrap();
        let granted = grant
            .authorise(
                protective(
                    &c,
                    &t,
                    ProtectiveKind::StopMarket,
                    Size::ClosePosition,
                    &stop,
                    &basis,
                ),
                NOW,
            )
            .unwrap();
        let out = c
            .submit_auto(&ClosedGate, &NoRisk, &NoJournal, granted)
            .await;
        assert!(matches!(out, Err(SubmitError::Gate("closed"))));
        assert_eq!(fake.count("algoOrder"), 0);
    }

    /// One stage as the app sends it: issue the ids, build the requests, authorise each against
    /// the grant and send it. Returns what was issued.
    async fn send_stage(
        c: &TradeClient,
        t: &RulesTable,
        grant: &mut AutoGrant,
        entry_id: &str,
        spec: &crate::trade::protect::ProtectSpec,
        stage: crate::trade::Stage,
        filled: &str,
    ) -> Vec<crate::trade::IssuedRef> {
        let rules = t.lookup("BTCUSDT").unwrap();
        let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
        let mut issued = Vec::new();
        let mut ids = Vec::new();
        for role in crate::trade::stage_roles(spec, stage) {
            let (r, id) = grant.issue(role, NOW).unwrap();
            issued.push(r);
            ids.push((role, id));
        }
        let input = crate::trade::StageInput {
            rules,
            mode: PositionMode::OneWay,
            dir: Dir::Long,
            avg_price: d("60000"),
            filled: d(filled),
            ref_price: d("60000"),
            spec,
            stage,
        };
        for (_, request) in crate::trade::build_stage(&input, ids) {
            let intent = OrderIntent::binance(c.account(), request.unwrap(), &fresh).unwrap();
            let auto = AutoIntent::new(AutoPath::AttachProtective, intent)
                .unwrap()
                .with_basis(input.basis(entry_id));
            let granted = grant.authorise(auto, NOW).unwrap();
            let out = c
                .submit_auto(&OpenGate, &NoRisk, &NoJournal, granted)
                .await
                .unwrap();
            assert!(matches!(out.outcome, Outcome::Accepted { .. }));
        }
        issued
    }

    /// First fill → one whole-position stop; the entry completes → the take-profits for the
    /// total; the stop fires → the take-profits are cancelled, once. Nothing adds exposure.
    #[tokio::test]
    async fn a_partial_fill_sequence_reaches_the_server_as_a_stop_then_the_take_profits_then_the_cancels()
     {
        use crate::trade::Stage;
        use crate::trade::protect::{Offset, ProtectSpec, TargetSpec, TriggerBasis};
        let fake = serve(gen_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
        let entry = OrderIntent::binance(
            c.account(),
            futures_limit_qty(
                t.lookup("BTCUSDT").unwrap(),
                &ClientId::generate(Role::Entry),
                "0.004",
            ),
            &fresh,
        )
        .unwrap();
        let roles = GrantRoles {
            stop: true,
            targets: 2,
            trailing: false,
        };
        let entry_id = entry.client_ids()[0].to_string();
        let spec = ProtectSpec {
            stop: Some(Offset::Percent(d("1"))),
            targets: vec![
                TargetSpec {
                    offset: Offset::Percent(d("2")),
                    share_pct: 50,
                },
                TargetSpec {
                    offset: Offset::Percent(d("4")),
                    share_pct: 50,
                },
            ],
            trailing: None,
            trigger_price: TriggerBasis::Mark,
        };
        let mut grant = AutoGrant::from_entry(&entry, roles, &spec, 10_000, NOW).unwrap();
        // Nothing is authorised before a fill exists.
        let (_, early) = grant.issue(Role::StopLoss, NOW).unwrap();
        let too_early = protective(
            &c,
            &t,
            ProtectiveKind::StopMarket,
            Size::ClosePosition,
            &early,
            &crate::trade::Basis::new(&entry_id, &spec),
        );
        assert_eq!(
            grant.authorise(too_early, NOW).unwrap_err(),
            GrantRefusal::NoFill
        );
        assert_eq!(fake.count("POST /fapi/v1/algoOrder"), 0);
        let mut grant = AutoGrant::from_entry(&entry, roles, &spec, 10_000, NOW).unwrap();

        assert!(grant.add_fill(&entry_id, d("0.001")));
        let stop = send_stage(&c, &t, &mut grant, &entry_id, &spec, Stage::Stop, "0.001").await;
        assert_eq!(fake.count("POST /fapi/v1/algoOrder"), 1);
        assert!(grant.add_fill(&entry_id, d("0.003")));
        let tps = send_stage(&c, &t, &mut grant, &entry_id, &spec, Stage::Rest, "0.004").await;
        assert_eq!(fake.count("POST /fapi/v1/algoOrder"), 3);
        let raw = fake.raw.lock().unwrap().clone();
        let posts: Vec<&String> = raw
            .iter()
            .filter(|(m, p, _)| m == "POST" && p.contains("/fapi/v1/algoOrder"))
            .map(|(_, p, b)| if b.is_empty() { p } else { b })
            .collect();
        assert!(posts[0].contains("closePosition=true"), "{}", posts[0]);
        assert!(posts[1].contains("quantity=0.002") && posts[1].contains("reduceOnly=true"));
        assert!(posts[2].contains("quantity=0.002") && posts[2].contains("reduceOnly=true"));
        // Nothing of the run was an entry.
        assert_eq!(fake.count("POST /fapi/v1/order"), 0);

        // The stop fired: both take-profits are cancelled, each once.
        for r in &tps {
            let cancel = grant.cancel_intent(*r, &fresh).unwrap();
            let granted = grant.authorise(cancel, NOW).unwrap();
            let out = c
                .submit_auto(&OpenGate, &NoRisk, &NoJournal, granted)
                .await
                .unwrap();
            assert!(matches!(out.outcome, Outcome::Accepted { .. }));
        }
        assert_eq!(fake.count("DELETE /fapi/v1/algoOrder"), 2);
        // A second cancel of the same order, and a cancel of a placed one that was never
        // cancelled by the grant's own id list, change nothing on the wire.
        assert_eq!(
            grant.cancel_intent(tps[0], &fresh).unwrap_err(),
            GrantRefusal::AlreadyCancelled
        );
        assert_eq!(fake.count("DELETE /fapi/v1/algoOrder"), 2);
        // The stop is still cancellable once (the take-profits fired instead).
        assert!(grant.cancel_intent(stop[0], &fresh).is_ok());
    }
}

// ── Replacing a protective order: one confirmation, cancel then place (f3a) ──

mod replace_protective {
    use super::*;
    use crate::trade::client::OpenOrder;
    use crate::trade::protect::ProtectRole;
    use crate::trade::replace::{
        REPLACE_GRANT_VALID_MS, ReplaceError, ReplaceGrant, ReplaceKind, ReplaceRefusal,
    };
    use crate::trade::stream::StreamState;

    fn old_row(role: ProtectRole, close_position: bool, qty: Option<&str>) -> OpenOrder {
        OpenOrder {
            symbol: "BTCUSDT".to_string(),
            id: "777".to_string(),
            kind: OpenKind::Algo(None),
            side: Some(Side::Sell),
            position_side: None,
            qty: qty.map(d),
            trigger_price: Some(d("59000")),
            close_position,
            reduce_only: !close_position,
            role: Some(role),
            attached: None,
            client_id: Some("eOLDSTOP1".to_string()),
            limit: false,
            limit_price: None,
            activation_price: None,
            callback_rate: None,
            price: None,
        }
    }

    fn new_order(
        c: &TradeClient,
        t: &RulesTable,
        kind: ProtectiveKind,
        dir: Dir,
        size: Size,
        trigger: &str,
    ) -> OrderIntent {
        let r = t.lookup("BTCUSDT").unwrap();
        let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
        let p = Protective {
            kind,
            dir,
            size,
            trigger_price: Some(d(trigger)),
            limit_price: None,
            tif: TimeInForce::Gtc,
            working_type: WorkingType::MarkPrice,
            trailing: None,
            ref_price: d("60000"),
            client_id: ClientId::generate(Role::StopLoss),
        };
        let req = bf::new_algo_order(r, Some(PositionMode::OneWay), p).unwrap();
        OrderIntent::binance(c.account(), req, &fresh).unwrap()
    }

    fn stop(c: &TradeClient, t: &RulesTable, trigger: &str) -> OrderIntent {
        new_order(
            c,
            t,
            ProtectiveKind::StopMarket,
            Dir::Long,
            Size::ClosePosition,
            trigger,
        )
    }

    fn grant_for(
        c: &TradeClient,
        t: &RulesTable,
        old: &OpenOrder,
        new: &OrderIntent,
    ) -> ReplaceGrant {
        let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
        c.replace_grant(
            ConfirmToken::for_replace(&new.preview(), old),
            c.account(),
            old,
            &fresh,
        )
        .unwrap()
    }

    async fn run(
        c: &TradeClient,
        grant: ReplaceGrant,
        new: OrderIntent,
    ) -> Result<crate::trade::replace::ReplaceDone, ReplaceError<()>> {
        c.submit_replace(&OpenGate, &NoRisk, &NoJournal, grant, new)
            .await
    }

    /// The cancel names the old order (id 777), as the exchange does for a cancel it took.
    fn replace_binance(m: &str, p: &str) -> (u16, String) {
        if m == "DELETE" {
            (200, json!({"algoId": 777, "clientAlgoId": "x"}).to_string())
        } else {
            gen_binance(m, p)
        }
    }

    fn replace_okx(m: &str, p: &str) -> (u16, String) {
        if p.contains("cancel-algos") {
            (
                200,
                json!({"code": "0", "msg": "", "data": [
                    {"sCode": "0", "sMsg": "", "algoId": "777"}
                ]})
                .to_string(),
            )
        } else {
            gen_okx(m, p)
        }
    }

    fn deleted_then_posted(fake: &Fake) -> (usize, usize) {
        (
            fake.count("DELETE /fapi/v1/algoOrder"),
            fake.count("POST /fapi/v1/algoOrder"),
        )
    }

    #[tokio::test]
    async fn a_completed_replace_carries_the_proof_for_the_automatic_grant() {
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, true, None);
        let new = stop(&c, &t, "58000");
        let new_id = new.client_ids()[0].to_string();
        let grant = grant_for(&c, &t, &old, &new);
        let done = run(&c, grant, new).await.unwrap();
        let r = done.replacement.expect("a binance replace has a proof");
        assert_eq!(r.old_client_id(), "eOLDSTOP1");
        assert_eq!(r.new_client_id(), new_id);
        assert!(!format!("{r:?}").contains(&new_id));
    }

    #[tokio::test]
    async fn a_replace_of_an_order_without_a_client_id_has_no_proof() {
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let mut old = old_row(ProtectRole::Stop, true, None);
        old.client_id = None;
        let new = stop(&c, &t, "58000");
        let grant = grant_for(&c, &t, &old, &new);
        assert!(run(&c, grant, new).await.unwrap().replacement.is_none());
    }

    #[tokio::test]
    async fn cancels_first_and_places_after_the_cancel_was_confirmed() {
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, true, None);
        let new = stop(&c, &t, "58000");
        let grant = grant_for(&c, &t, &old, &new);
        assert_eq!(grant.kind(), ReplaceKind::Stop);
        let done = run(&c, grant, new).await.unwrap();
        assert!(matches!(done.cancel.outcome, Outcome::Accepted { .. }));
        assert!(matches!(done.new.outcome, Outcome::Accepted { .. }));
        // The cancel is on the wire first, for the old id, then the new order.
        let log = fake.all();
        assert_eq!(
            log,
            vec![
                "DELETE /fapi/v1/algoOrder".to_string(),
                "POST /fapi/v1/algoOrder".to_string()
            ]
        );
        let raw = fake.raw();
        let cancel = if raw[0].2.is_empty() {
            &raw[0].1
        } else {
            &raw[0].2
        };
        assert!(cancel.contains("algoId=777"), "{cancel}");
    }

    #[tokio::test]
    async fn a_new_order_that_is_not_the_confirmed_one_is_refused_before_anything_is_sent() {
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, true, None);
        let confirmed = stop(&c, &t, "58000");
        let other = stop(&c, &t, "57000");
        let grant = grant_for(&c, &t, &old, &confirmed);
        let e = run(&c, grant, other).await.unwrap_err();
        assert!(matches!(
            e,
            ReplaceError::Refused(ReplaceRefusal::NewOrderMismatch)
        ));
        assert_eq!(deleted_then_posted(&fake), (0, 0));
    }

    #[tokio::test]
    async fn the_old_order_must_be_a_readable_stop_or_target() {
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let new = stop(&c, &t, "58000");
        let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
        let make = |old: &OpenOrder| {
            c.replace_grant(
                ConfirmToken::for_replace(&new.preview(), old),
                c.account(),
                old,
                &fresh,
            )
            .map(|_| ())
            .unwrap_err()
        };
        // A trailing stop, an order with both legs and a plain order are not replaced.
        assert_eq!(
            make(&old_row(ProtectRole::Trailing, true, None)),
            ReplaceRefusal::NotReplaceable
        );
        assert_eq!(
            make(&old_row(ProtectRole::StopAndTarget, true, None)),
            ReplaceRefusal::NotReplaceable
        );
        let mut plain = old_row(ProtectRole::Stop, true, None);
        plain.kind = OpenKind::Plain;
        assert_eq!(make(&plain), ReplaceRefusal::NotReplaceable);
        // An id that is not a number, a missing side, a missing size.
        let mut bad_id = old_row(ProtectRole::Stop, true, None);
        bad_id.id = "12a".to_string();
        assert_eq!(make(&bad_id), ReplaceRefusal::OldOrderUnreadable);
        let mut no_side = old_row(ProtectRole::Stop, true, None);
        no_side.side = None;
        assert_eq!(make(&no_side), ReplaceRefusal::OldOrderUnreadable);
        assert_eq!(
            make(&old_row(ProtectRole::Stop, false, None)),
            ReplaceRefusal::OldOrderUnreadable
        );
        let mut other_symbol = old_row(ProtectRole::Stop, true, None);
        other_symbol.symbol = "ETHUSDT".to_string();
        assert_eq!(make(&other_symbol), ReplaceRefusal::WrongSymbol);
        // A bulk confirmation is not an order confirmation.
        let bulk = ConfirmToken::for_bulk(c.account(), BulkAction::CancelAll);
        let e = c
            .replace_grant(
                bulk,
                c.account(),
                &old_row(ProtectRole::Stop, true, None),
                &fresh,
            )
            .map(|_| ())
            .unwrap_err();
        assert_eq!(e, ReplaceRefusal::NotAReplaceConfirmation);
        assert_eq!(deleted_then_posted(&fake), (0, 0));
    }

    async fn cancel_fails(handler: fn(&str, &str) -> (u16, String)) {
        let fake = serve(handler);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, true, None);
        let new = stop(&c, &t, "58000");
        let grant = grant_for(&c, &t, &old, &new);
        let e = run(&c, grant, new).await.unwrap_err();
        let ReplaceError::CancelNotConfirmed(sub) = e else {
            panic!("{e:?}");
        };
        assert!(!matches!(sub.outcome, Outcome::Accepted { .. }));
        // The cancel went out once; the new order never did.
        assert_eq!(deleted_then_posted(&fake), (1, 0));
    }

    #[tokio::test]
    async fn a_cancel_that_found_no_order_voids_the_grant() {
        // The old order already triggered or filled: the exchange has no such order.
        cancel_fails(|m, p| {
            if m == "DELETE" {
                (
                    400,
                    json!({"code": -2011, "msg": "Unknown order sent."}).to_string(),
                )
            } else {
                gen_binance(m, p)
            }
        })
        .await;
    }

    #[tokio::test]
    async fn a_cancel_whose_result_is_unknown_voids_the_grant() {
        cancel_fails(|m, p| {
            if m == "DELETE" {
                (
                    500,
                    json!({"code": -1006, "msg": "Unexpected response."}).to_string(),
                )
            } else {
                gen_binance(m, p)
            }
        })
        .await;
    }

    #[tokio::test]
    async fn a_cancel_that_is_rejected_voids_the_grant() {
        cancel_fails(|m, p| {
            if m == "DELETE" {
                (400, json!({"code": -4130, "msg": "refused"}).to_string())
            } else {
                gen_binance(m, p)
            }
        })
        .await;
    }

    #[tokio::test]
    async fn a_cancel_that_never_left_voids_the_grant() {
        // 200 without an id: the result is unknown, not a confirmation.
        cancel_fails(|m, p| {
            if m == "DELETE" {
                (200, json!({}).to_string())
            } else {
                gen_binance(m, p)
            }
        })
        .await;
    }

    #[tokio::test]
    async fn a_confirmed_cancel_with_a_refused_new_order_says_the_old_one_is_gone() {
        let fake = serve(|m, p| {
            if m == "POST" {
                (
                    400,
                    json!({"code": -2021, "msg": "Order would immediately trigger."}).to_string(),
                )
            } else {
                replace_binance(m, p)
            }
        });
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, true, None);
        let new = stop(&c, &t, "58000");
        let grant = grant_for(&c, &t, &old, &new);
        let e = run(&c, grant, new).await.unwrap_err();
        let ReplaceError::NewNotPlaced { cancel, new } = e else {
            panic!("{e:?}");
        };
        assert!(matches!(cancel.outcome, Outcome::Accepted { .. }));
        assert!(matches!(
            new.unwrap().outcome,
            Outcome::Rejected {
                kind: RejectKind::Exchange,
                ..
            }
        ));
        // Not repeated.
        assert_eq!(deleted_then_posted(&fake), (1, 1));
    }

    #[tokio::test]
    async fn a_second_grant_for_the_same_old_order_cannot_place_a_second_new_order() {
        let cancels = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&cancels);
        let fake = serve(move |m, p| {
            if m == "DELETE" && seen.fetch_add(1, Ordering::SeqCst) > 0 {
                (
                    400,
                    json!({"code": -2011, "msg": "Unknown order sent."}).to_string(),
                )
            } else {
                replace_binance(m, p)
            }
        });
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, true, None);
        let first = stop(&c, &t, "58000");
        let grant = grant_for(&c, &t, &old, &first);
        run(&c, grant, first).await.unwrap();
        // The same drag confirmed again: the old order is gone, so no new order follows.
        let second = stop(&c, &t, "57000");
        let grant = grant_for(&c, &t, &old, &second);
        let e = run(&c, grant, second).await.unwrap_err();
        assert!(matches!(e, ReplaceError::CancelNotConfirmed(_)), "{e:?}");
        assert_eq!(deleted_then_posted(&fake), (2, 1));
    }

    #[tokio::test]
    async fn an_expired_grant_sends_nothing() {
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, true, None);
        let new = stop(&c, &t, "58000");
        // Made so long ago that it ran out one millisecond ago.
        let grant = grant_for(&c, &t, &old, &new);
        c.test_elapsed_ms
            .fetch_add(REPLACE_GRANT_VALID_MS + 1, Ordering::Relaxed);
        let e = run(&c, grant, new).await.unwrap_err();
        assert!(matches!(e, ReplaceError::Refused(ReplaceRefusal::Expired)));
        assert_eq!(deleted_then_posted(&fake), (0, 0));
        // At the last moment it is still good (a client of its own: its clock starts again).
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let new = stop(&c, &t, "58000");
        let grant = grant_for(&c, &t, &old, &new);
        c.test_elapsed_ms
            .fetch_add(REPLACE_GRANT_VALID_MS, Ordering::Relaxed);
        assert!(run(&c, grant, new).await.is_ok());
    }

    #[tokio::test]
    async fn a_stream_that_is_not_connected_refuses_both_legs() {
        for st in [
            StreamState::Reconnecting,
            StreamState::Resyncing,
            StreamState::Unavailable,
        ] {
            let fake = serve(replace_binance);
            let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
            let t = futures();
            let old = old_row(ProtectRole::Stop, true, None);
            let new = stop(&c, &t, "58000");
            let grant = grant_for(&c, &t, &old, &new);
            set_stream(&c, st);
            let e = run(&c, grant, new).await.unwrap_err();
            assert!(
                matches!(e, ReplaceError::NotSent(SubmitError::StreamNotReady(s)) if s == st),
                "{st:?}: {e:?}"
            );
            // Not even the cancel: a plain cancel of the same order is still allowed.
            assert_eq!(deleted_then_posted(&fake), (0, 0), "{st:?}");
        }
    }

    #[tokio::test]
    async fn a_closed_gate_sends_nothing_not_even_the_cancel() {
        // The F2 gate refuses a Live account while LIVE sending is off (its own table test in
        // `data::config::trade_gate` covers `Reduce` with the switch off); here the client is a
        // Live one and the gate says no.
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, true, None);
        let new = stop(&c, &t, "58000");
        let grant = grant_for(&c, &t, &old, &new);
        let e = c
            .submit_replace(&ClosedGate, &NoRisk, &NoJournal, grant, new)
            .await
            .unwrap_err();
        assert!(matches!(
            e,
            ReplaceError::NotSent(SubmitError::Gate("closed"))
        ));
        assert_eq!(deleted_then_posted(&fake), (0, 0));
    }

    /// Refuses its second question, the new order's.
    struct RefusesSecond(AtomicUsize);
    impl SendGate for RefusesSecond {
        type Refusal = &'static str;
        fn check(&self, _: GateRequest) -> Result<(), &'static str> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(())
            } else {
                Err("second")
            }
        }
    }

    #[tokio::test]
    async fn a_refusal_of_the_new_order_comes_before_the_cancel_is_sent() {
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, true, None);
        let new = stop(&c, &t, "58000");
        let grant = grant_for(&c, &t, &old, &new);
        let gate = RefusesSecond(AtomicUsize::new(0));
        let e = c
            .submit_replace(&gate, &NoRisk, &NoJournal, grant, new)
            .await
            .unwrap_err();
        assert!(matches!(
            e,
            ReplaceError::NotSent(SubmitError::Gate("second"))
        ));
        assert_eq!(deleted_then_posted(&fake), (0, 0));
    }

    #[tokio::test]
    async fn the_new_order_must_be_the_same_kind_side_and_size_and_may_not_add() {
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let stop_row = old_row(ProtectRole::Stop, true, None);
        let part_row = old_row(ProtectRole::Stop, false, Some("0.002"));
        let cases: Vec<(&OpenOrder, OrderIntent, ReplaceRefusal)> = vec![
            // A take-profit for a stop.
            (
                &stop_row,
                new_order(
                    &c,
                    &t,
                    ProtectiveKind::TakeProfitMarket,
                    Dir::Long,
                    Size::ClosePosition,
                    "61000",
                ),
                ReplaceRefusal::WrongKind,
            ),
            // The other side (a protective order for a short, which buys): a reversal.
            (
                &stop_row,
                new_order(
                    &c,
                    &t,
                    ProtectiveKind::StopMarket,
                    Dir::Short,
                    Size::ClosePosition,
                    "61000",
                ),
                ReplaceRefusal::WrongSide,
            ),
            // More than the old order covered.
            (
                &part_row,
                new_order(
                    &c,
                    &t,
                    ProtectiveKind::StopMarket,
                    Dir::Long,
                    Size::Qty(d("0.003")),
                    "58000",
                ),
                ReplaceRefusal::SizeChanged,
            ),
            // A whole-position order for a partial one.
            (
                &part_row,
                stop(&c, &t, "58000"),
                ReplaceRefusal::SizeChanged,
            ),
        ];
        for (old, new, want) in cases {
            let grant = grant_for(&c, &t, old, &new);
            let e = run(&c, grant, new).await.unwrap_err();
            assert!(
                matches!(&e, ReplaceError::Refused(r) if *r == want),
                "{want:?}: {e:?}"
            );
        }
        // A request that adds exposure (a limit entry) is not a replacement.
        let r = t.lookup("BTCUSDT").unwrap();
        let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
        let entry = OrderIntent::binance(
            c.account(),
            futures_limit(r, &ClientId::generate(Role::Entry), Action::Open(Dir::Long)),
            &fresh,
        )
        .unwrap();
        let grant = grant_for(&c, &t, &stop_row, &entry);
        let e = run(&c, grant, entry).await.unwrap_err();
        assert!(matches!(
            e,
            ReplaceError::Refused(ReplaceRefusal::NotReducing)
        ));
        // A position side that the old order did not have.
        let mut hedged = old_row(ProtectRole::Stop, true, None);
        hedged.position_side = Some(Dir::Long);
        let new = stop(&c, &t, "58000");
        let grant = grant_for(&c, &t, &hedged, &new);
        let e = run(&c, grant, new).await.unwrap_err();
        assert!(matches!(
            e,
            ReplaceError::Refused(ReplaceRefusal::WrongPositionSide)
        ));
        assert_eq!(deleted_then_posted(&fake), (0, 0));
    }

    #[tokio::test]
    async fn the_grant_belongs_to_one_account() {
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let other = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, true, None);
        let new = stop(&other, &t, "58000");
        // The grant was made for `other`, the client that sends is `c`.
        let grant = grant_for(&other, &t, &old, &new);
        let e = run(&c, grant, new).await.unwrap_err();
        assert!(
            matches!(
                e,
                ReplaceError::NotSent(SubmitError::AccountMismatch)
                    | ReplaceError::Refused(ReplaceRefusal::WrongAccount)
            ),
            "{e:?}"
        );
        assert_eq!(deleted_then_posted(&fake), (0, 0));
    }

    #[tokio::test]
    async fn an_okx_swap_stop_is_replaced_by_cancel_algos_then_order_algo() {
        let fake = serve(replace_okx);
        let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        let fresh = t.fresh_lookup("BTC-USDT-SWAP", NOW, AGE).unwrap();
        let place = |trigger: &str| {
            let req = okx::place_algo(OkxProtective {
                rules: r,
                target: OkxTarget::Swap {
                    mode: Some(PositionMode::OneWay),
                    margin: MarginMode::Cross,
                    action: Action::Close(Dir::Long),
                },
                ord_type: OkxAlgoType::Conditional,
                size: Size::ClosePosition,
                tp_trigger: None,
                sl_trigger: Some(d(trigger)),
                trigger_type: TriggerPxType::Last,
                ref_price: d("60000"),
                client_id: ClientId::generate(Role::StopLoss),
            })
            .unwrap();
            OrderIntent::okx(c.account(), req, &fresh).unwrap()
        };
        let mut old = old_row(ProtectRole::Stop, true, None);
        old.symbol = "BTC-USDT-SWAP".to_string();
        old.kind = OpenKind::Algo(Some(OkxAlgoType::Conditional));
        let new = place("58000");
        let grant = c
            .replace_grant(
                ConfirmToken::for_replace(&new.preview(), &old),
                c.account(),
                &old,
                &fresh,
            )
            .unwrap();
        run(&c, grant, new).await.unwrap();
        assert_eq!(
            fake.all(),
            vec![
                "POST /api/v5/trade/cancel-algos".to_string(),
                "POST /api/v5/trade/order-algo".to_string()
            ]
        );
        // A take-profit for the stop is refused before anything is sent.
        let tp = okx::place_algo(OkxProtective {
            rules: r,
            target: OkxTarget::Swap {
                mode: Some(PositionMode::OneWay),
                margin: MarginMode::Cross,
                action: Action::Close(Dir::Long),
            },
            ord_type: OkxAlgoType::Conditional,
            size: Size::ClosePosition,
            tp_trigger: Some(d("61000")),
            sl_trigger: None,
            trigger_type: TriggerPxType::Last,
            ref_price: d("60000"),
            client_id: ClientId::generate(Role::TakeProfit),
        })
        .unwrap();
        let tp = OrderIntent::okx(c.account(), tp, &fresh).unwrap();
        let grant = c
            .replace_grant(
                ConfirmToken::for_replace(&tp.preview(), &old),
                c.account(),
                &old,
                &fresh,
            )
            .unwrap();
        let e = run(&c, grant, tp).await.unwrap_err();
        assert!(matches!(
            e,
            ReplaceError::Refused(ReplaceRefusal::WrongKind)
        ));
        assert_eq!(fake.all().len(), 2);
    }

    #[tokio::test]
    async fn an_order_confirmation_is_not_a_replace_confirmation() {
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, true, None);
        let new = stop(&c, &t, "58000");
        let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
        // The confirmation of "add this stop" cannot make a grant that cancels another one.
        let e = c
            .replace_grant(
                ConfirmToken::for_preview(&new.preview()),
                c.account(),
                &old,
                &fresh,
            )
            .map(|_| ())
            .unwrap_err();
        assert_eq!(e, ReplaceRefusal::NotAReplaceConfirmation);
        assert_eq!(deleted_then_posted(&fake), (0, 0));
    }

    #[tokio::test]
    async fn a_replace_confirmation_names_one_old_order() {
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, true, None);
        let mut other = old_row(ProtectRole::Stop, true, None);
        other.id = "778".to_string();
        let new = stop(&c, &t, "58000");
        let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
        // Confirmed for 777, offered 778: another order of the same position and side.
        let e = c
            .replace_grant(
                ConfirmToken::for_replace(&new.preview(), &old),
                c.account(),
                &other,
                &fresh,
            )
            .map(|_| ())
            .unwrap_err();
        assert_eq!(e, ReplaceRefusal::OldOrderMismatch);
        assert_eq!(deleted_then_posted(&fake), (0, 0));
    }

    #[tokio::test]
    async fn a_replace_confirmation_is_for_the_old_row_it_saw_and_not_only_its_id() {
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, false, Some("0.01"));
        let new = stop(&c, &t, "58000");
        let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
        // The same exchange id, read with another size, side, position side, kind or flag.
        let mut bigger = old.clone();
        bigger.qty = bigger
            .qty
            .map(|_| crate::trade::Decimal::parse("100").unwrap());
        let mut other_side = old.clone();
        other_side.side = other_side.side.map(|s| match s {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        });
        let mut hedged = old.clone();
        hedged.position_side = Some(Dir::Long);
        let mut whole = old.clone();
        whole.close_position = true;
        let mut target = old.clone();
        target.role = Some(ProtectRole::Target);
        // The same exchange id under another client id (a mixed-up row): the adoption would
        // follow the wrong order.
        let mut renamed = old.clone();
        renamed.client_id = Some("sSOMEOTHERORDERIDXXXX".to_string());
        let mut unnamed = old.clone();
        unnamed.client_id = None;
        for changed in [bigger, other_side, hedged, whole, target, renamed, unnamed] {
            let e = c
                .replace_grant(
                    ConfirmToken::for_replace(&new.preview(), &old),
                    c.account(),
                    &changed,
                    &fresh,
                )
                .map(|_| ())
                .unwrap_err();
            assert_eq!(e, ReplaceRefusal::OldOrderMismatch);
        }
        // The row as it was confirmed is accepted.
        assert!(
            c.replace_grant(
                ConfirmToken::for_replace(&new.preview(), &old),
                c.account(),
                &old,
                &fresh,
            )
            .is_ok()
        );
        assert_eq!(deleted_then_posted(&fake), (0, 0));
    }

    #[tokio::test]
    async fn a_replace_confirmation_is_not_a_bulk_confirmation() {
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, true, None);
        let new = stop(&c, &t, "58000");
        let e = c
            .cancel_all(
                &OpenGate,
                &NoRisk,
                &NoJournal,
                ConfirmToken::for_replace(&new.preview(), &old),
                &t,
            )
            .await
            .unwrap_err();
        assert!(matches!(e, crate::trade::BulkError::ConfirmationMismatch));
        let e = c
            .flatten(
                &OpenGate,
                &NoRisk,
                &NoJournal,
                ConfirmToken::for_replace(&new.preview(), &old),
                &t,
                MarginMode::Cross,
            )
            .await
            .unwrap_err();
        assert!(matches!(e, crate::trade::BulkError::ConfirmationMismatch));
        assert_eq!(fake.count("DELETE /fapi/v1/algoOrder"), 0);
        assert_eq!(fake.count("POST /fapi/v1/algoOrder"), 0);
        assert_eq!(fake.count("DELETE /fapi/v1/allOpenOrders"), 0);
        assert_eq!(fake.count("DELETE /fapi/v1/algoOpenOrders"), 0);
        assert_eq!(fake.count("POST /fapi/v1/order"), 0);
    }

    #[tokio::test]
    async fn a_replace_confirmation_does_not_send_a_plain_order() {
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, true, None);
        let new = stop(&c, &t, "58000");
        let token = ConfirmToken::for_replace(&new.preview(), &old);
        let e = c
            .submit(&OpenGate, &NoRisk, &NoJournal, token, new)
            .await
            .unwrap_err();
        assert!(matches!(e, SubmitError::ConfirmationMismatch));
        assert_eq!(deleted_then_posted(&fake), (0, 0));
    }

    #[tokio::test]
    async fn the_cancel_can_be_previewed_before_the_confirmation() {
        let fake = serve(replace_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, true, None);
        let new = stop(&c, &t, "58000");
        let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
        let before = ReplaceGrant::cancel_preview_for(c.account(), &old, &fresh).unwrap();
        let grant = grant_for(&c, &t, &old, &new);
        assert_eq!(before.hash, grant.cancel_preview().hash);
        let mut plain = old_row(ProtectRole::Trailing, true, None);
        plain.id = "1".to_string();
        assert_eq!(
            ReplaceGrant::cancel_preview_for(c.account(), &plain, &fresh).map(|_| ()),
            Err(ReplaceRefusal::NotReplaceable)
        );
        assert_eq!(deleted_then_posted(&fake), (0, 0));
    }

    #[tokio::test]
    async fn a_cancel_accepted_without_the_old_id_is_not_a_confirmed_cancel() {
        // Binance answers for another order id: not a confirmed cancel of the old order.
        let fake = serve(gen_binance);
        let c = client(AccountVenue::Binance, AccountProduct::UsdM, &fake);
        let t = futures();
        let old = old_row(ProtectRole::Stop, true, None);
        let new = stop(&c, &t, "58000");
        let grant = grant_for(&c, &t, &old, &new);
        let e = run(&c, grant, new).await.unwrap_err();
        assert!(matches!(e, ReplaceError::CancelNotConfirmed(_)), "{e:?}");
        assert_eq!(deleted_then_posted(&fake), (1, 0));
    }

    async fn okx_cancel_fails(handler: fn(&str, &str) -> (u16, String)) {
        let fake = serve(handler);
        let c = client(AccountVenue::Okx, AccountProduct::UsdM, &fake);
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        let fresh = t.fresh_lookup("BTC-USDT-SWAP", NOW, AGE).unwrap();
        let req = okx::place_algo(OkxProtective {
            rules: r,
            target: OkxTarget::Swap {
                mode: Some(PositionMode::OneWay),
                margin: MarginMode::Cross,
                action: Action::Close(Dir::Long),
            },
            ord_type: OkxAlgoType::Conditional,
            size: Size::ClosePosition,
            tp_trigger: None,
            sl_trigger: Some(d("58000")),
            trigger_type: TriggerPxType::Last,
            ref_price: d("60000"),
            client_id: ClientId::generate(Role::StopLoss),
        })
        .unwrap();
        let new = OrderIntent::okx(c.account(), req, &fresh).unwrap();
        let mut old = old_row(ProtectRole::Stop, true, None);
        old.symbol = "BTC-USDT-SWAP".to_string();
        old.kind = OpenKind::Algo(Some(OkxAlgoType::Conditional));
        let grant = c
            .replace_grant(
                ConfirmToken::for_replace(&new.preview(), &old),
                c.account(),
                &old,
                &fresh,
            )
            .unwrap();
        let e = run(&c, grant, new).await.unwrap_err();
        let ReplaceError::CancelNotConfirmed(sub) = e else {
            panic!("{e:?}");
        };
        assert!(!matches!(sub.outcome, Outcome::Accepted { ids } if ids.contains(&"777".into())));
        // The cancel went out once; the new order never did.
        assert_eq!(
            fake.all(),
            vec!["POST /api/v5/trade/cancel-algos".to_string()]
        );
    }

    #[tokio::test]
    async fn an_okx_cancel_of_an_order_that_is_gone_voids_the_grant() {
        okx_cancel_fails(|_, _| {
            (
                200,
                json!({"code": "1", "msg": "", "data": [
                    {"sCode": "51603", "sMsg": "Order does not exist", "algoId": "777"}
                ]})
                .to_string(),
            )
        })
        .await;
    }

    #[tokio::test]
    async fn an_okx_cancel_without_an_algo_id_voids_the_grant() {
        // `sCode 0` and no id: not a confirmed cancel of the old order.
        okx_cancel_fails(|_, _| {
            (
                200,
                json!({"code": "0", "msg": "", "data": [{"sCode": "0", "sMsg": ""}]}).to_string(),
            )
        })
        .await;
    }
}
