//! What the order ticket asks for, turned into an [`OrderIntent`] by the venue builders (M5
//! c2b-2). Pure: nothing is sent, and every value that reaches the exchange comes out of a
//! builder (rounded there), never from the caller. The GUI keeps the form; this keeps the
//! rules about which builder a form maps to, so they can be tested with the rules fixtures.
//!
//! Scope of the manual ticket: limit and market entries (opening only), stop-loss and
//! take-profit protective orders, a trailing stop on Binance USDⓈ-M only (OKX trailing stays
//! off), and the cancel of one open order. Binance spot has no verified private stream, so it
//! has no manual path at all. Closing and bulk actions are not here.

use super::binance_futures::{self, FuturesEntry};
use super::checks::check_qty;
use super::client::OpenKind;
use super::client_id::{ClientId, Role};
use super::decimal::Decimal;
use super::gate::OrderIntent;
use super::okx::{self, OkxAlgoType, OkxAttach, OkxEntry, OkxProtective, OkxTarget, TriggerPxType};
use super::protect::{ProtectPlan, ProtectSpec, Rounding, TriggerBasis, plan};
use super::rules::{FreshRules, SymbolRules, TradeProduct};
use super::types::{
    Action, Dir, Exec, MarginMode, OrderRef, Protective, ProtectiveKind, Side, Size, TimeInForce,
    TradeError, Trailing, WorkingType,
};
use crate::account::{AccountKey, PositionMode};

/// One manual request, as the user typed it (numbers parsed, nothing rounded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualSpec {
    /// Opens or adds: `qty` is base units (Binance) or contracts (OKX swap).
    Limit {
        dir: Dir,
        qty: Decimal,
        price: Decimal,
    },
    Market {
        dir: Dir,
        qty: Decimal,
    },
    /// `dir` is the direction of the position the order protects.
    Stop {
        dir: Dir,
        size: Size,
        trigger: Decimal,
    },
    TakeProfit {
        dir: Dir,
        size: Size,
        trigger: Decimal,
    },
    Trailing {
        dir: Dir,
        qty: Decimal,
        callback_percent: Decimal,
    },
    Cancel {
        id: String,
        kind: OpenKind,
    },
}

impl ManualSpec {
    /// A new entry, which needs a synchronised stream (condition 51). Protective orders and
    /// cancels only reduce.
    pub fn is_entry(&self) -> bool {
        matches!(self, ManualSpec::Limit { .. } | ManualSpec::Market { .. })
    }
}

/// What the builders need besides the spec.
#[derive(Debug, Clone, Copy)]
pub struct ManualContext<'a> {
    /// The position mode the exchange reported (never the saved one); `None` refuses.
    pub mode: Option<PositionMode>,
    /// Last or mark price: needed for a market order's value, and to check a trigger against
    /// the market. `None` refuses the orders that need it, nothing is guessed.
    pub ref_price: Option<Decimal>,
    /// OKX swap margin mode of the order form.
    pub okx_margin: MarginMode,
    /// The checked protection to attach to an entry (`ProtectDefaults::spec`). Only OKX uses
    /// it here, as `attachAlgoOrds` on the entry itself; a Binance futures entry is sent
    /// unchanged and its protection follows the fill. An empty spec attaches nothing.
    pub protect: Option<&'a ProtectSpec>,
}

/// The protective prices for an entry at `base` (the limit price, or the reference price of a
/// market order), as the entry would get them if it filled there. Pure. The GUI runs it before
/// the confirmation to refuse settings that could not be applied (a stop distance at or above
/// the price, a share too small for the step), and OKX takes its attached triggers from it.
pub fn entry_protection(
    rules: &SymbolRules,
    dir: Dir,
    base: &Decimal,
    qty: &Decimal,
    spec: &ProtectSpec,
) -> Result<ProtectPlan, TradeError> {
    let planned = plan(
        dir,
        *base,
        *qty,
        spec,
        &Rounding {
            tick: *rules.tick(),
            step: rules.lot().step,
        },
    )
    .map_err(|_| TradeError::ProtectionNotPossible)?;
    // A Binance futures protective order sized by quantity is checked against the market lot
    // rule when it is built (`new_algo_order`); check each one now, so a leg the exchange
    // would refuse after the fill is refused before the entry. (Minimum notional does not
    // apply to reduce-only protective orders.) OKX attaches to the whole entry: no own size.
    if rules.product() == TradeProduct::BinanceUsdM {
        let sized = planned
            .targets
            .iter()
            .map(|t| &t.qty)
            .chain(spec.trailing.is_some().then_some(&planned.total_qty));
        for q in sized {
            check_qty(rules, q, true).map_err(|_| TradeError::ProtectionNotPossible)?;
        }
    }
    Ok(planned)
}

/// The OKX attachment of an entry: one group (single TP and/or SL), from the checked spec.
/// `None` when the spec holds nothing for OKX.
fn okx_attach(
    rules: &SymbolRules,
    dir: Dir,
    base: Option<&Decimal>,
    qty: &Decimal,
    spec: Option<&ProtectSpec>,
) -> Result<Option<OkxAttach>, TradeError> {
    let Some(spec) = spec.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    // OKX attaches one TP and one SL to the entry itself; trailing is off (decision 4) and
    // more than one target does not fit. Refused here too, whatever the caller checked.
    if spec.trailing.is_some() || spec.targets.len() > 1 {
        return Err(TradeError::ProtectionNotPossible);
    }
    let base = base.ok_or(TradeError::MissingReferencePrice)?;
    let planned = entry_protection(rules, dir, base, qty, spec)?;
    Ok(Some(OkxAttach {
        tp_trigger: planned.targets.first().map(|t| t.trigger),
        sl_trigger: planned.stop,
        trigger_type: match spec.trigger_price {
            TriggerBasis::Last => TriggerPxType::Last,
            TriggerBasis::Mark => TriggerPxType::Mark,
        },
        client_id: ClientId::generate(if planned.stop.is_some() {
            Role::StopLoss
        } else {
            Role::TakeProfit
        }),
    }))
}

/// Build the intent for `spec` with the builder of the product `fresh` belongs to. A fresh
/// `ClientId` is made for every call, so a rebuilt order has a new id (and needs a new
/// confirmation: the preview hash includes the id).
pub fn build_intent(
    account: AccountKey,
    fresh: &FreshRules<'_>,
    spec: &ManualSpec,
    ctx: ManualContext<'_>,
) -> Result<OrderIntent, TradeError> {
    let rules = fresh.rules();
    match rules.product() {
        TradeProduct::BinanceUsdM => {
            let request = match spec {
                ManualSpec::Limit { dir, qty, price } => {
                    binance_futures::new_order(FuturesEntry {
                        rules,
                        mode: ctx.mode,
                        action: Action::Open(*dir),
                        qty: *qty,
                        exec: Exec::Limit {
                            price: *price,
                            tif: TimeInForce::Gtc,
                        },
                        ref_price: ctx.ref_price,
                        client_id: ClientId::generate(Role::Entry),
                    })?
                }
                ManualSpec::Market { dir, qty } => binance_futures::new_order(FuturesEntry {
                    rules,
                    mode: ctx.mode,
                    action: Action::Open(*dir),
                    qty: *qty,
                    exec: Exec::Market,
                    ref_price: ctx.ref_price,
                    client_id: ClientId::generate(Role::Entry),
                })?,
                ManualSpec::Stop { dir, size, trigger } => binance_futures::new_algo_order(
                    rules,
                    ctx.mode,
                    protective(
                        ProtectiveKind::StopMarket,
                        *dir,
                        *size,
                        Some(*trigger),
                        None,
                        ctx,
                    )?,
                )?,
                ManualSpec::TakeProfit { dir, size, trigger } => binance_futures::new_algo_order(
                    rules,
                    ctx.mode,
                    protective(
                        ProtectiveKind::TakeProfitMarket,
                        *dir,
                        *size,
                        Some(*trigger),
                        None,
                        ctx,
                    )?,
                )?,
                ManualSpec::Trailing {
                    dir,
                    qty,
                    callback_percent,
                } => binance_futures::new_algo_order(
                    rules,
                    ctx.mode,
                    protective(
                        ProtectiveKind::Trailing,
                        *dir,
                        Size::Qty(*qty),
                        None,
                        Some(Trailing::CallbackRate {
                            percent: *callback_percent,
                            activate_price: None,
                        }),
                        ctx,
                    )?,
                )?,
                ManualSpec::Cancel { id, kind } => {
                    let order = OrderRef::exchange(id)?;
                    match kind {
                        OpenKind::Plain => binance_futures::cancel_order(rules, &order)?,
                        OpenKind::Algo(_) => binance_futures::cancel_algo_order(&order)?,
                        OpenKind::List => return Err(TradeError::Unsupported),
                    }
                }
            };
            OrderIntent::binance(account, request, fresh)
        }
        TradeProduct::OkxSwap | TradeProduct::OkxSpot => {
            let swap = rules.product() == TradeProduct::OkxSwap;
            let request = match spec {
                ManualSpec::Limit { dir, qty, price } => okx::place_order(OkxEntry {
                    rules,
                    target: okx_entry_target(swap, *dir, ctx)?,
                    qty: *qty,
                    exec: Exec::Limit {
                        price: *price,
                        tif: TimeInForce::Gtc,
                    },
                    ref_price: ctx.ref_price,
                    client_id: ClientId::generate(Role::Entry),
                    attach: okx_attach(rules, *dir, Some(price), qty, ctx.protect)?,
                })?,
                ManualSpec::Market { dir, qty } => okx::place_order(OkxEntry {
                    rules,
                    target: okx_entry_target(swap, *dir, ctx)?,
                    qty: *qty,
                    exec: Exec::Market,
                    ref_price: ctx.ref_price,
                    client_id: ClientId::generate(Role::Entry),
                    attach: okx_attach(rules, *dir, ctx.ref_price.as_ref(), qty, ctx.protect)?,
                })?,
                ManualSpec::Stop { dir, size, trigger } if swap => {
                    okx_protective(rules, *dir, *size, Some(*trigger), None, ctx)?
                }
                ManualSpec::TakeProfit { dir, size, trigger } if swap => {
                    okx_protective(rules, *dir, *size, None, Some(*trigger), ctx)?
                }
                // OKX trailing stays off (user decision 4); OKX spot has no manual protective
                // order here.
                ManualSpec::Stop { .. }
                | ManualSpec::TakeProfit { .. }
                | ManualSpec::Trailing { .. } => return Err(TradeError::Unsupported),
                ManualSpec::Cancel { id, kind } => {
                    let order = OrderRef::exchange(id)?;
                    match kind {
                        OpenKind::Plain => okx::cancel_order(rules, &order)?,
                        OpenKind::Algo(Some(t)) => {
                            okx::cancel_algos(rules, *t, std::slice::from_ref(&order))?
                        }
                        OpenKind::Algo(None) | OpenKind::List => {
                            return Err(TradeError::Unsupported);
                        }
                    }
                }
            };
            OrderIntent::okx(account, request, fresh)
        }
        TradeProduct::BinanceSpot => Err(TradeError::Unsupported),
    }
}

fn protective(
    kind: ProtectiveKind,
    dir: Dir,
    size: Size,
    trigger: Option<Decimal>,
    trailing: Option<Trailing>,
    ctx: ManualContext<'_>,
) -> Result<Protective, TradeError> {
    Ok(Protective {
        kind,
        dir,
        size,
        trigger_price: trigger,
        limit_price: None,
        tif: TimeInForce::Gtc,
        working_type: WorkingType::MarkPrice,
        trailing,
        ref_price: ctx.ref_price.ok_or(TradeError::MissingReferencePrice)?,
        client_id: ClientId::generate(match kind {
            ProtectiveKind::StopMarket | ProtectiveKind::StopLimit => Role::StopLoss,
            ProtectiveKind::TakeProfitMarket | ProtectiveKind::TakeProfitLimit => Role::TakeProfit,
            ProtectiveKind::Trailing => Role::Trailing,
        }),
    })
}

fn okx_entry_target(swap: bool, dir: Dir, ctx: ManualContext<'_>) -> Result<OkxTarget, TradeError> {
    if swap {
        Ok(OkxTarget::Swap {
            mode: ctx.mode,
            margin: ctx.okx_margin,
            action: Action::Open(dir),
        })
    } else {
        // A spot entry is a buy; selling what is held is not an entry.
        match dir {
            Dir::Long => Ok(OkxTarget::Spot { side: Side::Buy }),
            Dir::Short => Err(TradeError::Unsupported),
        }
    }
}

fn okx_protective(
    rules: &super::rules::SymbolRules,
    dir: Dir,
    size: Size,
    sl: Option<Decimal>,
    tp: Option<Decimal>,
    ctx: ManualContext<'_>,
) -> Result<super::request::OkxRequest, TradeError> {
    okx::place_algo(OkxProtective {
        rules,
        target: OkxTarget::Swap {
            mode: ctx.mode,
            margin: ctx.okx_margin,
            action: Action::Close(dir),
        },
        ord_type: OkxAlgoType::Conditional,
        size,
        tp_trigger: tp,
        sl_trigger: sl,
        trigger_type: TriggerPxType::Last,
        ref_price: ctx.ref_price.ok_or(TradeError::MissingReferencePrice)?,
        client_id: ClientId::generate(if sl.is_some() {
            Role::StopLoss
        } else {
            Role::TakeProfit
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::super::protect::{Offset, TargetSpec, TrailingSpec};
    use super::super::request::Exposure;
    use super::super::rules::fixtures::*;
    use super::*;

    const NOW: i64 = 1_000;
    const AGE: i64 = 60_000;

    fn acct() -> AccountKey {
        AccountKey::from_bytes([7; 16])
    }

    fn ctx(mode: Option<PositionMode>, ref_price: Option<&str>) -> ManualContext<'static> {
        ManualContext {
            mode,
            ref_price: ref_price.map(d),
            okx_margin: MarginMode::Cross,
            protect: None,
        }
    }

    fn intent(
        table: &super::super::rules::RulesTable,
        sym: &str,
        spec: &ManualSpec,
        c: ManualContext<'_>,
    ) -> Result<OrderIntent, TradeError> {
        let fresh = table.fresh_lookup(sym, NOW, AGE).unwrap();
        build_intent(acct(), &fresh, spec, c)
    }

    #[test]
    fn a_binance_limit_entry_previews_the_rounded_values_and_adds_exposure() {
        let t = futures();
        let spec = ManualSpec::Limit {
            dir: Dir::Long,
            qty: d("0.0039"),
            price: d("50000.04"),
        };
        let i = intent(&t, "BTCUSDT", &spec, ctx(Some(PositionMode::OneWay), None)).unwrap();
        let p = i.preview();
        assert_eq!(p.field("side"), Some("BUY"));
        assert_eq!(p.field("type"), Some("LIMIT"));
        // The preview is the builder's output: the price is on the tick, not what was typed.
        assert_eq!(
            p.field("quantity"),
            Some("0.003"),
            "typed 0.0039, rounded down"
        );
        assert_eq!(
            p.field("price"),
            Some("50000"),
            "typed 50000.04, rounded to the tick"
        );
        assert!(matches!(i.exposure(), Exposure::Adds { notional: Some(_) }));
    }

    #[test]
    fn a_market_entry_without_a_reference_price_is_refused_not_guessed() {
        let t = futures();
        let spec = ManualSpec::Market {
            dir: Dir::Long,
            qty: d("0.01"),
        };
        let r = intent(&t, "BTCUSDT", &spec, ctx(Some(PositionMode::OneWay), None));
        assert!(r.is_err(), "no price, no market order");
        let ok = intent(
            &t,
            "BTCUSDT",
            &spec,
            ctx(Some(PositionMode::OneWay), Some("50000")),
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn an_unknown_position_mode_refuses_every_order_that_needs_it() {
        let t = futures();
        let spec = ManualSpec::Limit {
            dir: Dir::Short,
            qty: d("0.01"),
            price: d("50000"),
        };
        assert_eq!(
            intent(&t, "BTCUSDT", &spec, ctx(None, Some("50000"))).unwrap_err(),
            TradeError::PositionModeUnknown
        );
    }

    #[test]
    fn a_protective_order_needs_a_reference_price_and_a_hedge_account_only_closes_all() {
        let t = futures();
        let stop = ManualSpec::Stop {
            dir: Dir::Long,
            size: Size::Qty(d("0.01")),
            trigger: d("48000"),
        };
        assert_eq!(
            intent(&t, "BTCUSDT", &stop, ctx(Some(PositionMode::OneWay), None)).unwrap_err(),
            TradeError::MissingReferencePrice
        );
        let c = ctx(Some(PositionMode::Hedge), Some("50000"));
        assert_eq!(
            intent(&t, "BTCUSDT", &stop, c).unwrap_err(),
            TradeError::HedgeProtectiveMustClosePosition
        );
        let close_all = ManualSpec::Stop {
            dir: Dir::Long,
            size: Size::ClosePosition,
            trigger: d("48000"),
        };
        let i = intent(&t, "BTCUSDT", &close_all, c).unwrap();
        assert_eq!(i.preview().field("closePosition"), Some("true"));
        assert_eq!(i.exposure(), Exposure::Reduces);
    }

    #[test]
    fn a_trailing_stop_is_binance_futures_only() {
        let spec = ManualSpec::Trailing {
            dir: Dir::Long,
            qty: d("1"),
            callback_percent: d("1"),
        };
        let c = ctx(Some(PositionMode::OneWay), Some("50000"));
        let fut = intent(&futures(), "BTCUSDT", &spec, c).unwrap();
        assert_eq!(fut.preview().field("type"), Some("TRAILING_STOP_MARKET"));
        assert_eq!(
            intent(&okx_swap_table(), "BTC-USDT-SWAP", &spec, c).unwrap_err(),
            TradeError::Unsupported
        );
    }

    #[test]
    fn binance_spot_has_no_manual_path() {
        let spec = ManualSpec::Limit {
            dir: Dir::Long,
            qty: d("0.01"),
            price: d("50000"),
        };
        let c = ctx(None, Some("50000"));
        assert_eq!(
            intent(&spot(), "BTCUSDT", &spec, c).unwrap_err(),
            TradeError::Unsupported
        );
    }

    #[test]
    fn an_okx_swap_entry_and_a_spot_buy_build_but_a_spot_short_does_not() {
        let c = ctx(Some(PositionMode::OneWay), Some("50000"));
        let long = ManualSpec::Limit {
            dir: Dir::Long,
            qty: d("1"),
            price: d("50000"),
        };
        let i = intent(&okx_swap_table(), "BTC-USDT-SWAP", &long, c).unwrap();
        assert_eq!(i.preview().field("side"), Some("buy"));
        assert_eq!(i.preview().field("tdMode"), Some("cross"));
        assert!(intent(&okx_spot_table(), "BTC-USDT", &long, c).is_ok());
        let short = ManualSpec::Limit {
            dir: Dir::Short,
            qty: d("1"),
            price: d("50000"),
        };
        assert_eq!(
            intent(&okx_spot_table(), "BTC-USDT", &short, c).unwrap_err(),
            TradeError::Unsupported
        );
    }

    #[test]
    fn a_cancel_picks_the_endpoint_of_the_order_kind_and_only_cancels() {
        let c = ctx(None, None);
        let plain = ManualSpec::Cancel {
            id: "123".into(),
            kind: OpenKind::Plain,
        };
        let algo = ManualSpec::Cancel {
            id: "123".into(),
            kind: OpenKind::Algo(None),
        };
        let p = intent(&futures(), "BTCUSDT", &plain, c).unwrap();
        assert_eq!(p.preview().path, "/fapi/v1/order");
        assert_eq!(p.exposure(), Exposure::Neutral);
        let a = intent(&futures(), "BTCUSDT", &algo, c).unwrap();
        assert_eq!(a.preview().path, "/fapi/v1/algoOrder");
        // OKX: an algo order of an unknown type is not cancelled blind.
        assert_eq!(
            intent(&okx_swap_table(), "BTC-USDT-SWAP", &algo, c).unwrap_err(),
            TradeError::Unsupported
        );
        let bad = ManualSpec::Cancel {
            id: "12;3".into(),
            kind: OpenKind::Plain,
        };
        assert_eq!(
            intent(&futures(), "BTCUSDT", &bad, c).unwrap_err(),
            TradeError::OrderIdInvalid
        );
    }

    fn spec_of(stop: Option<Offset>, targets: &[(Offset, u8)]) -> ProtectSpec {
        ProtectSpec {
            stop,
            targets: targets
                .iter()
                .map(|&(offset, share_pct)| TargetSpec { offset, share_pct })
                .collect(),
            ..ProtectSpec::default()
        }
    }

    fn pct(s: &str) -> Offset {
        Offset::Percent(d(s))
    }

    fn with_protect<'a>(c: ManualContext<'a>, p: &'a ProtectSpec) -> ManualContext<'a> {
        ManualContext {
            protect: Some(p),
            ..c
        }
    }

    #[test]
    fn an_okx_entry_carries_the_attached_triggers_in_its_preview() {
        let c = ctx(Some(PositionMode::OneWay), Some("50000"));
        let p = spec_of(Some(pct("2")), &[(pct("4"), 100)]);
        let market = ManualSpec::Market {
            dir: Dir::Long,
            qty: d("1"),
        };
        let i = intent(
            &okx_swap_table(),
            "BTC-USDT-SWAP",
            &market,
            with_protect(c, &p),
        )
        .unwrap();
        let v = i.preview();
        assert_eq!(v.field("attachAlgoOrds[0].slTriggerPx"), Some("49000"));
        assert_eq!(v.field("attachAlgoOrds[0].tpTriggerPx"), Some("52000"));
        assert_eq!(v.field("attachAlgoOrds[0].slTriggerPxType"), Some("last"));
        // A short mirrors it.
        let short = ManualSpec::Market {
            dir: Dir::Short,
            qty: d("1"),
        };
        let v = intent(
            &okx_swap_table(),
            "BTC-USDT-SWAP",
            &short,
            with_protect(c, &p),
        )
        .unwrap()
        .preview();
        assert_eq!(v.field("attachAlgoOrds[0].slTriggerPx"), Some("51000"));
        assert_eq!(v.field("attachAlgoOrds[0].tpTriggerPx"), Some("48000"));
        // A limit entry measures from its own price.
        let limit = ManualSpec::Limit {
            dir: Dir::Long,
            qty: d("1"),
            price: d("49000"),
        };
        let only_stop = spec_of(Some(Offset::Distance(d("1000"))), &[]);
        let v = intent(
            &okx_swap_table(),
            "BTC-USDT-SWAP",
            &limit,
            with_protect(c, &only_stop),
        )
        .unwrap()
        .preview();
        assert_eq!(v.field("attachAlgoOrds[0].slTriggerPx"), Some("48000"));
        assert_eq!(v.field("attachAlgoOrds[0].tpTriggerPx"), None);
        // A spot buy may carry it too.
        assert!(
            intent(
                &okx_spot_table(),
                "BTC-USDT",
                &limit,
                with_protect(c, &only_stop)
            )
            .unwrap()
            .preview()
            .field("attachAlgoOrds[0].slTriggerPx")
            .is_some()
        );
    }

    #[test]
    fn no_protection_or_an_empty_spec_attaches_nothing() {
        let c = ctx(Some(PositionMode::OneWay), Some("50000"));
        let market = ManualSpec::Market {
            dir: Dir::Long,
            qty: d("1"),
        };
        let none = intent(&okx_swap_table(), "BTC-USDT-SWAP", &market, c).unwrap();
        let empty = ProtectSpec::default();
        let with_empty = intent(
            &okx_swap_table(),
            "BTC-USDT-SWAP",
            &market,
            with_protect(c, &empty),
        )
        .unwrap();
        for i in [none, with_empty] {
            assert!(
                i.preview()
                    .fields
                    .iter()
                    .all(|f| !f.name.starts_with("attachAlgoOrds")),
                "{:?}",
                i.preview().fields
            );
        }
    }

    #[test]
    fn okx_refuses_what_an_entry_cannot_carry_and_never_sends_it_bare() {
        let c = ctx(Some(PositionMode::OneWay), Some("50000"));
        let market = ManualSpec::Market {
            dir: Dir::Long,
            qty: d("1"),
        };
        let build = |p: &ProtectSpec| {
            intent(
                &okx_swap_table(),
                "BTC-USDT-SWAP",
                &market,
                with_protect(c, p),
            )
        };
        // Two targets, and a trailing stop, do not fit one attached group.
        let two = spec_of(None, &[(pct("1"), 50), (pct("2"), 50)]);
        assert_eq!(build(&two).unwrap_err(), TradeError::ProtectionNotPossible);
        let trailing = ProtectSpec {
            trailing: Some(TrailingSpec {
                callback_pct: d("1"),
                activation: None,
            }),
            ..ProtectSpec::default()
        };
        assert_eq!(
            build(&trailing).unwrap_err(),
            TradeError::ProtectionNotPossible
        );
        // A stop distance at or above the price cannot be applied.
        let wide = spec_of(Some(Offset::Distance(d("50000"))), &[]);
        assert_eq!(build(&wide).unwrap_err(), TradeError::ProtectionNotPossible);
        // A market entry without a reference price cannot work out the triggers.
        let ok = spec_of(Some(pct("1")), &[]);
        let no_ref = ctx(Some(PositionMode::OneWay), None);
        assert!(
            intent(
                &okx_swap_table(),
                "BTC-USDT-SWAP",
                &market,
                with_protect(no_ref, &ok)
            )
            .is_err()
        );
        // A spot short is not an entry at all.
        let short = ManualSpec::Limit {
            dir: Dir::Short,
            qty: d("1"),
            price: d("50000"),
        };
        assert!(intent(&okx_spot_table(), "BTC-USDT", &short, with_protect(c, &ok)).is_err());
    }

    #[test]
    fn a_binance_entry_is_sent_unchanged_whatever_the_protection() {
        let c = ctx(Some(PositionMode::OneWay), Some("50000"));
        let market = ManualSpec::Market {
            dir: Dir::Long,
            qty: d("0.01"),
        };
        let p = spec_of(Some(pct("2")), &[(pct("4"), 100)]);
        let plain = intent(&futures(), "BTCUSDT", &market, c).unwrap().preview();
        let with = intent(&futures(), "BTCUSDT", &market, with_protect(c, &p))
            .unwrap()
            .preview();
        let names = |v: &super::super::preview::OrderPreview| {
            v.fields.iter().map(|f| f.name.clone()).collect::<Vec<_>>()
        };
        assert_eq!(names(&plain), names(&with));
    }

    #[test]
    fn entry_protection_works_out_the_prices_from_the_base() {
        let t = futures();
        let fresh = t.fresh_lookup("BTCUSDT", NOW, AGE).unwrap();
        let p = spec_of(Some(pct("2")), &[(pct("4"), 100)]);
        let plan = entry_protection(fresh.rules(), Dir::Long, &d("50000"), &d("0.01"), &p).unwrap();
        assert_eq!(plan.stop, Some(d("49000")));
        assert_eq!(plan.targets[0].trigger, d("52000"));
        // A leg above the market lot maximum (120) is refused now, as it would be after the
        // fill; the same split under the maximum passes. Only Binance futures sizes legs.
        let split = spec_of(None, &[(pct("1"), 40), (pct("2"), 60)]);
        let at =
            |qty: &str| entry_protection(fresh.rules(), Dir::Long, &d("50000"), &d(qty), &split);
        assert_eq!(at("300").unwrap_err(), TradeError::ProtectionNotPossible);
        assert!(at("200").is_ok());
        let trailing = ProtectSpec {
            trailing: Some(TrailingSpec {
                callback_pct: d("1"),
                activation: None,
            }),
            ..ProtectSpec::default()
        };
        assert!(
            entry_protection(fresh.rules(), Dir::Long, &d("50000"), &d("130"), &trailing).is_err()
        );
        let wide = spec_of(Some(Offset::Distance(d("50000"))), &[]);
        assert_eq!(
            entry_protection(fresh.rules(), Dir::Long, &d("50000"), &d("0.01"), &wide).unwrap_err(),
            TradeError::ProtectionNotPossible
        );
    }

    #[test]
    fn a_rebuilt_order_has_another_client_id_and_so_another_confirmation() {
        let t = futures();
        let spec = ManualSpec::Limit {
            dir: Dir::Long,
            qty: d("0.01"),
            price: d("50000"),
        };
        let c = ctx(Some(PositionMode::OneWay), None);
        let a = intent(&t, "BTCUSDT", &spec, c).unwrap();
        let b = intent(&t, "BTCUSDT", &spec, c).unwrap();
        assert_ne!(a.client_ids(), b.client_ids());
        assert_ne!(
            a.preview().field("newClientOrderId"),
            None,
            "the id is shown, only its tail"
        );
    }
}
