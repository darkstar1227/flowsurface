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
use super::client::OpenKind;
use super::client_id::{ClientId, Role};
use super::decimal::Decimal;
use super::gate::OrderIntent;
use super::okx::{self, OkxAlgoType, OkxEntry, OkxProtective, OkxTarget, TriggerPxType};
use super::rules::{FreshRules, TradeProduct};
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
pub struct ManualContext {
    /// The position mode the exchange reported (never the saved one); `None` refuses.
    pub mode: Option<PositionMode>,
    /// Last or mark price: needed for a market order's value, and to check a trigger against
    /// the market. `None` refuses the orders that need it, nothing is guessed.
    pub ref_price: Option<Decimal>,
    /// OKX swap margin mode of the order form.
    pub okx_margin: MarginMode,
}

/// Build the intent for `spec` with the builder of the product `fresh` belongs to. A fresh
/// `ClientId` is made for every call, so a rebuilt order has a new id (and needs a new
/// confirmation: the preview hash includes the id).
pub fn build_intent(
    account: AccountKey,
    fresh: &FreshRules<'_>,
    spec: &ManualSpec,
    ctx: ManualContext,
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
                    attach: None,
                })?,
                ManualSpec::Market { dir, qty } => okx::place_order(OkxEntry {
                    rules,
                    target: okx_entry_target(swap, *dir, ctx)?,
                    qty: *qty,
                    exec: Exec::Market,
                    ref_price: ctx.ref_price,
                    client_id: ClientId::generate(Role::Entry),
                    attach: None,
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
    ctx: ManualContext,
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

fn okx_entry_target(swap: bool, dir: Dir, ctx: ManualContext) -> Result<OkxTarget, TradeError> {
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
    ctx: ManualContext,
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
    use super::super::request::Exposure;
    use super::super::rules::fixtures::*;
    use super::*;

    const NOW: i64 = 1_000;
    const AGE: i64 = 60_000;

    fn acct() -> AccountKey {
        AccountKey::from_bytes([7; 16])
    }

    fn ctx(mode: Option<PositionMode>, ref_price: Option<&str>) -> ManualContext {
        ManualContext {
            mode,
            ref_price: ref_price.map(d),
            okx_margin: MarginMode::Cross,
        }
    }

    fn intent(
        table: &super::super::rules::RulesTable,
        sym: &str,
        spec: &ManualSpec,
        c: ManualContext,
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
