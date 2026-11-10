//! Binance spot request builders (pure: parameters only, nothing is sent).
//!
//! `POST /api/v3/order` for LIMIT, MARKET, STOP_LOSS(_LIMIT), TAKE_PROFIT(_LIMIT) and
//! trailing stops (`trailingDelta`); `POST /api/v3/orderList/oco` and `/orderList/otoco`
//! for protective pairs. The deprecated `/api/v3/order/oco` is not used (condition 79).
//! Spot has no `reduceOnly`: a protective sell can sell the asset from any source.

use super::checks::{check_order, check_qty};
use super::client_id::{ClientId, IdField};
use super::common::{check_trigger_side, push_ref};
use super::decimal::Decimal;
use super::request::{BinanceRequest, Method};
use super::rules::{SymbolRules, TradeProduct};
use super::types::{
    Dir, Exec, OrderRef, Protective, ProtectiveKind, Side, Size, TimeInForce, TradeError, Trailing,
};
use std::cmp::Ordering;

const PRODUCT: TradeProduct = TradeProduct::BinanceSpot;

#[derive(Debug)]
pub struct SpotEntry<'a> {
    pub rules: &'a SymbolRules,
    pub side: Side,
    pub qty: Decimal,
    pub exec: Exec,
    /// Last price; needed for the notional check of a market order.
    pub ref_price: Option<Decimal>,
    /// Owned: one id belongs to one request (condition 61).
    pub client_id: ClientId,
}

#[cfg(test)]
impl SpotEntry<'_> {
    /// Tests build the same order several times; production code cannot (condition 61).
    pub(crate) fn duplicate(&self) -> SpotEntry<'_> {
        SpotEntry {
            rules: self.rules,
            side: self.side,
            qty: self.qty,
            exec: self.exec,
            ref_price: self.ref_price,
            client_id: ClientId::parse(self.client_id.as_str()).unwrap(),
        }
    }
}

/// `POST /api/v3/order` (LIMIT or MARKET).
pub fn new_order(i: SpotEntry<'_>) -> Result<BinanceRequest, TradeError> {
    i.rules.require_product(PRODUCT)?;
    let id = i.client_id.check(IdField::BinanceSpot)?;
    let market = matches!(i.exec, Exec::Market);
    let qty = i.rules.round_qty(&i.qty, market)?;
    let (limit_price, tif) = match i.exec {
        Exec::Limit { price, tif } => (Some(i.rules.round_limit_price(i.side, &price)?), Some(tif)),
        Exec::Market => (None, None),
    };
    let price = limit_price.as_ref().or(i.ref_price.as_ref());
    let order_value = check_order(i.rules, &qty, price, market)?;

    let mut r = BinanceRequest::new(Method::Post, "/api/v3/order");
    r.push("symbol", i.rules.symbol().as_str());
    r.push("side", i.side.binance());
    r.push("type", if market { "MARKET" } else { "LIMIT" });
    if let Some(tif) = tif {
        r.push("timeInForce", tif.binance());
    }
    r.push("quantity", qty.to_plain_string());
    r.push_opt("price", limit_price.map(|p| p.to_plain_string()));
    r.push("newClientOrderId", id);
    // A plain spot sell is not reduce-only: a repeat would sell more of the holding, and in a
    // non-USDT pair it buys the quote asset. It counts as adding (F4); only the protective
    // orders bound to a holding stay `Reduces`.
    Ok(r.adding(order_value))
}

fn order_type(kind: ProtectiveKind) -> &'static str {
    match kind {
        // A trailing stop is a STOP_LOSS with `trailingDelta`.
        ProtectiveKind::StopMarket | ProtectiveKind::Trailing => "STOP_LOSS",
        ProtectiveKind::TakeProfitMarket => "TAKE_PROFIT",
        ProtectiveKind::StopLimit => "STOP_LOSS_LIMIT",
        ProtectiveKind::TakeProfitLimit => "TAKE_PROFIT_LIMIT",
    }
}

/// `POST /api/v3/order`: a protective sell of a spot holding (long only). The sell
/// quantity must be checked against the available balance by the caller
/// ([`super::checks::check_protective_qty_available`]).
pub(crate) fn build_protective_order(
    rules: &SymbolRules,
    p: Protective,
) -> Result<BinanceRequest, TradeError> {
    rules.require_product(PRODUCT)?;
    let id = p.client_id.check(IdField::BinanceSpot)?;
    if p.dir != Dir::Long {
        return Err(TradeError::Unsupported); // spot cannot be short
    }
    let Size::Qty(qty) = p.size else {
        return Err(TradeError::ClosePositionNotAllowed); // spot has no closePosition
    };
    let side = Side::Sell;
    let market = p.kind.is_market() || p.kind == ProtectiveKind::Trailing;
    let qty = rules.round_qty(&qty, market)?;

    let trigger = match p.trigger_price {
        Some(t) => {
            let t = rules.round_trigger_price(&t)?;
            check_trigger_side(p.kind, p.dir, &t, &p.ref_price)?;
            Some(t)
        }
        None if p.kind == ProtectiveKind::Trailing => None,
        None => return Err(TradeError::PriceInvalid),
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
    let trailing_delta = match (p.kind, p.trailing) {
        (ProtectiveKind::Trailing, Some(Trailing::DeltaBips(bips))) => {
            // STOP_LOSS SELL uses the "below" range (TRAILING_DELTA filter).
            let rule = rules.trailing_delta.ok_or(TradeError::RuleMissing)?;
            if !(rule.min_below..=rule.max_below).contains(&bips) {
                return Err(TradeError::TrailingDeltaOutOfRange);
            }
            Some(bips)
        }
        (ProtectiveKind::Trailing, _) => return Err(TradeError::PriceInvalid),
        (_, None) => None,
        (_, Some(_)) => return Err(TradeError::PriceInvalid),
    };

    let notional_price = trigger.as_ref().or(Some(&p.ref_price));
    check_order(rules, &qty, notional_price, market)?;

    let mut r = BinanceRequest::new(Method::Post, "/api/v3/order");
    r.push("symbol", rules.symbol().as_str());
    r.push("side", side.binance());
    r.push("type", order_type(p.kind));
    if limit_price.is_some() {
        r.push("timeInForce", p.tif.binance());
    }
    r.push("quantity", qty.to_plain_string());
    r.push_opt("price", limit_price.map(|l| l.to_plain_string()));
    r.push_opt("stopPrice", trigger.map(|t| t.to_plain_string()));
    r.push_opt("trailingDelta", trailing_delta.map(|b| b.to_string()));
    r.push("newClientOrderId", id);
    Ok(r.reducing())
}

/// The sell pair of an OCO or OTOCO: a take-profit `LIMIT_MAKER` above and a stop below.
#[derive(Debug)]
pub struct SpotExits {
    pub take_profit_price: Decimal,
    pub stop_trigger: Decimal,
    /// `Some` makes the stop a `STOP_LOSS_LIMIT` with this limit price.
    pub stop_limit_price: Option<Decimal>,
    pub stop_tif: TimeInForce,
    pub take_profit_id: ClientId,
    pub stop_id: ClientId,
}

#[cfg(test)]
impl SpotExits {
    pub(crate) fn duplicate(&self) -> SpotExits {
        SpotExits {
            take_profit_price: self.take_profit_price,
            stop_trigger: self.stop_trigger,
            stop_limit_price: self.stop_limit_price,
            stop_tif: self.stop_tif,
            take_profit_id: ClientId::parse(self.take_profit_id.as_str()).unwrap(),
            stop_id: ClientId::parse(self.stop_id.as_str()).unwrap(),
        }
    }
}

struct Exits {
    tp: Decimal,
    stop: Decimal,
    stop_limit: Option<Decimal>,
}

fn round_exits(rules: &SymbolRules, e: &SpotExits) -> Result<Exits, TradeError> {
    e.take_profit_id.check(IdField::BinanceSpot)?;
    e.stop_id.check(IdField::BinanceSpot)?;
    // Sells: limit prices round up, the stop trigger to the nearest tick.
    Ok(Exits {
        tp: rules.round_limit_price(Side::Sell, &e.take_profit_price)?,
        stop: rules.round_trigger_price(&e.stop_trigger)?,
        stop_limit: match e.stop_limit_price {
            Some(l) => Some(rules.round_limit_price(Side::Sell, &l)?),
            None => None,
        },
    })
}

/// Every id of one list must be different (the exchange would reject duplicates, and a
/// reused id would make cancel and query ambiguous).
fn check_distinct(ids: &[&str]) -> Result<(), TradeError> {
    for (n, a) in ids.iter().enumerate() {
        if ids[n + 1..].contains(a) {
            return Err(TradeError::ClientIdsNotDistinct);
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct SpotOco<'a> {
    pub rules: &'a SymbolRules,
    pub qty: Decimal,
    /// Last price: `stop < last < take profit` must hold.
    pub ref_price: Decimal,
    pub exits: SpotExits,
    pub list_id: ClientId,
}

#[cfg(test)]
impl SpotOco<'_> {
    pub(crate) fn duplicate(&self) -> SpotOco<'_> {
        SpotOco {
            rules: self.rules,
            qty: self.qty,
            ref_price: self.ref_price,
            exits: self.exits.duplicate(),
            list_id: ClientId::parse(self.list_id.as_str()).unwrap(),
        }
    }
}

/// `POST /api/v3/orderList/oco`: sell with a take-profit above and a stop below.
pub(crate) fn build_oco(i: SpotOco<'_>) -> Result<BinanceRequest, TradeError> {
    i.rules.require_product(PRODUCT)?;
    let list_id = i.list_id.check(IdField::BinanceSpot)?;
    let ex = round_exits(i.rules, &i.exits)?;
    check_distinct(&[
        list_id,
        i.exits.take_profit_id.as_str(),
        i.exits.stop_id.as_str(),
    ])?;
    let qty = i.rules.round_qty(&i.qty, false)?;
    if ex.stop.checked_cmp(&i.ref_price)? != Ordering::Less
        || ex.tp.checked_cmp(&i.ref_price)? != Ordering::Greater
    {
        return Err(TradeError::TriggerOnWrongSide);
    }
    check_order(i.rules, &qty, Some(&ex.stop), false)?;

    let mut r = BinanceRequest::new(Method::Post, "/api/v3/orderList/oco");
    r.push("symbol", i.rules.symbol().as_str());
    r.push("listClientOrderId", list_id);
    r.push("side", "SELL");
    r.push("quantity", qty.to_plain_string());
    r.push("aboveType", "LIMIT_MAKER");
    r.push("aboveClientOrderId", i.exits.take_profit_id.as_str());
    r.push("abovePrice", ex.tp.to_plain_string());
    push_below(&mut r, OCO_BELOW, &ex, &i.exits);
    Ok(r.reducing())
}

/// Parameter names of the stop leg: `[type, clientOrderId, price, stopPrice, timeInForce]`.
type BelowNames = [&'static str; 5];

const OCO_BELOW: BelowNames = [
    "belowType",
    "belowClientOrderId",
    "belowPrice",
    "belowStopPrice",
    "belowTimeInForce",
];

const OTOCO_BELOW: BelowNames = [
    "pendingBelowType",
    "pendingBelowClientOrderId",
    "pendingBelowPrice",
    "pendingBelowStopPrice",
    "pendingBelowTimeInForce",
];

fn push_below(r: &mut BinanceRequest, names: BelowNames, ex: &Exits, e: &SpotExits) {
    r.push(
        names[0],
        if ex.stop_limit.is_some() {
            "STOP_LOSS_LIMIT"
        } else {
            "STOP_LOSS"
        },
    );
    r.push(names[1], e.stop_id.as_str());
    r.push_opt(names[2], ex.stop_limit.map(|l| l.to_plain_string()));
    r.push(names[3], ex.stop.to_plain_string());
    if ex.stop_limit.is_some() {
        r.push(names[4], e.stop_tif.binance());
    }
}

#[derive(Debug)]
pub struct SpotOtoco<'a> {
    pub rules: &'a SymbolRules,
    pub qty: Decimal,
    /// Working order: a buy `LIMIT` that, once fully filled, places the exit pair.
    pub entry_price: Decimal,
    pub entry_tif: TimeInForce,
    pub exits: SpotExits,
    pub list_id: ClientId,
    pub entry_id: ClientId,
}

#[cfg(test)]
impl SpotOtoco<'_> {
    pub(crate) fn duplicate(&self) -> SpotOtoco<'_> {
        SpotOtoco {
            rules: self.rules,
            qty: self.qty,
            entry_price: self.entry_price,
            entry_tif: self.entry_tif,
            exits: self.exits.duplicate(),
            list_id: ClientId::parse(self.list_id.as_str()).unwrap(),
            entry_id: ClientId::parse(self.entry_id.as_str()).unwrap(),
        }
    }
}

/// `POST /api/v3/orderList/otoco`: a buy limit that triggers a sell OCO pair.
#[cfg(test)]
pub(crate) fn build_otoco(i: SpotOtoco<'_>) -> Result<BinanceRequest, TradeError> {
    otoco_with_fee(i, 0)
}

fn otoco_with_fee(i: SpotOtoco<'_>, fee_bps: u32) -> Result<BinanceRequest, TradeError> {
    i.rules.require_product(PRODUCT)?;
    let list_id = i.list_id.check(IdField::BinanceSpot)?;
    let entry_id = i.entry_id.check(IdField::BinanceSpot)?;
    let ex = round_exits(i.rules, &i.exits)?;
    check_distinct(&[
        list_id,
        entry_id,
        i.exits.take_profit_id.as_str(),
        i.exits.stop_id.as_str(),
    ])?;
    let entry = i.rules.round_limit_price(Side::Buy, &i.entry_price)?;
    let qty = i.rules.round_qty(&i.qty, false)?;
    // stop < entry < take profit: the pair must bracket the entry price.
    if ex.stop.checked_cmp(&entry)? != Ordering::Less
        || ex.tp.checked_cmp(&entry)? != Ordering::Greater
    {
        return Err(TradeError::TriggerOnWrongSide);
    }
    let order_value = check_order(i.rules, &qty, Some(&entry), false)?;
    check_qty(i.rules, &qty, false)?;
    // The buy fee is taken from the base asset, so the exit pair may only sell what is left.
    let pending_qty = after_fee(i.rules, &qty, fee_bps)?;
    check_qty(i.rules, &pending_qty, false)?;

    let mut r = BinanceRequest::new(Method::Post, "/api/v3/orderList/otoco");
    r.push("symbol", i.rules.symbol().as_str());
    r.push("listClientOrderId", list_id);
    r.push("workingType", "LIMIT");
    r.push("workingSide", "BUY");
    r.push("workingClientOrderId", entry_id);
    r.push("workingPrice", entry.to_plain_string());
    r.push("workingQuantity", qty.to_plain_string());
    r.push("workingTimeInForce", i.entry_tif.binance());
    r.push("pendingSide", "SELL");
    r.push("pendingQuantity", pending_qty.to_plain_string());
    r.push("pendingAboveType", "LIMIT_MAKER");
    r.push("pendingAboveClientOrderId", i.exits.take_profit_id.as_str());
    r.push("pendingAbovePrice", ex.tp.to_plain_string());
    push_below(&mut r, OTOCO_BELOW, &ex, &i.exits);
    Ok(r.adding(order_value))
}

/// `quantity * (10000 - fee_bps) / 10000`, rounded down to the lot step.
fn after_fee(rules: &SymbolRules, qty: &Decimal, fee_bps: u32) -> Result<Decimal, TradeError> {
    if fee_bps >= 10_000 {
        return Err(TradeError::NonPositive);
    }
    let factor =
        Decimal::from_int(i64::from(10_000 - fee_bps)).checked_mul(&Decimal::parse("0.0001")?)?;
    rules.round_qty(&qty.checked_mul(&factor)?, false)
}

/// `POST /api/v3/order` (protective sell). The sell quantity may not exceed `available`, the
/// free base-asset balance after fees (condition 64).
pub fn new_protective_order(
    rules: &SymbolRules,
    p: Protective,
    available: &Decimal,
) -> Result<BinanceRequest, TradeError> {
    if let Size::Qty(q) = p.size {
        super::checks::check_protective_qty_available(&rules.round_qty(&q, false)?, available)?;
    }
    build_protective_order(rules, p)
}

/// `POST /api/v3/orderList/oco`; the quantity may not exceed `available` (free balance
/// after fees).
pub fn new_oco(i: SpotOco<'_>, available: &Decimal) -> Result<BinanceRequest, TradeError> {
    super::checks::check_protective_qty_available(&i.rules.round_qty(&i.qty, false)?, available)?;
    build_oco(i)
}

/// `POST /api/v3/orderList/otoco`; the exit pair sells the filled quantity less the buy fee
/// (`fee_bps`, taken in the base asset, rounded down). A fee paid in BNB is not assumed.
pub fn new_otoco(i: SpotOtoco<'_>, fee_bps: u32) -> Result<BinanceRequest, TradeError> {
    otoco_with_fee(i, fee_bps)
}

/// `DELETE /api/v3/order`.
pub fn cancel_order(rules: &SymbolRules, order: &OrderRef) -> Result<BinanceRequest, TradeError> {
    order_request(Method::Delete, rules, order)
}

/// `GET /api/v3/order`.
pub fn query_order(rules: &SymbolRules, order: &OrderRef) -> Result<BinanceRequest, TradeError> {
    order_request(Method::Get, rules, order)
}

fn order_request(
    method: Method,
    rules: &SymbolRules,
    order: &OrderRef,
) -> Result<BinanceRequest, TradeError> {
    rules.require_product(PRODUCT)?;
    let mut r = BinanceRequest::new(method, "/api/v3/order");
    r.push("symbol", rules.symbol().as_str());
    push_ref(
        &mut r,
        order,
        "orderId",
        "origClientOrderId",
        IdField::BinanceSpot,
    )?;
    Ok(r)
}

/// `DELETE /api/v3/orderList` (an OCO or OTOCO as a whole).
pub fn cancel_order_list(
    rules: &SymbolRules,
    list: &OrderRef,
) -> Result<BinanceRequest, TradeError> {
    rules.require_product(PRODUCT)?;
    let mut r = BinanceRequest::new(Method::Delete, "/api/v3/orderList");
    r.push("symbol", rules.symbol().as_str());
    push_ref(
        &mut r,
        list,
        "orderListId",
        "listClientOrderId",
        IdField::BinanceSpot,
    )?;
    Ok(r)
}

/// `GET /api/v3/orderList` (`orderListId` or `origClientOrderId`; no symbol).
pub fn query_order_list(list: &OrderRef) -> Result<BinanceRequest, TradeError> {
    let mut r = BinanceRequest::new(Method::Get, "/api/v3/orderList");
    push_ref(
        &mut r,
        list,
        "orderListId",
        "origClientOrderId",
        IdField::BinanceSpot,
    )?;
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::super::rules::fixtures::*;
    use super::super::types::WorkingType;
    use super::*;

    const SECRET: &str = "test-secret-not-real";
    const TS: i64 = 1_700_000_000_000;

    fn id(s: &str) -> ClientId {
        ClientId::parse(s).unwrap()
    }

    // The builders consume their input (one id, one request). These wrappers let a test build
    // the same input again, with a copy of the id.
    fn dup(c: &ClientId) -> ClientId {
        id(c.as_str())
    }

    fn new_order(i: &SpotEntry<'_>) -> Result<BinanceRequest, TradeError> {
        super::new_order(i.duplicate())
    }

    fn build_protective_order(
        rules: &SymbolRules,
        p: &Protective,
    ) -> Result<BinanceRequest, TradeError> {
        super::build_protective_order(rules, p.duplicate())
    }

    fn build_oco(i: &SpotOco<'_>) -> Result<BinanceRequest, TradeError> {
        super::build_oco(i.duplicate())
    }

    fn build_otoco(i: &SpotOtoco<'_>) -> Result<BinanceRequest, TradeError> {
        super::build_otoco(i.duplicate())
    }

    fn new_oco(i: &SpotOco<'_>, available: &Decimal) -> Result<BinanceRequest, TradeError> {
        super::new_oco(i.duplicate(), available)
    }

    fn new_otoco(i: &SpotOtoco<'_>, fee_bps: u32) -> Result<BinanceRequest, TradeError> {
        super::new_otoco(i.duplicate(), fee_bps)
    }

    fn signed(r: &BinanceRequest) -> String {
        let s = r.signed(SECRET, 5000, TS).unwrap();
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

    fn prot(kind: ProtectiveKind, cid: &ClientId) -> Protective {
        Protective {
            kind,
            dir: Dir::Long,
            size: Size::Qty(d("0.12345678")),
            trigger_price: Some(d("59000.004")),
            limit_price: None,
            tif: TimeInForce::Gtc,
            working_type: WorkingType::ContractPrice,
            trailing: None,
            ref_price: d("60000"),
            client_id: dup(cid),
        }
    }

    const TAIL: &str = "&recvWindow=5000&timestamp=1700000000000";

    #[test]
    fn spot_entry_limit_and_market() {
        let t = spot();
        let rules = t.lookup("BTCUSDT").unwrap();
        let cid = id("eAAAA");
        let e = SpotEntry {
            rules,
            side: Side::Buy,
            qty: d("0.12345678"),
            exec: Exec::Limit {
                price: d("59999.999"),
                tif: TimeInForce::Gtc,
            },
            ref_price: None,
            client_id: dup(&cid),
        };
        assert_eq!(
            signed(&new_order(&e).unwrap()),
            format!(
                "symbol=BTCUSDT&side=BUY&type=LIMIT&timeInForce=GTC&quantity=0.12345&price=59999.99&newClientOrderId=eAAAA{TAIL}"
            )
        );
        let m = SpotEntry {
            exec: Exec::Market,
            ref_price: Some(d("60000")),
            side: Side::Sell,
            ..e.duplicate()
        };
        assert_eq!(
            signed(&new_order(&m).unwrap()),
            format!(
                "symbol=BTCUSDT&side=SELL&type=MARKET&quantity=0.12345&newClientOrderId=eAAAA{TAIL}"
            )
        );
        // a market order cannot be checked against min notional without a reference price
        let m = SpotEntry {
            ref_price: None,
            ..m.duplicate()
        };
        assert_eq!(
            new_order(&m).unwrap_err(),
            TradeError::MissingReferencePrice
        );
    }

    #[test]
    fn spot_protective_stop_take_profit_and_limit_variants() {
        let t = spot();
        let rules = t.lookup("BTCUSDT").unwrap();
        let cid = id("sBBBB");
        assert_eq!(
            signed(
                &build_protective_order(rules, &prot(ProtectiveKind::StopMarket, &cid)).unwrap()
            ),
            format!(
                "symbol=BTCUSDT&side=SELL&type=STOP_LOSS&quantity=0.12345&stopPrice=59000&newClientOrderId=sBBBB{TAIL}"
            )
        );
        let mut tp = prot(ProtectiveKind::TakeProfitMarket, &cid);
        tp.trigger_price = Some(d("61000"));
        assert_eq!(
            signed(&build_protective_order(rules, &tp).unwrap()),
            format!(
                "symbol=BTCUSDT&side=SELL&type=TAKE_PROFIT&quantity=0.12345&stopPrice=61000&newClientOrderId=sBBBB{TAIL}"
            )
        );
        let mut sl = prot(ProtectiveKind::StopLimit, &cid);
        sl.limit_price = Some(d("58990.001"));
        assert_eq!(
            signed(&build_protective_order(rules, &sl).unwrap()),
            format!(
                "symbol=BTCUSDT&side=SELL&type=STOP_LOSS_LIMIT&timeInForce=GTC&quantity=0.12345&price=58990.01&stopPrice=59000&newClientOrderId=sBBBB{TAIL}"
            )
        );
    }

    #[test]
    fn spot_trailing_delta_is_range_checked() {
        let t = spot();
        let rules = t.lookup("BTCUSDT").unwrap();
        let cid = id("rCCCC");
        let mut tr = prot(ProtectiveKind::Trailing, &cid);
        tr.trigger_price = None;
        tr.trailing = Some(Trailing::DeltaBips(150));
        assert_eq!(
            signed(&build_protective_order(rules, &tr).unwrap()),
            format!(
                "symbol=BTCUSDT&side=SELL&type=STOP_LOSS&quantity=0.12345&trailingDelta=150&newClientOrderId=rCCCC{TAIL}"
            )
        );
        for bips in [9, 2001, 0, u32::MAX] {
            tr.trailing = Some(Trailing::DeltaBips(bips));
            assert_eq!(
                build_protective_order(rules, &tr).unwrap_err(),
                TradeError::TrailingDeltaOutOfRange
            );
        }
        for bips in [10, 2000] {
            tr.trailing = Some(Trailing::DeltaBips(bips));
            assert!(build_protective_order(rules, &tr).is_ok());
        }
        // a symbol without a TRAILING_DELTA filter cannot take a trailing stop
        let odd = t.lookup("ODDUSDT").unwrap();
        tr.trailing = Some(Trailing::DeltaBips(150));
        tr.ref_price = d("100");
        tr.size = Size::Qty(d("2"));
        assert_eq!(
            build_protective_order(odd, &tr).unwrap_err(),
            TradeError::RuleMissing
        );
    }

    #[test]
    fn spot_protective_rejects_short_and_close_position() {
        let t = spot();
        let rules = t.lookup("BTCUSDT").unwrap();
        let cid = id("sBBBB");
        let mut p = prot(ProtectiveKind::StopMarket, &cid);
        p.dir = Dir::Short;
        assert_eq!(
            build_protective_order(rules, &p).unwrap_err(),
            TradeError::Unsupported
        );
        let mut p = prot(ProtectiveKind::StopMarket, &cid);
        p.size = Size::ClosePosition;
        assert_eq!(
            build_protective_order(rules, &p).unwrap_err(),
            TradeError::ClosePositionNotAllowed
        );
        let mut p = prot(ProtectiveKind::StopMarket, &cid);
        p.trigger_price = Some(d("60500"));
        assert_eq!(
            build_protective_order(rules, &p).unwrap_err(),
            TradeError::TriggerOnWrongSide
        );
    }

    fn exits(tp_id: &ClientId, sl_id: &ClientId, limit: Option<&str>) -> SpotExits {
        SpotExits {
            take_profit_price: d("61000.001"),
            stop_trigger: d("59000.004"),
            stop_limit_price: limit.map(d),
            stop_tif: TimeInForce::Gtc,
            take_profit_id: dup(tp_id),
            stop_id: dup(sl_id),
        }
    }

    #[test]
    fn spot_oco_uses_order_list_oco() {
        let t = spot();
        let rules = t.lookup("BTCUSDT").unwrap();
        let (list, tp, sl) = (id("lDDDD"), id("tEEEE"), id("sFFFF"));
        let i = SpotOco {
            rules,
            qty: d("0.12345678"),
            ref_price: d("60000"),
            exits: exits(&tp, &sl, None),
            list_id: dup(&list),
        };
        let r = build_oco(&i).unwrap();
        assert_eq!(r.path(), "/api/v3/orderList/oco");
        assert_eq!(
            signed(&r),
            format!(
                "symbol=BTCUSDT&listClientOrderId=lDDDD&side=SELL&quantity=0.12345&aboveType=LIMIT_MAKER&aboveClientOrderId=tEEEE&abovePrice=61000.01&belowType=STOP_LOSS&belowClientOrderId=sFFFF&belowStopPrice=59000{TAIL}"
            )
        );
        let i2 = SpotOco {
            exits: exits(&tp, &sl, Some("58990.001")),
            ..i.duplicate()
        };
        assert_eq!(
            signed(&build_oco(&i2).unwrap()),
            format!(
                "symbol=BTCUSDT&listClientOrderId=lDDDD&side=SELL&quantity=0.12345&aboveType=LIMIT_MAKER&aboveClientOrderId=tEEEE&abovePrice=61000.01&belowType=STOP_LOSS_LIMIT&belowClientOrderId=sFFFF&belowPrice=58990.01&belowStopPrice=59000&belowTimeInForce=GTC{TAIL}"
            )
        );
        // last price outside the pair
        let bad = SpotOco {
            ref_price: d("62000"),
            ..i.duplicate()
        };
        assert_eq!(build_oco(&bad).unwrap_err(), TradeError::TriggerOnWrongSide);
    }

    #[test]
    fn spot_otoco_uses_order_list_otoco() {
        let t = spot();
        let rules = t.lookup("BTCUSDT").unwrap();
        let (list, tp, sl, entry) = (id("lDDDD"), id("tEEEE"), id("sFFFF"), id("eGGGG"));
        let i = SpotOtoco {
            rules,
            qty: d("0.12345678"),
            entry_price: d("60000.009"),
            entry_tif: TimeInForce::Gtc,
            exits: exits(&tp, &sl, None),
            list_id: dup(&list),
            entry_id: dup(&entry),
        };
        let r = build_otoco(&i).unwrap();
        assert_eq!(r.path(), "/api/v3/orderList/otoco");
        assert_eq!(
            signed(&r),
            format!(
                "symbol=BTCUSDT&listClientOrderId=lDDDD&workingType=LIMIT&workingSide=BUY&workingClientOrderId=eGGGG&workingPrice=60000&workingQuantity=0.12345&workingTimeInForce=GTC&pendingSide=SELL&pendingQuantity=0.12345&pendingAboveType=LIMIT_MAKER&pendingAboveClientOrderId=tEEEE&pendingAbovePrice=61000.01&pendingBelowType=STOP_LOSS&pendingBelowClientOrderId=sFFFF&pendingBelowStopPrice=59000{TAIL}"
            )
        );
        let bad = SpotOtoco {
            entry_price: d("58000"),
            ..i.duplicate()
        };
        assert_eq!(
            build_otoco(&bad).unwrap_err(),
            TradeError::TriggerOnWrongSide
        );
    }

    #[test]
    fn spot_cancel_and_query_for_orders_and_lists() {
        let t = spot();
        let rules = t.lookup("BTCUSDT").unwrap();
        let by_id = OrderRef::exchange("42").unwrap();
        let by_client = OrderRef::Client(id("lDDDD"));
        assert_eq!(
            signed(&cancel_order(rules, &by_id).unwrap()),
            format!("symbol=BTCUSDT&orderId=42{TAIL}")
        );
        assert_eq!(
            signed(&query_order(rules, &by_client).unwrap()),
            format!("symbol=BTCUSDT&origClientOrderId=lDDDD{TAIL}")
        );
        let c = cancel_order_list(rules, &by_client).unwrap();
        assert_eq!(
            (c.method(), c.path()),
            (Method::Delete, "/api/v3/orderList")
        );
        assert_eq!(
            signed(&c),
            format!("symbol=BTCUSDT&listClientOrderId=lDDDD{TAIL}")
        );
        assert_eq!(
            signed(&cancel_order_list(rules, &by_id).unwrap()),
            format!("symbol=BTCUSDT&orderListId=42{TAIL}")
        );
        let q = query_order_list(&by_client).unwrap();
        assert_eq!((q.method(), q.path()), (Method::Get, "/api/v3/orderList"));
        assert_eq!(signed(&q), format!("origClientOrderId=lDDDD{TAIL}"));
    }

    #[test]
    fn spot_limits_are_enforced() {
        let t = spot();
        let rules = t.lookup("BTCUSDT").unwrap();
        let cid = id("eAAAA");
        let e = SpotEntry {
            rules,
            side: Side::Buy,
            qty: d("0.00001"),
            exec: Exec::Limit {
                price: d("100"),
                tif: TimeInForce::Gtc,
            },
            ref_price: None,
            client_id: dup(&cid),
        };
        // 0.00001 * 100 = 0.001 < 5
        assert_eq!(new_order(&e).unwrap_err(), TradeError::NotionalBelowMin);
        let e = SpotEntry {
            qty: d("0.000001"),
            ..e.duplicate()
        };
        assert_eq!(new_order(&e).unwrap_err(), TradeError::NonPositive);
    }

    #[test]
    fn oco_and_otoco_ids_must_differ() {
        let t = spot();
        let rules = t.lookup("BTCUSDT").unwrap();
        let (list, tp, sl, entry) = (id("lDDDD"), id("tEEEE"), id("sFFFF"), id("eGGGG"));
        let oco = |list: &ClientId, tp: &ClientId, sl: &ClientId| {
            build_oco(&SpotOco {
                rules,
                qty: d("0.12345678"),
                ref_price: d("60000"),
                exits: exits(tp, sl, None),
                list_id: dup(list),
            })
        };
        assert!(oco(&list, &tp, &sl).is_ok());
        for r in [oco(&tp, &tp, &sl), oco(&list, &sl, &sl), oco(&sl, &tp, &sl)] {
            assert_eq!(r.unwrap_err(), TradeError::ClientIdsNotDistinct);
        }

        let otoco = |list: &ClientId, entry: &ClientId, tp: &ClientId, sl: &ClientId| {
            build_otoco(&SpotOtoco {
                rules,
                qty: d("0.12345678"),
                entry_price: d("60000"),
                entry_tif: TimeInForce::Gtc,
                exits: exits(tp, sl, None),
                list_id: dup(list),
                entry_id: dup(entry),
            })
        };
        assert!(otoco(&list, &entry, &tp, &sl).is_ok());
        for r in [
            otoco(&list, &list, &tp, &sl),
            otoco(&list, &entry, &list, &sl),
            otoco(&list, &entry, &tp, &list),
            otoco(&list, &entry, &entry, &sl),
            otoco(&list, &entry, &tp, &entry),
            otoco(&list, &entry, &sl, &sl),
        ] {
            assert_eq!(r.unwrap_err(), TradeError::ClientIdsNotDistinct);
        }
    }

    #[test]
    fn otoco_exit_quantity_is_floored_by_the_buy_fee() {
        let t = spot();
        let rules = t.lookup("BTCUSDT").unwrap();
        let (list, tp, sl, entry) = (id("lDDDD"), id("tEEEE"), id("sFFFF"), id("eGGGG"));
        let i = SpotOtoco {
            rules,
            qty: d("0.12345"),
            entry_price: d("60000"),
            entry_tif: TimeInForce::Gtc,
            exits: exits(&tp, &sl, None),
            list_id: dup(&list),
            entry_id: dup(&entry),
        };
        // 10 bps fee: 0.12345 * 0.999 = 0.12332655, floored to the 0.00001 step
        let s = signed(&new_otoco(&i, 10).unwrap());
        assert!(s.contains("workingQuantity=0.12345&"), "{s}");
        assert!(s.contains("pendingQuantity=0.12332&"), "{s}");
        // no fee: the old behaviour
        assert!(signed(&new_otoco(&i, 0).unwrap()).contains("pendingQuantity=0.12345&"));
    }

    #[test]
    fn oco_sell_cannot_exceed_the_available_balance() {
        let t = spot();
        let rules = t.lookup("BTCUSDT").unwrap();
        let (list, tp, sl) = (id("lDDDD"), id("tEEEE"), id("sFFFF"));
        let i = SpotOco {
            rules,
            qty: d("0.1"),
            ref_price: d("60000"),
            exits: exits(&tp, &sl, None),
            list_id: dup(&list),
        };
        assert!(new_oco(&i, &d("0.09")).is_err());
        assert!(new_oco(&i, &d("0.1")).is_ok());
    }
}
