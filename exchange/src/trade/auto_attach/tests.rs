use super::*;
use crate::trade::protect::{TargetSpec, TrailingSpec};
use crate::trade::rules::fixtures::*;

fn spec() -> ProtectSpec {
    ProtectSpec {
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
    }
}

fn input<'a>(
    rules: &'a SymbolRules,
    spec: &'a ProtectSpec,
    mode: PositionMode,
    stage: Stage,
    filled: &str,
) -> StageInput<'a> {
    StageInput {
        rules,
        mode,
        dir: Dir::Long,
        avg_price: d("60000"),
        filled: d(filled),
        ref_price: d("60000"),
        spec,
        stage,
    }
}

fn ids(roles: &[Role]) -> Vec<(Role, ClientId)> {
    roles.iter().map(|r| (*r, ClientId::generate(*r))).collect()
}

fn param(r: &BinanceRequest, name: &str) -> Option<String> {
    r.param(name).map(str::to_string)
}

fn built(out: Vec<(Role, Result<BinanceRequest, TradeError>)>) -> Vec<BinanceRequest> {
    out.into_iter().map(|(_, r)| r.unwrap()).collect()
}

#[test]
fn the_stop_stage_sends_one_whole_position_stop_from_the_first_fill() {
    let table = futures();
    let rules = table.lookup("BTCUSDT").unwrap();
    let spec = spec();
    let inp = input(rules, &spec, PositionMode::OneWay, Stage::Stop, "0.001");
    assert_eq!(stage_roles(&spec, Stage::Stop), vec![Role::StopLoss]);
    let reqs = built(build_stage(&inp, ids(&[Role::StopLoss])));
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(param(r, "type").as_deref(), Some("STOP_MARKET"));
    assert_eq!(param(r, "side").as_deref(), Some("SELL"));
    assert_eq!(param(r, "closePosition").as_deref(), Some("true"));
    assert_eq!(param(r, "quantity"), None);
    assert_eq!(param(r, "triggerPrice").as_deref(), Some("59400"));
    assert_eq!(param(r, "workingType").as_deref(), Some("MARK_PRICE"));
    assert!(matches!(r.exposure(), crate::trade::Exposure::Reduces));
}

#[test]
fn the_rest_stage_sizes_the_targets_from_the_total_and_the_last_takes_the_remainder() {
    let table = futures();
    let rules = table.lookup("BTCUSDT").unwrap();
    let spec = spec();
    let inp = input(rules, &spec, PositionMode::OneWay, Stage::Rest, "0.003");
    assert_eq!(
        stage_roles(&spec, Stage::Rest),
        vec![Role::TakeProfit, Role::TakeProfit]
    );
    let reqs = built(build_stage(
        &inp,
        ids(&[Role::TakeProfit, Role::TakeProfit]),
    ));
    let q: Vec<_> = reqs.iter().map(|r| param(r, "quantity").unwrap()).collect();
    assert_eq!(q, vec!["0.001", "0.002"]);
    let p: Vec<_> = reqs
        .iter()
        .map(|r| param(r, "triggerPrice").unwrap())
        .collect();
    assert_eq!(p, vec!["61200", "62400"]);
    for r in &reqs {
        assert_eq!(param(r, "type").as_deref(), Some("TAKE_PROFIT_MARKET"));
        assert_eq!(param(r, "reduceOnly").as_deref(), Some("true"));
        assert_eq!(param(r, "closePosition"), None);
    }
}

#[test]
fn a_partial_fill_sequence_is_a_stop_first_and_the_rest_for_the_total() {
    let table = futures();
    let rules = table.lookup("BTCUSDT").unwrap();
    let spec = spec();
    // First fill 0.001 at 60000: the stop only.
    let first = input(rules, &spec, PositionMode::OneWay, Stage::Stop, "0.001");
    let stop = built(build_stage(&first, ids(&[Role::StopLoss])));
    assert_eq!(param(&stop[0], "closePosition").as_deref(), Some("true"));
    // The entry finished at 0.002, average 60010: targets for 0.002 from that price.
    let mut done = input(rules, &spec, PositionMode::OneWay, Stage::Rest, "0.002");
    done.avg_price = d("60010");
    done.ref_price = d("60010");
    let rest = built(build_stage(
        &done,
        ids(&[Role::TakeProfit, Role::TakeProfit]),
    ));
    let q: Vec<_> = rest.iter().map(|r| param(r, "quantity").unwrap()).collect();
    assert_eq!(q, vec!["0.001", "0.001"]);
}

#[test]
fn a_hedge_account_gets_whole_position_market_orders_and_no_trailing() {
    let table = futures();
    let rules = table.lookup("BTCUSDT").unwrap();
    let mut spec = spec();
    spec.targets.truncate(1);
    spec.targets[0].share_pct = 100;
    let inp = input(rules, &spec, PositionMode::Hedge, Stage::Rest, "0.002");
    let reqs = built(build_stage(&inp, ids(&[Role::TakeProfit])));
    let r = &reqs[0];
    assert_eq!(param(r, "closePosition").as_deref(), Some("true"));
    assert_eq!(param(r, "positionSide").as_deref(), Some("LONG"));
    assert_eq!(param(r, "reduceOnly"), None);
    assert_eq!(param(r, "quantity"), None);
    spec.trailing = Some(TrailingSpec {
        callback_pct: d("1"),
        activation: None,
    });
    let inp = input(rules, &spec, PositionMode::Hedge, Stage::Rest, "0.002");
    let out = build_stage(&inp, ids(&[Role::TakeProfit, Role::Trailing]));
    assert!(out[0].1.is_ok());
    assert!(out[1].1.is_err(), "no trailing stop on a hedge account");
}

#[test]
fn a_trailing_stop_takes_the_total_and_an_activation_price() {
    let table = futures();
    let rules = table.lookup("BTCUSDT").unwrap();
    let spec = ProtectSpec {
        trailing: Some(TrailingSpec {
            callback_pct: d("1.5"),
            activation: Some(Offset::Percent(d("1"))),
        }),
        ..Default::default()
    };
    let inp = input(rules, &spec, PositionMode::OneWay, Stage::Rest, "0.002");
    assert_eq!(stage_roles(&spec, Stage::Rest), vec![Role::Trailing]);
    let reqs = built(build_stage(&inp, ids(&[Role::Trailing])));
    let r = &reqs[0];
    assert_eq!(param(r, "type").as_deref(), Some("TRAILING_STOP_MARKET"));
    assert_eq!(param(r, "quantity").as_deref(), Some("0.002"));
    assert_eq!(param(r, "callbackRate").as_deref(), Some("1.5"));
    assert_eq!(param(r, "activatePrice").as_deref(), Some("60600"));
}

#[test]
fn nothing_is_half_sent_when_the_prices_cannot_be_worked_out() {
    let table = futures();
    let rules = table.lookup("BTCUSDT").unwrap();
    let spec = spec();
    // Ids that are not the stage's roles.
    let inp = input(rules, &spec, PositionMode::OneWay, Stage::Rest, "0.003");
    let out = build_stage(&inp, ids(&[Role::StopLoss]));
    assert!(out.iter().all(|(_, r)| r.is_err()));
    // A quantity the targets cannot be split from (below one step).
    let tiny = input(rules, &spec, PositionMode::OneWay, Stage::Rest, "0.0004");
    let out = build_stage(&tiny, ids(&[Role::TakeProfit, Role::TakeProfit]));
    assert!(out.iter().all(|(_, r)| r.is_err()));
}

#[test]
fn a_market_already_past_the_trigger_is_refused_before_anything_is_sent() {
    let table = futures();
    let rules = table.lookup("BTCUSDT").unwrap();
    let spec = spec();
    let mut inp = input(rules, &spec, PositionMode::OneWay, Stage::Stop, "0.001");
    // The price fell to 59000 after the fill: a sell stop at 59400 would trigger at once.
    inp.ref_price = d("59000");
    let out = build_stage(&inp, ids(&[Role::StopLoss]));
    assert!(out[0].1.is_err());
}

#[test]
fn nothing_the_automatic_path_builds_adds_exposure() {
    let table = futures();
    let rules = table.lookup("BTCUSDT").unwrap();
    let mut spec = spec();
    spec.trailing = Some(TrailingSpec {
        callback_pct: d("1"),
        activation: None,
    });
    for mode in [PositionMode::OneWay, PositionMode::Hedge] {
        for stage in [Stage::Stop, Stage::Rest] {
            let roles = stage_roles(&spec, stage);
            let inp = input(rules, &spec, mode, stage, "0.003");
            for (_, r) in build_stage(&inp, ids(&roles)) {
                // A refused leg (no trailing stop on a hedge account) builds nothing.
                if let Ok(r) = r {
                    assert!(
                        !matches!(r.exposure(), crate::trade::Exposure::Adds { .. }),
                        "{mode:?} {stage:?}"
                    );
                }
            }
        }
    }
}
