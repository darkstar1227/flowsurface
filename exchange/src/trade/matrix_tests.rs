//! Condition 67: every order type that can be placed can also be cancelled and queried.
//! One row per type; each row builds the place, cancel and query request.

use super::binance_futures as bf;
use super::binance_spot as bs;
use super::client_id::ClientId;
use super::okx::{self, OkxAlgoType, OkxProtective, OkxTarget, TriggerPxType};
use super::rules::fixtures::*;
use super::types::*;
use crate::account::PositionMode;

fn id(s: &str) -> ClientId {
    ClientId::parse(s).unwrap()
}

#[test]
fn binance_futures_every_type_has_place_cancel_and_query() {
    let t = futures();
    let r = t.lookup("BTCUSDT").unwrap();
    let cid = id("eAAAA");
    let by_client = OrderRef::Client(id("eAAAA"));
    for mode in [PositionMode::OneWay, PositionMode::Hedge] {
        // LIMIT, MARKET -> /fapi/v1/order
        for exec in [
            Exec::Limit {
                price: d("60000"),
                tif: TimeInForce::Gtc,
            },
            Exec::Market,
        ] {
            let e = bf::FuturesEntry {
                rules: r,
                mode: Some(mode),
                action: Action::Open(Dir::Long),
                qty: d("0.002"),
                exec,
                ref_price: Some(d("60000")),
                client_id: &cid,
            };
            assert_eq!(bf::new_order(&e).unwrap().path(), "/fapi/v1/order");
            assert_eq!(
                bf::cancel_order(r, &by_client).unwrap().path(),
                "/fapi/v1/order"
            );
            assert_eq!(
                bf::query_order(r, &by_client).unwrap().path(),
                "/fapi/v1/order"
            );
        }
        // algo types -> /fapi/v1/algoOrder
        let kinds: &[ProtectiveKind] = match mode {
            PositionMode::OneWay => &[
                ProtectiveKind::StopMarket,
                ProtectiveKind::TakeProfitMarket,
                ProtectiveKind::StopLimit,
                ProtectiveKind::TakeProfitLimit,
                ProtectiveKind::Trailing,
            ],
            // hedge: full-close market orders only
            PositionMode::Hedge => &[ProtectiveKind::StopMarket, ProtectiveKind::TakeProfitMarket],
        };
        for &kind in kinds {
            let stop_side = kind.is_stop_side();
            let p = Protective {
                kind,
                dir: Dir::Long,
                size: if mode == PositionMode::Hedge {
                    Size::ClosePosition
                } else {
                    Size::Qty(d("0.002"))
                },
                trigger_price: (kind != ProtectiveKind::Trailing)
                    .then(|| d(if stop_side { "59000" } else { "61000" })),
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
            assert_eq!(
                bf::new_algo_order(r, Some(mode), &p).unwrap().path(),
                "/fapi/v1/algoOrder"
            );
            assert_eq!(
                bf::cancel_algo_order(&by_client).unwrap().path(),
                "/fapi/v1/algoOrder"
            );
            assert_eq!(
                bf::query_algo_order(&by_client).unwrap().path(),
                "/fapi/v1/algoOrder"
            );
        }
    }
}

#[test]
fn binance_spot_every_type_has_place_cancel_and_query() {
    let t = spot();
    let r = t.lookup("BTCUSDT").unwrap();
    let cid = id("eAAAA");
    let by_client = OrderRef::Client(id("eAAAA"));
    // single orders: LIMIT, MARKET, STOP_LOSS(_LIMIT), TAKE_PROFIT(_LIMIT), trailingDelta
    for exec in [
        Exec::Limit {
            price: d("60000"),
            tif: TimeInForce::Gtc,
        },
        Exec::Market,
    ] {
        let e = bs::SpotEntry {
            rules: r,
            side: Side::Buy,
            qty: d("0.1"),
            exec,
            ref_price: Some(d("60000")),
            client_id: &cid,
        };
        assert_eq!(bs::new_order(&e).unwrap().path(), "/api/v3/order");
    }
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
        assert_eq!(
            bs::build_protective_order(r, &p).unwrap().path(),
            "/api/v3/order"
        );
    }
    assert_eq!(
        bs::cancel_order(r, &by_client).unwrap().path(),
        "/api/v3/order"
    );
    assert_eq!(
        bs::query_order(r, &by_client).unwrap().path(),
        "/api/v3/order"
    );
    // order lists: OCO, OTOCO
    let (list, tp, sl, entry) = (id("lAAAA"), id("tAAAA"), id("sAAAA"), id("eBBBB"));
    let exits = bs::SpotExits {
        take_profit_price: d("61000"),
        stop_trigger: d("59000"),
        stop_limit_price: None,
        stop_tif: TimeInForce::Gtc,
        take_profit_id: &tp,
        stop_id: &sl,
    };
    let oco = bs::SpotOco {
        rules: r,
        qty: d("0.1"),
        ref_price: d("60000"),
        exits,
        list_id: &list,
    };
    assert_eq!(bs::build_oco(&oco).unwrap().path(), "/api/v3/orderList/oco");
    let otoco = bs::SpotOtoco {
        rules: r,
        qty: d("0.1"),
        entry_price: d("60000"),
        entry_tif: TimeInForce::Gtc,
        exits,
        list_id: &list,
        entry_id: &entry,
    };
    assert_eq!(
        bs::build_otoco(&otoco).unwrap().path(),
        "/api/v3/orderList/otoco"
    );
    assert_eq!(
        bs::cancel_order_list(r, &by_client).unwrap().path(),
        "/api/v3/orderList"
    );
    assert_eq!(
        bs::query_order_list(&by_client).unwrap().path(),
        "/api/v3/orderList"
    );
}

#[test]
fn okx_every_type_has_place_cancel_and_query() {
    let st = okx_swap_table();
    let sr = st.lookup("BTC-USDT-SWAP").unwrap();
    let pt = okx_spot_table();
    let pr = pt.lookup("BTC-USDT").unwrap();
    let cid = id("eAAAA");
    let by_client = OrderRef::Client(id("eAAAA"));

    for mode in [PositionMode::OneWay, PositionMode::Hedge] {
        let target = |action| OkxTarget::Swap {
            mode: Some(mode),
            margin: MarginMode::Cross,
            action,
        };
        for exec in [
            Exec::Limit {
                price: d("60000"),
                tif: TimeInForce::Gtc,
            },
            Exec::Market,
        ] {
            let e = okx::OkxEntry {
                rules: sr,
                target: target(Action::Open(Dir::Long)),
                qty: d("2"),
                exec,
                ref_price: Some(d("60000")),
                client_id: &cid,
                attach: None,
            };
            assert_eq!(okx::place_order(&e).unwrap().path(), "/api/v5/trade/order");
        }
        let kinds = [
            (OkxAlgoType::Conditional, None, Some("59000")),
            (OkxAlgoType::Conditional, Some("61000"), None),
            (OkxAlgoType::Oco, Some("61000"), Some("59000")),
        ];
        for (ord_type, tp, sl) in kinds {
            let p = OkxProtective {
                rules: sr,
                target: target(Action::Close(Dir::Long)),
                ord_type,
                size: Size::ClosePosition,
                tp_trigger: tp.map(d),
                sl_trigger: sl.map(d),
                trigger_type: TriggerPxType::Last,
                ref_price: d("60000"),
                client_id: &cid,
            };
            assert_eq!(
                okx::place_algo(&p).unwrap().path(),
                "/api/v5/trade/order-algo"
            );
            assert_eq!(
                okx::cancel_algos(sr, ord_type, std::slice::from_ref(&by_client))
                    .unwrap()
                    .path(),
                "/api/v5/trade/cancel-algos"
            );
            assert!(
                okx::query_algo(ord_type, &by_client)
                    .unwrap()
                    .path()
                    .starts_with("/api/v5/trade/order-algo?")
            );
        }
        assert_eq!(
            okx::cancel_order(sr, &by_client).unwrap().path(),
            "/api/v5/trade/cancel-order"
        );
        assert!(
            okx::query_order(sr, &by_client)
                .unwrap()
                .path()
                .starts_with("/api/v5/trade/order?")
        );
    }

    // spot
    for exec in [
        Exec::Limit {
            price: d("60000"),
            tif: TimeInForce::Gtc,
        },
        Exec::Market,
    ] {
        let e = okx::OkxEntry {
            rules: pr,
            target: OkxTarget::Spot { side: Side::Buy },
            qty: d("0.1"),
            exec,
            ref_price: Some(d("60000")),
            client_id: &cid,
            attach: None,
        };
        assert_eq!(okx::place_order(&e).unwrap().path(), "/api/v5/trade/order");
    }
    for (ord_type, tp, sl) in [
        (OkxAlgoType::Conditional, None, Some("59000")),
        (OkxAlgoType::Oco, Some("61000"), Some("59000")),
    ] {
        let p = OkxProtective {
            rules: pr,
            target: OkxTarget::Spot { side: Side::Sell },
            ord_type,
            size: Size::Qty(d("0.1")),
            tp_trigger: tp.map(d),
            sl_trigger: sl.map(d),
            trigger_type: TriggerPxType::Last,
            ref_price: d("60000"),
            client_id: &cid,
        };
        assert_eq!(
            okx::place_algo(&p).unwrap().path(),
            "/api/v5/trade/order-algo"
        );
        assert_eq!(
            okx::cancel_algos(pr, ord_type, std::slice::from_ref(&by_client))
                .unwrap()
                .path(),
            "/api/v5/trade/cancel-algos"
        );
    }

    // trailing stop (move_order_stop): refused for place, cancel and query alike
    let p = OkxProtective {
        rules: sr,
        target: OkxTarget::Swap {
            mode: Some(PositionMode::OneWay),
            margin: MarginMode::Cross,
            action: Action::Close(Dir::Long),
        },
        ord_type: OkxAlgoType::MoveOrderStop,
        size: Size::Qty(d("2")),
        tp_trigger: None,
        sl_trigger: Some(d("59000")),
        trigger_type: TriggerPxType::Last,
        ref_price: d("60000"),
        client_id: &cid,
    };
    assert!(okx::place_algo(&p).is_err());
    assert!(
        okx::cancel_algos(
            sr,
            OkxAlgoType::MoveOrderStop,
            std::slice::from_ref(&by_client)
        )
        .is_err()
    );
    assert!(okx::query_algo(OkxAlgoType::MoveOrderStop, &by_client).is_err());
}
