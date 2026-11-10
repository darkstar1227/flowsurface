//! Binance USDⓈ-M futures request builders (pure: parameters only, nothing is sent).
//!
//! Endpoints: `POST /fapi/v1/order` for LIMIT and MARKET, and `POST /fapi/v1/algoOrder`
//! for conditional orders (STOP_MARKET, TAKE_PROFIT_MARKET, STOP, TAKE_PROFIT,
//! TRAILING_STOP_MARKET). Algo orders have their own cancel and query endpoints.
//! Field names are from the official SDK docstrings (explorer-1 report 1.1, 1.2).

use super::checks::{check_order, check_qty};
use super::client_id::{ClientId, IdField};
use super::common::{check_trigger_side, push_ref};
use super::decimal::Decimal;
use super::request::{BinanceRequest, Method};
use super::rules::{SymbolRules, TradeProduct};
use super::types::{
    Action, Exec, OrderRef, Protective, ProtectiveKind, Size, TradeError, Trailing,
};
use crate::account::PositionMode;

const PRODUCT: TradeProduct = TradeProduct::BinanceUsdM;

#[derive(Debug)]
pub struct FuturesEntry<'a> {
    pub rules: &'a SymbolRules,
    /// The position mode the exchange reported. `None` refuses the order (condition 58).
    pub mode: Option<PositionMode>,
    pub action: Action,
    pub qty: Decimal,
    pub exec: Exec,
    /// Last or mark price; needed for the notional check of a market order.
    pub ref_price: Option<Decimal>,
    /// Owned: one id belongs to one request (condition 61).
    pub client_id: ClientId,
}

#[cfg(test)]
impl FuturesEntry<'_> {
    /// Tests build the same order several times; production code cannot (condition 61).
    pub(crate) fn duplicate(&self) -> FuturesEntry<'_> {
        FuturesEntry {
            rules: self.rules,
            mode: self.mode,
            action: self.action,
            qty: self.qty,
            exec: self.exec,
            ref_price: self.ref_price,
            client_id: ClientId::parse(self.client_id.as_str()).unwrap(),
        }
    }
}

fn position_side(dir: super::types::Dir) -> &'static str {
    match dir {
        super::types::Dir::Long => "LONG",
        super::types::Dir::Short => "SHORT",
    }
}

/// `POST /fapi/v1/order`.
pub fn new_order(i: FuturesEntry<'_>) -> Result<BinanceRequest, TradeError> {
    i.rules.require_product(PRODUCT)?;
    let mode = i.mode.ok_or(TradeError::PositionModeUnknown)?;
    let id = i.client_id.check(IdField::BinanceFutures)?;
    let side = i.action.side();
    let market = matches!(i.exec, Exec::Market);

    let qty = i.rules.round_qty(&i.qty, market)?;
    let (limit_price, tif) = match i.exec {
        Exec::Limit { price, tif } => (Some(i.rules.round_limit_price(side, &price)?), Some(tif)),
        Exec::Market => (None, None),
    };
    // A closing order is exempt from the minimum notional on the exchange: only check size,
    // so a small position can always be closed.
    let mut order_value = None;
    if i.action.is_close() {
        check_qty(i.rules, &qty, market)?;
    } else {
        let price = limit_price.as_ref().or(i.ref_price.as_ref());
        order_value = check_order(i.rules, &qty, price, market)?;
    }

    let mut r = BinanceRequest::new(Method::Post, "/fapi/v1/order");
    r.push("symbol", i.rules.symbol().as_str());
    r.push("side", side.binance());
    if mode == PositionMode::Hedge {
        r.push("positionSide", position_side(i.action.dir()));
    }
    r.push("type", if market { "MARKET" } else { "LIMIT" });
    if let Some(tif) = tif {
        r.push("timeInForce", tif.binance());
    }
    r.push("quantity", qty.to_plain_string());
    r.push_opt("price", limit_price.map(|p| p.to_plain_string()));
    // Hedge mode never gets reduceOnly: the exchange refuses it there (condition 64).
    if mode == PositionMode::OneWay && i.action.is_close() {
        r.push("reduceOnly", "true");
    }
    r.push("newClientOrderId", id);
    Ok(if i.action.is_close() {
        r.reducing()
    } else {
        r.adding(order_value)
    })
}

fn algo_type(kind: ProtectiveKind) -> &'static str {
    match kind {
        ProtectiveKind::StopMarket => "STOP_MARKET",
        ProtectiveKind::TakeProfitMarket => "TAKE_PROFIT_MARKET",
        ProtectiveKind::StopLimit => "STOP",
        ProtectiveKind::TakeProfitLimit => "TAKE_PROFIT",
        ProtectiveKind::Trailing => "TRAILING_STOP_MARKET",
    }
}

/// `POST /fapi/v1/algoOrder`: a protective order for an open position.
///
/// Hedge accounts only get full-close market orders (`closePosition=true`, no quantity,
/// no `reduceOnly`); limit and trailing variants are refused there, because
/// `closePosition` is not allowed on them and a quantity order would need `reduceOnly`,
/// which hedge mode forbids.
pub fn new_algo_order(
    rules: &SymbolRules,
    mode: Option<PositionMode>,
    p: Protective,
) -> Result<BinanceRequest, TradeError> {
    rules.require_product(PRODUCT)?;
    let mode = mode.ok_or(TradeError::PositionModeUnknown)?;
    let id = p.client_id.check(IdField::BinanceFutures)?;
    let side = p.dir.closing_side();

    match (mode, p.size) {
        (PositionMode::Hedge, Size::Qty(_)) => {
            return Err(TradeError::HedgeProtectiveMustClosePosition);
        }
        (PositionMode::Hedge, Size::ClosePosition) if !p.kind.is_market() => {
            return Err(TradeError::HedgeProtectiveMustClosePosition);
        }
        (PositionMode::OneWay, Size::ClosePosition) if !p.kind.is_market() => {
            return Err(TradeError::ClosePositionNotAllowed);
        }
        _ => {}
    }

    // Trigger price: required for everything but a trailing stop.
    let trigger = match (p.kind, p.trigger_price) {
        (ProtectiveKind::Trailing, None) => None,
        (ProtectiveKind::Trailing, Some(_)) => return Err(TradeError::PriceInvalid),
        (_, Some(t)) => {
            let t = rules.round_trigger_price(&t)?;
            check_trigger_side(p.kind, p.dir, &t, &p.ref_price)?;
            Some(t)
        }
        (_, None) => return Err(TradeError::PriceInvalid),
    };

    let limit_price = match (p.kind, p.limit_price) {
        (ProtectiveKind::StopLimit | ProtectiveKind::TakeProfitLimit, Some(l)) => {
            Some(rules.round_limit_price(side, &l)?)
        }
        (ProtectiveKind::StopLimit | ProtectiveKind::TakeProfitLimit, None) => {
            return Err(TradeError::PriceInvalid);
        }
        (_, Some(_)) => return Err(TradeError::PriceInvalid),
        (_, None) => None,
    };

    let qty = match p.size {
        Size::Qty(q) => {
            // Triggered market-type orders are sized by MARKET_LOT_SIZE.
            let market = p.kind.is_market() || p.kind == ProtectiveKind::Trailing;
            let q = rules.round_qty(&q, market)?;
            // reduce-only protective orders are exempt from the minimum notional.
            check_qty(rules, &q, market)?;
            Some(q)
        }
        Size::ClosePosition => None,
    };

    let (callback_rate, activate_price) = match (p.kind, p.trailing) {
        (
            ProtectiveKind::Trailing,
            Some(Trailing::CallbackRate {
                percent,
                activate_price,
            }),
        ) => {
            if !percent.is_positive() {
                return Err(TradeError::NonPositive);
            }
            let activate = match activate_price {
                Some(a) => Some(rules.round_trigger_price(&a)?),
                None => None,
            };
            (Some(percent), activate)
        }
        (ProtectiveKind::Trailing, _) => return Err(TradeError::PriceInvalid),
        (_, None) => (None, None),
        (_, Some(_)) => return Err(TradeError::PriceInvalid),
    };

    let mut r = BinanceRequest::new(Method::Post, "/fapi/v1/algoOrder");
    r.push("algoType", "CONDITIONAL");
    r.push("symbol", rules.symbol().as_str());
    r.push("side", side.binance());
    if mode == PositionMode::Hedge {
        r.push("positionSide", position_side(p.dir));
    }
    r.push("type", algo_type(p.kind));
    if limit_price.is_some() {
        r.push("timeInForce", p.tif.binance());
    }
    r.push_opt("quantity", qty.map(|q| q.to_plain_string()));
    r.push_opt("price", limit_price.map(|l| l.to_plain_string()));
    r.push_opt("triggerPrice", trigger.map(|t| t.to_plain_string()));
    r.push("workingType", p.working_type.binance());
    if p.size == Size::ClosePosition {
        r.push("closePosition", "true");
    } else if mode == PositionMode::OneWay {
        r.push("reduceOnly", "true");
    }
    r.push_opt("activatePrice", activate_price.map(|a| a.to_plain_string()));
    r.push_opt("callbackRate", callback_rate.map(|c| c.to_plain_string()));
    r.push("clientAlgoId", id);
    // Protective orders only ever close (condition 64, 71).
    Ok(r.reducing())
}

/// `DELETE /fapi/v1/order`.
pub fn cancel_order(rules: &SymbolRules, order: &OrderRef) -> Result<BinanceRequest, TradeError> {
    order_request(Method::Delete, rules, order)
}

/// `GET /fapi/v1/order`.
pub fn query_order(rules: &SymbolRules, order: &OrderRef) -> Result<BinanceRequest, TradeError> {
    order_request(Method::Get, rules, order)
}

fn order_request(
    method: Method,
    rules: &SymbolRules,
    order: &OrderRef,
) -> Result<BinanceRequest, TradeError> {
    rules.require_product(PRODUCT)?;
    let mut r = BinanceRequest::new(method, "/fapi/v1/order");
    r.push("symbol", rules.symbol().as_str());
    push_ref(
        &mut r,
        order,
        "orderId",
        "origClientOrderId",
        IdField::BinanceFutures,
    )?;
    Ok(r)
}

/// `DELETE /fapi/v1/algoOrder` (`algoId` or `clientAlgoId`; no symbol).
pub fn cancel_algo_order(order: &OrderRef) -> Result<BinanceRequest, TradeError> {
    algo_request(Method::Delete, order)
}

/// `GET /fapi/v1/algoOrder`.
pub fn query_algo_order(order: &OrderRef) -> Result<BinanceRequest, TradeError> {
    algo_request(Method::Get, order)
}

fn algo_request(method: Method, order: &OrderRef) -> Result<BinanceRequest, TradeError> {
    let mut r = BinanceRequest::new(method, "/fapi/v1/algoOrder");
    push_ref(
        &mut r,
        order,
        "algoId",
        "clientAlgoId",
        IdField::BinanceFutures,
    )?;
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::super::client_id::ClientId;
    use super::super::rules::fixtures::*;
    use super::super::types::{Dir, TimeInForce, WorkingType};
    use super::*;

    const SECRET: &str = "test-secret-not-real";
    const TS: i64 = 1_700_000_000_000;

    fn id(s: &str) -> ClientId {
        ClientId::parse(s).unwrap()
    }

    // The builders consume their input (one id, one request). These wrappers let a test build
    // the same input again, with a copy of the id.
    fn new_order(i: &FuturesEntry<'_>) -> Result<BinanceRequest, TradeError> {
        super::new_order(i.duplicate())
    }

    fn new_algo_order(
        rules: &SymbolRules,
        mode: Option<PositionMode>,
        p: &Protective,
    ) -> Result<BinanceRequest, TradeError> {
        super::new_algo_order(rules, mode, p.duplicate())
    }

    fn dup(c: &ClientId) -> ClientId {
        id(c.as_str())
    }

    /// The signed string, which the golden strings are compared to.
    fn signed(r: &BinanceRequest) -> String {
        let s = r.signed(SECRET, 5000, TS).unwrap();
        // what is sent is exactly what was signed, plus the signature
        assert!(s.body.starts_with(&s.query));
        assert_eq!(
            s.body,
            format!(
                "{}&signature={}",
                s.query,
                crate::account::binance_sign(SECRET, &s.query)
            )
        );
        s.query
    }

    fn entry<'a>(
        t: &'a super::super::rules::RulesTable,
        mode: Option<PositionMode>,
        action: Action,
        exec: Exec,
        cid: &'a ClientId,
    ) -> FuturesEntry<'a> {
        FuturesEntry {
            rules: t.lookup("BTCUSDT").unwrap(),
            mode,
            action,
            qty: d("0.0029"),
            exec,
            ref_price: Some(d("60000")),
            client_id: dup(cid),
        }
    }

    fn prot(kind: ProtectiveKind, dir: Dir, size: Size, cid: &ClientId) -> Protective {
        Protective {
            kind,
            dir,
            size,
            trigger_price: Some(d("59000.04")),
            limit_price: None,
            tif: TimeInForce::Gtc,
            working_type: WorkingType::MarkPrice,
            trailing: None,
            ref_price: d("60000"),
            client_id: dup(cid),
        }
    }

    const LIMIT: Exec = Exec::Limit {
        price: Decimal::ZERO,
        tif: TimeInForce::Gtc,
    };

    fn limit(price: &str) -> Exec {
        match LIMIT {
            Exec::Limit { tif, .. } => Exec::Limit {
                price: d(price),
                tif,
            },
            m => m,
        }
    }

    // ── one-way ──────────────────────────────────────────────────────────

    #[test]
    fn one_way_entry_limit_and_market() {
        let t = futures();
        let cid = id("eAAAA");
        let q = signed(
            &new_order(&entry(
                &t,
                Some(PositionMode::OneWay),
                Action::Open(Dir::Long),
                limit("59999.97"),
                &cid,
            ))
            .unwrap(),
        );
        // buy limit rounds down to the tick (59999.9); quantity is 0.0019
        assert_eq!(
            q,
            "symbol=BTCUSDT&side=BUY&type=LIMIT&timeInForce=GTC&quantity=0.002&price=59999.9&newClientOrderId=eAAAA&recvWindow=5000&timestamp=1700000000000"
        );
        let q = signed(
            &new_order(&entry(
                &t,
                Some(PositionMode::OneWay),
                Action::Open(Dir::Short),
                limit("59999.91"),
                &cid,
            ))
            .unwrap(),
        );
        // sell limit rounds up
        assert!(q.contains("side=SELL") && q.contains("price=60000&"));
        let q = signed(
            &new_order(&entry(
                &t,
                Some(PositionMode::OneWay),
                Action::Open(Dir::Long),
                Exec::Market,
                &cid,
            ))
            .unwrap(),
        );
        assert_eq!(
            q,
            "symbol=BTCUSDT&side=BUY&type=MARKET&quantity=0.002&newClientOrderId=eAAAA&recvWindow=5000&timestamp=1700000000000"
        );
    }

    #[test]
    fn one_way_close_is_reduce_only() {
        let t = futures();
        let cid = id("eAAAA");
        let q = signed(
            &new_order(&entry(
                &t,
                Some(PositionMode::OneWay),
                Action::Close(Dir::Long),
                Exec::Market,
                &cid,
            ))
            .unwrap(),
        );
        assert_eq!(
            q,
            "symbol=BTCUSDT&side=SELL&type=MARKET&quantity=0.002&reduceOnly=true&newClientOrderId=eAAAA&recvWindow=5000&timestamp=1700000000000"
        );
    }

    #[test]
    fn one_way_stop_take_profit_and_trailing() {
        let t = futures();
        let r = t.lookup("BTCUSDT").unwrap();
        let cid = id("sBBBB");
        let size = Size::Qty(d("0.0029"));
        let q = signed(
            &new_algo_order(
                r,
                Some(PositionMode::OneWay),
                &prot(ProtectiveKind::StopMarket, Dir::Long, size, &cid),
            )
            .unwrap(),
        );
        assert_eq!(
            q,
            "algoType=CONDITIONAL&symbol=BTCUSDT&side=SELL&type=STOP_MARKET&quantity=0.002&triggerPrice=59000&workingType=MARK_PRICE&reduceOnly=true&clientAlgoId=sBBBB&recvWindow=5000&timestamp=1700000000000"
        );

        let mut tp = prot(ProtectiveKind::TakeProfitMarket, Dir::Long, size, &cid);
        tp.trigger_price = Some(d("61000.06"));
        let q = signed(&new_algo_order(r, Some(PositionMode::OneWay), &tp).unwrap());
        assert_eq!(
            q,
            "algoType=CONDITIONAL&symbol=BTCUSDT&side=SELL&type=TAKE_PROFIT_MARKET&quantity=0.002&triggerPrice=61000.1&workingType=MARK_PRICE&reduceOnly=true&clientAlgoId=sBBBB&recvWindow=5000&timestamp=1700000000000"
        );

        // closePosition: no quantity, no reduceOnly
        let q = signed(
            &new_algo_order(
                r,
                Some(PositionMode::OneWay),
                &prot(
                    ProtectiveKind::StopMarket,
                    Dir::Long,
                    Size::ClosePosition,
                    &cid,
                ),
            )
            .unwrap(),
        );
        assert_eq!(
            q,
            "algoType=CONDITIONAL&symbol=BTCUSDT&side=SELL&type=STOP_MARKET&triggerPrice=59000&workingType=MARK_PRICE&closePosition=true&clientAlgoId=sBBBB&recvWindow=5000&timestamp=1700000000000"
        );

        // stop-limit
        let mut sl = prot(ProtectiveKind::StopLimit, Dir::Long, size, &cid);
        sl.limit_price = Some(d("58990.01"));
        let q = signed(&new_algo_order(r, Some(PositionMode::OneWay), &sl).unwrap());
        assert_eq!(
            q,
            "algoType=CONDITIONAL&symbol=BTCUSDT&side=SELL&type=STOP&timeInForce=GTC&quantity=0.002&price=58990.1&triggerPrice=59000&workingType=MARK_PRICE&reduceOnly=true&clientAlgoId=sBBBB&recvWindow=5000&timestamp=1700000000000"
        );

        // trailing
        let mut tr = prot(ProtectiveKind::Trailing, Dir::Long, size, &cid);
        tr.trigger_price = None;
        tr.trailing = Some(Trailing::CallbackRate {
            percent: d("1.5"),
            activate_price: Some(d("61000")),
        });
        let q = signed(&new_algo_order(r, Some(PositionMode::OneWay), &tr).unwrap());
        assert_eq!(
            q,
            "algoType=CONDITIONAL&symbol=BTCUSDT&side=SELL&type=TRAILING_STOP_MARKET&quantity=0.002&workingType=MARK_PRICE&reduceOnly=true&activatePrice=61000&callbackRate=1.5&clientAlgoId=sBBBB&recvWindow=5000&timestamp=1700000000000"
        );
    }

    #[test]
    fn one_way_cancel_and_query() {
        let t = futures();
        let r = t.lookup("BTCUSDT").unwrap();
        let by_id = OrderRef::exchange("123456").unwrap();
        let by_client = OrderRef::Client(id("eAAAA"));
        assert_eq!(
            signed(&cancel_order(r, &by_id).unwrap()),
            "symbol=BTCUSDT&orderId=123456&recvWindow=5000&timestamp=1700000000000"
        );
        assert_eq!(
            signed(&cancel_order(r, &by_client).unwrap()),
            "symbol=BTCUSDT&origClientOrderId=eAAAA&recvWindow=5000&timestamp=1700000000000"
        );
        assert_eq!(query_order(r, &by_id).unwrap().method(), Method::Get);
        assert_eq!(
            signed(&query_order(r, &by_client).unwrap()),
            "symbol=BTCUSDT&origClientOrderId=eAAAA&recvWindow=5000&timestamp=1700000000000"
        );
        assert_eq!(
            signed(&cancel_algo_order(&by_id).unwrap()),
            "algoId=123456&recvWindow=5000&timestamp=1700000000000"
        );
        assert_eq!(
            signed(&query_algo_order(&by_client).unwrap()),
            "clientAlgoId=eAAAA&recvWindow=5000&timestamp=1700000000000"
        );
        assert_eq!(
            cancel_algo_order(&by_id).unwrap().path(),
            "/fapi/v1/algoOrder"
        );
        assert_eq!(cancel_algo_order(&by_id).unwrap().method(), Method::Delete);
    }

    // ── hedge ────────────────────────────────────────────────────────────

    #[test]
    fn hedge_entry_and_close_never_send_reduce_only() {
        let t = futures();
        let cid = id("eAAAA");
        let h = Some(PositionMode::Hedge);
        let q =
            signed(&new_order(&entry(&t, h, Action::Open(Dir::Long), Exec::Market, &cid)).unwrap());
        assert_eq!(
            q,
            "symbol=BTCUSDT&side=BUY&positionSide=LONG&type=MARKET&quantity=0.002&newClientOrderId=eAAAA&recvWindow=5000&timestamp=1700000000000"
        );
        // close long = SELL + LONG, close short = BUY + SHORT
        let q = signed(
            &new_order(&entry(&t, h, Action::Close(Dir::Long), Exec::Market, &cid)).unwrap(),
        );
        assert!(q.contains("side=SELL&positionSide=LONG&"));
        let q = signed(
            &new_order(&entry(&t, h, Action::Close(Dir::Short), Exec::Market, &cid)).unwrap(),
        );
        assert!(q.contains("side=BUY&positionSide=SHORT&"));
        for action in [
            Action::Open(Dir::Long),
            Action::Open(Dir::Short),
            Action::Close(Dir::Long),
            Action::Close(Dir::Short),
        ] {
            let r = new_order(&entry(&t, h, action, limit("60000"), &cid)).unwrap();
            assert!(!r.param_names().contains(&"reduceOnly"));
        }
    }

    #[test]
    fn hedge_protective_is_full_close_market_only() {
        let t = futures();
        let r = t.lookup("BTCUSDT").unwrap();
        let cid = id("sBBBB");
        let h = Some(PositionMode::Hedge);
        let q = signed(
            &new_algo_order(
                r,
                h,
                &prot(
                    ProtectiveKind::StopMarket,
                    Dir::Long,
                    Size::ClosePosition,
                    &cid,
                ),
            )
            .unwrap(),
        );
        assert_eq!(
            q,
            "algoType=CONDITIONAL&symbol=BTCUSDT&side=SELL&positionSide=LONG&type=STOP_MARKET&triggerPrice=59000&workingType=MARK_PRICE&closePosition=true&clientAlgoId=sBBBB&recvWindow=5000&timestamp=1700000000000"
        );
        // short: stop above the market, closing side is BUY
        let mut short_stop = prot(
            ProtectiveKind::StopMarket,
            Dir::Short,
            Size::ClosePosition,
            &cid,
        );
        short_stop.trigger_price = Some(d("61000"));
        let q = signed(&new_algo_order(r, h, &short_stop).unwrap());
        assert!(q.contains("side=BUY&positionSide=SHORT&type=STOP_MARKET"));
        let mut tp = prot(
            ProtectiveKind::TakeProfitMarket,
            Dir::Long,
            Size::ClosePosition,
            &cid,
        );
        tp.trigger_price = Some(d("61000"));
        let q = signed(&new_algo_order(r, h, &tp).unwrap());
        assert!(q.contains("type=TAKE_PROFIT_MARKET") && !q.contains("reduceOnly"));

        // quantity orders, limit variants and trailing are not offered in hedge mode
        for p in [
            prot(
                ProtectiveKind::StopMarket,
                Dir::Long,
                Size::Qty(d("0.0029")),
                &cid,
            ),
            prot(
                ProtectiveKind::StopLimit,
                Dir::Long,
                Size::ClosePosition,
                &cid,
            ),
            prot(
                ProtectiveKind::Trailing,
                Dir::Long,
                Size::ClosePosition,
                &cid,
            ),
            prot(
                ProtectiveKind::Trailing,
                Dir::Long,
                Size::Qty(d("0.0029")),
                &cid,
            ),
        ] {
            assert_eq!(
                new_algo_order(r, h, &p).unwrap_err(),
                TradeError::HedgeProtectiveMustClosePosition
            );
        }
    }

    #[test]
    fn hedge_cancel_and_query_are_the_same_endpoints() {
        let t = futures();
        let r = t.lookup("BTCUSDT").unwrap();
        let by_client = OrderRef::Client(id("sBBBB"));
        assert_eq!(
            cancel_order(r, &by_client).unwrap().path(),
            "/fapi/v1/order"
        );
        assert_eq!(
            query_algo_order(&by_client).unwrap().path(),
            "/fapi/v1/algoOrder"
        );
    }

    // ── rejections ───────────────────────────────────────────────────────

    #[test]
    fn unknown_position_mode_refuses_futures_orders() {
        let t = futures();
        let r = t.lookup("BTCUSDT").unwrap();
        let cid = id("eAAAA");
        assert_eq!(
            new_order(&entry(
                &t,
                None,
                Action::Open(Dir::Long),
                Exec::Market,
                &cid
            ))
            .unwrap_err(),
            TradeError::PositionModeUnknown
        );
        assert_eq!(
            new_algo_order(
                r,
                None,
                &prot(
                    ProtectiveKind::StopMarket,
                    Dir::Long,
                    Size::ClosePosition,
                    &cid
                )
            )
            .unwrap_err(),
            TradeError::PositionModeUnknown
        );
    }

    #[test]
    fn close_position_only_for_market_types_and_no_quantity() {
        let t = futures();
        let r = t.lookup("BTCUSDT").unwrap();
        let cid = id("sBBBB");
        let o = Some(PositionMode::OneWay);
        for kind in [
            ProtectiveKind::StopLimit,
            ProtectiveKind::TakeProfitLimit,
            ProtectiveKind::Trailing,
        ] {
            let mut p = prot(kind, Dir::Long, Size::ClosePosition, &cid);
            p.limit_price = Some(d("59000"));
            assert_eq!(
                new_algo_order(r, o, &p).unwrap_err(),
                TradeError::ClosePositionNotAllowed
            );
        }
        let q = new_algo_order(
            r,
            o,
            &prot(
                ProtectiveKind::StopMarket,
                Dir::Long,
                Size::ClosePosition,
                &cid,
            ),
        )
        .unwrap();
        assert!(!q.param_names().contains(&"quantity"));
    }

    #[test]
    fn trigger_must_not_fire_immediately() {
        let t = futures();
        let r = t.lookup("BTCUSDT").unwrap();
        let cid = id("sBBBB");
        let o = Some(PositionMode::OneWay);
        let mut p = prot(
            ProtectiveKind::StopMarket,
            Dir::Long,
            Size::ClosePosition,
            &cid,
        );
        p.trigger_price = Some(d("60500")); // long stop above the market
        assert_eq!(
            new_algo_order(r, o, &p).unwrap_err(),
            TradeError::TriggerOnWrongSide
        );
        let mut p = prot(
            ProtectiveKind::TakeProfitMarket,
            Dir::Long,
            Size::ClosePosition,
            &cid,
        );
        p.trigger_price = Some(d("59000"));
        assert_eq!(
            new_algo_order(r, o, &p).unwrap_err(),
            TradeError::TriggerOnWrongSide
        );
    }

    #[test]
    fn limits_are_enforced() {
        let t = futures();
        let cid = id("eAAAA");
        let o = Some(PositionMode::OneWay);
        let mut e = entry(&t, o, Action::Open(Dir::Long), limit("60000"), &cid);
        e.qty = d("0.0009"); // rounds down to 0 with stepSize 0.001
        assert_eq!(new_order(&e).unwrap_err(), TradeError::NonPositive);
        e.qty = d("0.0029"); // rounds to 0.002; 0.002 * 30000 = 60 < 100
        e.exec = limit("30000");
        assert_eq!(new_order(&e).unwrap_err(), TradeError::NotionalBelowMin);
        e.exec = Exec::Market;
        e.qty = d("121"); // above MARKET_LOT_SIZE max 120
        assert_eq!(new_order(&e).unwrap_err(), TradeError::QtyAboveMax);
        // a market order without a reference price cannot be checked
        e.qty = d("0.0029");
        e.ref_price = None;
        assert_eq!(
            new_order(&e).unwrap_err(),
            TradeError::MissingReferencePrice
        );
        // closing a small position is allowed
        e.action = Action::Close(Dir::Long);
        e.qty = d("0.001");
        assert!(new_order(&e).is_ok());
    }

    #[test]
    fn rules_of_another_product_are_refused() {
        let spot_t = spot();
        let cid = id("eAAAA");
        let e = FuturesEntry {
            rules: spot_t.lookup("BTCUSDT").unwrap(),
            mode: Some(PositionMode::OneWay),
            action: Action::Open(Dir::Long),
            qty: d("1"),
            exec: Exec::Market,
            ref_price: Some(d("1")),
            client_id: dup(&cid),
        };
        assert_eq!(new_order(&e).unwrap_err(), TradeError::ProductMismatch);
    }

    #[test]
    fn order_ids_are_digits_only() {
        for bad in ["", "12a", "1&symbol=X", "-1", "1 2"] {
            assert!(OrderRef::exchange(bad).is_err(), "{bad}");
        }
    }
}
