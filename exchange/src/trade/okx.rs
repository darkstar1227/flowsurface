//! OKX request builders (pure: parameters only, nothing is sent).
//!
//! `POST /api/v5/trade/order` (with `attachAlgoOrds`), `POST /api/v5/trade/order-algo`
//! (`conditional` and `oco`), `cancel-order`, `cancel-algos`, and the query endpoints.
//! Bodies are typed structs, so the field order is fixed and the JSON that is signed is
//! the JSON that is sent. `move_order_stop` (trailing stop) is refused until its cancel
//! endpoint is verified on the demo environment (condition 67).

use super::checks::check_order;
use super::client_id::{ClientId, IdField};
use super::common::check_trigger_side;
use super::decimal::Decimal;
use super::request::{
    Method, OkxAlgoBody, OkxAttachAlgo, OkxBody, OkxCancelAlgo, OkxCancelOrderBody, OkxOrderBody,
    OkxRequest,
};
use super::rules::{SymbolRules, TradeProduct};
use super::types::{Action, Dir, Exec, MarginMode, OrderRef, Side, Size, TimeInForce, TradeError};
use crate::account::PositionMode;

/// What the order trades: spot, or a swap in a given position mode and margin mode.
#[derive(Debug, Clone, Copy)]
pub enum OkxTarget {
    Spot {
        side: Side,
    },
    Swap {
        /// `OneWay` = net, `Hedge` = long_short. `None` refuses the order (condition 58).
        mode: Option<PositionMode>,
        margin: MarginMode,
        action: Action,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerPxType {
    Last,
    Index,
    Mark,
}

impl TriggerPxType {
    fn okx(self) -> &'static str {
        match self {
            TriggerPxType::Last => "last",
            TriggerPxType::Index => "index",
            TriggerPxType::Mark => "mark",
        }
    }
}

/// OKX `order-algo` order types the app knows. Only `Conditional` and `Oco` are built; the
/// others exist so that cancel-all can find (and report) every kind of pending algo order
/// (condition 68, review item 15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OkxAlgoType {
    /// One trigger, TP or SL.
    Conditional,
    /// TP and SL together (the only way to hold both).
    Oco,
    /// Trailing stop: refused (condition 67).
    MoveOrderStop,
    /// Trigger order (read and cancel only).
    Trigger,
    /// Chase order, futures and swaps only (read and cancel only; not named by the docs as
    /// excluded from `cancel-algos`, not verified on the demo trading environment).
    Chase,
    Iceberg,
    Twap,
    SmartIceberg,
}

impl OkxAlgoType {
    /// Every `ordType` of `orders-algo-pending` (docs copy, `ordType` parameter).
    pub const ALL_PENDING: [OkxAlgoType; 8] = [
        OkxAlgoType::Conditional,
        OkxAlgoType::Oco,
        OkxAlgoType::Trigger,
        OkxAlgoType::Chase,
        OkxAlgoType::MoveOrderStop,
        OkxAlgoType::Iceberg,
        OkxAlgoType::Twap,
        OkxAlgoType::SmartIceberg,
    ];

    /// The reads that cover `ALL_PENDING`. The docs copy (`ordType` of `orders-algo-pending`)
    /// says `conditional` and `oco` may be joined with a comma in one request; every other
    /// type needs its own.
    pub const PENDING_READS: [&'static [OkxAlgoType]; 7] = [
        &[OkxAlgoType::Conditional, OkxAlgoType::Oco],
        &[OkxAlgoType::Trigger],
        &[OkxAlgoType::Chase],
        &[OkxAlgoType::MoveOrderStop],
        &[OkxAlgoType::Iceberg],
        &[OkxAlgoType::Twap],
        &[OkxAlgoType::SmartIceberg],
    ];

    /// The type named by an `ordType` value of a read response.
    pub(crate) fn from_pending_name(name: &str) -> Option<OkxAlgoType> {
        Self::ALL_PENDING
            .into_iter()
            .find(|t| t.pending_name() == name)
    }

    /// The `ordType` value in a read query.
    pub(crate) fn pending_name(self) -> &'static str {
        match self {
            OkxAlgoType::Conditional => "conditional",
            OkxAlgoType::Oco => "oco",
            OkxAlgoType::Trigger => "trigger",
            OkxAlgoType::Chase => "chase",
            OkxAlgoType::MoveOrderStop => "move_order_stop",
            OkxAlgoType::Iceberg => "iceberg",
            OkxAlgoType::Twap => "twap",
            OkxAlgoType::SmartIceberg => "smart_iceberg",
        }
    }

    /// Whether `cancel-algos` is the cancel endpoint for this type. The docs example excludes
    /// iceberg, TWAP and trailing stop; their cancel endpoint is not verified, so cancel-all
    /// does not try them and lists them as remaining instead.
    pub fn cancel_algos_supported(self) -> bool {
        matches!(
            self,
            OkxAlgoType::Conditional | OkxAlgoType::Oco | OkxAlgoType::Trigger | OkxAlgoType::Chase
        )
    }

    /// The type a given TP/SL combination must use.
    pub fn for_legs(has_tp: bool, has_sl: bool) -> Result<OkxAlgoType, TradeError> {
        match (has_tp, has_sl) {
            (true, true) => Ok(OkxAlgoType::Oco),
            (true, false) | (false, true) => Ok(OkxAlgoType::Conditional),
            (false, false) => Err(TradeError::OkxNoTrigger),
        }
    }

    fn okx(self) -> Result<&'static str, TradeError> {
        match self {
            OkxAlgoType::Conditional => Ok("conditional"),
            OkxAlgoType::Oco => Ok("oco"),
            _ => Err(TradeError::Unsupported),
        }
    }
}

struct Ctx {
    td_mode: &'static str,
    side: Side,
    pos_side: Option<&'static str>,
    /// Net-mode swap and closing.
    net_close: bool,
    /// Long_short swap.
    hedge: bool,
    swap: bool,
    dir: Dir,
}

fn ctx(rules: &SymbolRules, target: OkxTarget) -> Result<Ctx, TradeError> {
    match target {
        OkxTarget::Spot { side } => {
            rules.require_product(TradeProduct::OkxSpot)?;
            Ok(Ctx {
                td_mode: "cash",
                side,
                pos_side: None,
                net_close: false,
                hedge: false,
                swap: false,
                dir: Dir::Long, // spot holdings are long
            })
        }
        OkxTarget::Swap {
            mode,
            margin,
            action,
        } => {
            rules.require_product(TradeProduct::OkxSwap)?;
            let mode = mode.ok_or(TradeError::PositionModeUnknown)?;
            let hedge = mode == PositionMode::Hedge;
            Ok(Ctx {
                td_mode: margin.okx(),
                side: action.side(),
                pos_side: hedge.then(|| match action.dir() {
                    Dir::Long => "long",
                    Dir::Short => "short",
                }),
                net_close: !hedge && action.is_close(),
                hedge,
                swap: true,
                dir: action.dir(),
            })
        }
    }
}

/// Take-profit and stop-loss triggers attached to an entry (`attachAlgoOrds`). OKX creates
/// them only after the entry is **fully** filled, so a partial fill is unprotected.
#[derive(Debug)]
pub struct OkxAttach {
    pub tp_trigger: Option<Decimal>,
    pub sl_trigger: Option<Decimal>,
    pub trigger_type: TriggerPxType,
    /// Owned: one id belongs to one request (condition 61).
    pub client_id: ClientId,
}

#[derive(Debug)]
pub struct OkxEntry<'a> {
    pub rules: &'a SymbolRules,
    pub target: OkxTarget,
    /// Contracts for swaps, base-currency amount for spot.
    pub qty: Decimal,
    pub exec: Exec,
    /// Last price; needed to check attached triggers.
    pub ref_price: Option<Decimal>,
    pub client_id: ClientId,
    pub attach: Option<OkxAttach>,
}

#[cfg(test)]
impl OkxAttach {
    /// Tests build the same order several times; production code cannot (condition 61).
    pub(crate) fn duplicate(&self) -> OkxAttach {
        OkxAttach {
            tp_trigger: self.tp_trigger,
            sl_trigger: self.sl_trigger,
            trigger_type: self.trigger_type,
            client_id: ClientId::parse(self.client_id.as_str()).unwrap(),
        }
    }
}

#[cfg(test)]
impl OkxEntry<'_> {
    pub(crate) fn duplicate(&self) -> OkxEntry<'_> {
        OkxEntry {
            rules: self.rules,
            target: self.target,
            qty: self.qty,
            exec: self.exec,
            ref_price: self.ref_price,
            client_id: ClientId::parse(self.client_id.as_str()).unwrap(),
            attach: self.attach.as_ref().map(OkxAttach::duplicate),
        }
    }
}

/// `POST /api/v5/trade/order`.
pub fn place_order(i: OkxEntry<'_>) -> Result<OkxRequest, TradeError> {
    let c = ctx(i.rules, i.target)?;
    let cl_ord_id = i.client_id.check(IdField::Okx)?.to_string();
    let market = matches!(i.exec, Exec::Market);
    let qty = i.rules.round_qty(&i.qty, market)?;
    let limit_px = match i.exec {
        Exec::Limit { price, .. } => Some(i.rules.round_limit_price(c.side, &price)?),
        Exec::Market => None,
    };
    let (px, ord_type) = match i.exec {
        Exec::Limit { tif, .. } => (
            limit_px.as_ref().map(Decimal::to_plain_string),
            match tif {
                TimeInForce::Gtc => "limit",
                TimeInForce::Ioc => "ioc",
                TimeInForce::Fok => "fok",
            },
        ),
        Exec::Market => (None, "market"),
    };
    // Spot has a notional rule (market cap), so it needs the limit price or a reference.
    let order_value = check_order(
        i.rules,
        &qty,
        limit_px.as_ref().or(i.ref_price.as_ref()),
        market,
    )?;
    // A plain spot sell is not reduce-only (F4): it counts as adding. Only a swap close reduces.
    let reduces = matches!(
        i.target,
        OkxTarget::Swap {
            action: Action::Close(_),
            ..
        }
    );

    // Attached orders are relative to the main order. Only an entry (spot buy, swap open)
    // may carry them; on a sell or a close they would sit on the wrong side.
    if i.attach.is_some()
        && !matches!(
            i.target,
            OkxTarget::Spot { side: Side::Buy }
                | OkxTarget::Swap {
                    action: Action::Open(_),
                    ..
                }
        )
    {
        return Err(TradeError::AttachOnlyOnOpen);
    }
    // A limit entry fills at its own price, and the triggers sit relative to that fill: they are
    // checked against the limit price (rounded as it is sent), not against a market that may be
    // far away. A market entry fills near the last price.
    let attach_base = limit_px.or(i.ref_price);
    let attach_algo_ords = match &i.attach {
        Some(a) => vec![attach_algo(i.rules, &c, a, attach_base.as_ref())?],
        None => Vec::new(),
    };

    let request = OkxRequest::new(
        Method::Post,
        "/api/v5/trade/order".to_string(),
        Some(OkxBody::Order(OkxOrderBody {
            inst_id: i.rules.symbol().as_str().to_string(),
            td_mode: c.td_mode,
            side: c.side.okx(),
            pos_side: c.pos_side,
            ord_type,
            sz: qty.to_plain_string(),
            px,
            // reduceOnly only exists for net-mode swaps; long_short and spot never get it.
            reduce_only: c.net_close.then_some(true),
            // Spot market `sz` is in the quote currency by default for buys: say base.
            tgt_ccy: (!c.swap && market).then_some("base_ccy"),
            cl_ord_id,
            attach_algo_ords,
        })),
    );
    // The single-order cap is in USDT. A spot pair quoted in another currency cannot be valued
    // in USDT, so its value is unknown and the cap refuses it (item 9: the cap applies to
    // USDT-quoted pairs only; `maxMktAmt` is not used).
    let usdt_valued = c.swap || i.rules.symbol().as_str().ends_with("-USDT");
    Ok(if reduces {
        request.reducing()
    } else {
        request.adding(order_value.filter(|_| usdt_valued))
    })
}

fn attach_algo(
    rules: &SymbolRules,
    c: &Ctx,
    a: &OkxAttach,
    ref_price: Option<&Decimal>,
) -> Result<OkxAttachAlgo, TradeError> {
    let id = a.client_id.check(IdField::Okx)?.to_string();
    if a.tp_trigger.is_none() && a.sl_trigger.is_none() {
        return Err(TradeError::OkxNoTrigger);
    }
    // The attached orders protect the position being opened.
    let ref_price = ref_price.ok_or(TradeError::MissingReferencePrice)?;
    let tp = leg(rules, c.dir, false, a.tp_trigger, ref_price)?;
    let sl = leg(rules, c.dir, true, a.sl_trigger, ref_price)?;
    let ty = a.trigger_type.okx();
    Ok(OkxAttachAlgo {
        attach_algo_cl_ord_id: id,
        tp_ord_px: tp.as_ref().map(|_| "-1"),
        tp_trigger_px_type: tp.as_ref().map(|_| ty),
        tp_trigger_px: tp,
        sl_ord_px: sl.as_ref().map(|_| "-1"),
        sl_trigger_px_type: sl.as_ref().map(|_| ty),
        sl_trigger_px: sl,
    })
}

/// One TP or SL leg: trigger rounded to the nearest tick and checked against the market.
fn leg(
    rules: &SymbolRules,
    dir: Dir,
    is_stop: bool,
    trigger: Option<Decimal>,
    ref_price: &Decimal,
) -> Result<Option<String>, TradeError> {
    let Some(t) = trigger else {
        return Ok(None);
    };
    let t = rules.round_trigger_price(&t)?;
    let kind = if is_stop {
        super::types::ProtectiveKind::StopMarket
    } else {
        super::types::ProtectiveKind::TakeProfitMarket
    };
    check_trigger_side(kind, dir, &t, ref_price)?;
    Ok(Some(t.to_plain_string()))
}

/// A protective algo order for an existing position or spot holding.
#[derive(Debug)]
pub struct OkxProtective<'a> {
    pub rules: &'a SymbolRules,
    /// Spot: a sell. Swap: `Action::Close(dir)`.
    pub target: OkxTarget,
    pub ord_type: OkxAlgoType,
    pub size: Size,
    pub tp_trigger: Option<Decimal>,
    pub sl_trigger: Option<Decimal>,
    pub trigger_type: TriggerPxType,
    pub ref_price: Decimal,
    pub client_id: ClientId,
}

#[cfg(test)]
impl OkxProtective<'_> {
    pub(crate) fn duplicate(&self) -> OkxProtective<'_> {
        OkxProtective {
            rules: self.rules,
            target: self.target,
            ord_type: self.ord_type,
            size: self.size,
            tp_trigger: self.tp_trigger,
            sl_trigger: self.sl_trigger,
            trigger_type: self.trigger_type,
            ref_price: self.ref_price,
            client_id: ClientId::parse(self.client_id.as_str()).unwrap(),
        }
    }
}

/// `POST /api/v5/trade/order-algo`.
///
/// A `conditional` order with both TP and SL is an error (OKX would run only the SL), and
/// an `oco` needs both. Triggers execute at market (`-1`).
pub fn place_algo(i: OkxProtective<'_>) -> Result<OkxRequest, TradeError> {
    let c = ctx(i.rules, i.target)?;
    let ord_type = i.ord_type.okx()?;
    let algo_cl_ord_id = i.client_id.check(IdField::Okx)?.to_string();

    match (i.ord_type, i.tp_trigger.is_some(), i.sl_trigger.is_some()) {
        (OkxAlgoType::Conditional, true, true) => {
            return Err(TradeError::OkxConditionalWithBothLegs);
        }
        (OkxAlgoType::Conditional, false, false) => return Err(TradeError::OkxNoTrigger),
        (OkxAlgoType::Oco, tp, sl) if !(tp && sl) => return Err(TradeError::OkxOcoNeedsBothLegs),
        _ => {}
    }
    // Protection closes a position: spot sells, swaps close.
    match i.target {
        OkxTarget::Spot { side: Side::Buy } => return Err(TradeError::Unsupported),
        OkxTarget::Swap { action, .. } if !action.is_close() => {
            return Err(TradeError::Unsupported);
        }
        _ => {}
    }

    let (sz, close_fraction) = match i.size {
        // Hedge accounts only get full-close orders (user decision).
        Size::Qty(_) if c.hedge => return Err(TradeError::HedgeProtectiveMustClosePosition),
        Size::Qty(q) => {
            let q = i.rules.round_qty(&q, true)?;
            super::checks::check_qty(i.rules, &q, true)?;
            (Some(q.to_plain_string()), None)
        }
        // `closeFraction` exists for futures/swaps only, and only accepts 1.
        Size::ClosePosition if c.swap => (None, Some("1")),
        Size::ClosePosition => return Err(TradeError::ClosePositionNotAllowed),
    };

    let tp = leg(i.rules, c.dir, false, i.tp_trigger, &i.ref_price)?;
    let sl = leg(i.rules, c.dir, true, i.sl_trigger, &i.ref_price)?;
    let ty = i.trigger_type.okx();

    // Protective orders only ever close (condition 64, 71).
    Ok(OkxRequest::new(
        Method::Post,
        "/api/v5/trade/order-algo".to_string(),
        Some(OkxBody::Algo(OkxAlgoBody {
            inst_id: i.rules.symbol().as_str().to_string(),
            td_mode: c.td_mode,
            side: c.side.okx(),
            pos_side: c.pos_side,
            ord_type,
            sz,
            close_fraction,
            // Net-mode swaps need reduceOnly (and closeFraction requires it); long_short
            // ignores it and spot has none.
            reduce_only: c.net_close.then_some(true),
            tp_ord_px: tp.as_ref().map(|_| "-1"),
            tp_trigger_px_type: tp.as_ref().map(|_| ty),
            tp_trigger_px: tp,
            sl_ord_px: sl.as_ref().map(|_| "-1"),
            sl_trigger_px_type: sl.as_ref().map(|_| ty),
            sl_trigger_px: sl,
            algo_cl_ord_id,
        })),
    )
    .reducing())
}

/// `POST /api/v5/trade/close-position`: close a whole swap position at market (condition
/// 70). Checked against the official docs copy: `instId`, `mgnMode`, `posSide` (required in
/// long/short mode, only `net` or omitted in net mode), `clOrdId`.
pub fn close_position(
    rules: &SymbolRules,
    mode: Option<PositionMode>,
    margin: MarginMode,
    dir: Dir,
    client_id: ClientId,
) -> Result<OkxRequest, TradeError> {
    rules.require_product(TradeProduct::OkxSwap)?;
    let mode = mode.ok_or(TradeError::PositionModeUnknown)?;
    let cl_ord_id = client_id.check(IdField::Okx)?.to_string();
    Ok(OkxRequest::new(
        Method::Post,
        "/api/v5/trade/close-position".to_string(),
        Some(OkxBody::ClosePosition(
            super::request::OkxClosePositionBody {
                inst_id: rules.symbol().as_str().to_string(),
                pos_side: (mode == PositionMode::Hedge).then_some(match dir {
                    Dir::Long => "long",
                    Dir::Short => "short",
                }),
                mgn_mode: margin.okx(),
                cl_ord_id: Some(cl_ord_id),
            },
        )),
    )
    .reducing())
}

fn ref_fields(order: &OrderRef) -> Result<(Option<String>, Option<String>), TradeError> {
    match order {
        OrderRef::Exchange(id) => {
            OrderRef::exchange(id)?;
            Ok((Some(id.clone()), None))
        }
        OrderRef::Client(id) => Ok((None, Some(id.check(IdField::Okx)?.to_string()))),
    }
}

/// `POST /api/v5/trade/cancel-order`.
pub fn cancel_order(rules: &SymbolRules, order: &OrderRef) -> Result<OkxRequest, TradeError> {
    require_okx(rules)?;
    let (ord_id, cl_ord_id) = ref_fields(order)?;
    Ok(OkxRequest::new(
        Method::Post,
        "/api/v5/trade/cancel-order".to_string(),
        Some(OkxBody::CancelOrder(OkxCancelOrderBody {
            inst_id: rules.symbol().as_str().to_string(),
            ord_id,
            cl_ord_id,
        })),
    ))
}

/// `GET /api/v5/trade/order?instId=..&ordId=..` (or `clOrdId`). The query is part of the
/// signed path and there is no body.
pub fn query_order(rules: &SymbolRules, order: &OrderRef) -> Result<OkxRequest, TradeError> {
    require_okx(rules)?;
    let (ord_id, cl_ord_id) = ref_fields(order)?;
    let (name, value) = match (ord_id, cl_ord_id) {
        (Some(v), _) => ("ordId", v),
        (_, Some(v)) => ("clOrdId", v),
        _ => return Err(TradeError::OrderIdInvalid),
    };
    Ok(OkxRequest::new(
        Method::Get,
        format!(
            "/api/v5/trade/order?instId={}&{name}={value}",
            rules.symbol().as_str()
        ),
        None,
    ))
}

/// Most algo orders `cancel-algos` takes per request.
pub const MAX_CANCEL_ALGOS: usize = 10;

/// `POST /api/v5/trade/cancel-algos` (an array of up to 10). Not for trailing stops, iceberg
/// or TWAP orders: their cancel endpoint is not verified, so those types are refused.
pub fn cancel_algos(
    rules: &SymbolRules,
    algo_type: OkxAlgoType,
    orders: &[OrderRef],
) -> Result<OkxRequest, TradeError> {
    require_okx(rules)?;
    if !algo_type.cancel_algos_supported() {
        return Err(TradeError::Unsupported);
    }
    if orders.is_empty() || orders.len() > MAX_CANCEL_ALGOS {
        return Err(TradeError::Unsupported);
    }
    let items = orders
        .iter()
        .map(|o| {
            let (algo_id, algo_cl_ord_id) = ref_fields(o)?;
            Ok(OkxCancelAlgo {
                inst_id: rules.symbol().as_str().to_string(),
                algo_id,
                algo_cl_ord_id,
            })
        })
        .collect::<Result<Vec<_>, TradeError>>()?;
    Ok(OkxRequest::new(
        Method::Post,
        "/api/v5/trade/cancel-algos".to_string(),
        Some(OkxBody::CancelAlgos(items)),
    ))
}

/// `GET /api/v5/trade/order-algo?algoId=..` (or `algoClOrdId`).
pub fn query_algo(algo_type: OkxAlgoType, order: &OrderRef) -> Result<OkxRequest, TradeError> {
    algo_type.okx()?;
    let (algo_id, algo_cl_ord_id) = ref_fields(order)?;
    let (name, value) = match (algo_id, algo_cl_ord_id) {
        (Some(v), _) => ("algoId", v),
        (_, Some(v)) => ("algoClOrdId", v),
        _ => return Err(TradeError::OrderIdInvalid),
    };
    Ok(OkxRequest::new(
        Method::Get,
        format!("/api/v5/trade/order-algo?{name}={value}"),
        None,
    ))
}

fn require_okx(rules: &SymbolRules) -> Result<(), TradeError> {
    match rules.product() {
        TradeProduct::OkxSpot | TradeProduct::OkxSwap => Ok(()),
        _ => Err(TradeError::ProductMismatch),
    }
}

#[cfg(test)]
mod tests {
    use super::super::rules::fixtures::*;
    use super::*;

    const SECRET: &str = "test-secret-not-real";
    const TS: &str = "2023-11-14T22:13:20.000Z";

    fn id(s: &str) -> ClientId {
        ClientId::parse(s).unwrap()
    }

    // The builders consume their input (one id, one request). These wrappers let a test build
    // the same input again, with a copy of the id.
    fn dup(c: &ClientId) -> ClientId {
        id(c.as_str())
    }

    fn place_order(i: &OkxEntry<'_>) -> Result<OkxRequest, TradeError> {
        super::place_order(i.duplicate())
    }

    fn place_algo(i: &OkxProtective<'_>) -> Result<OkxRequest, TradeError> {
        super::place_algo(i.duplicate())
    }

    fn close_position(
        rules: &SymbolRules,
        mode: Option<PositionMode>,
        margin: MarginMode,
        dir: Dir,
        cid: &ClientId,
    ) -> Result<OkxRequest, TradeError> {
        super::close_position(rules, mode, margin, dir, dup(cid))
    }

    /// The signed body, which the golden strings are compared to. Also checks that the
    /// signature covers exactly that body.
    fn body(r: &OkxRequest) -> String {
        let s = r.signed(SECRET, TS).unwrap();
        let expected = crate::account::okx_sign(SECRET, TS, r.method().as_str(), r.path(), &s.body);
        assert_eq!(s.sign, expected);
        s.body
    }

    fn net() -> Option<PositionMode> {
        Some(PositionMode::OneWay)
    }

    fn long_short() -> Option<PositionMode> {
        Some(PositionMode::Hedge)
    }

    fn swap(mode: Option<PositionMode>, action: Action) -> OkxTarget {
        OkxTarget::Swap {
            mode,
            margin: MarginMode::Cross,
            action,
        }
    }

    fn entry<'a>(
        rules: &'a SymbolRules,
        target: OkxTarget,
        exec: Exec,
        cid: &'a ClientId,
    ) -> OkxEntry<'a> {
        OkxEntry {
            rules,
            target,
            qty: d("3"),
            exec,
            ref_price: Some(d("60000")),
            client_id: dup(cid),
            attach: None,
        }
    }

    fn algo<'a>(
        rules: &'a SymbolRules,
        target: OkxTarget,
        ord_type: OkxAlgoType,
        size: Size,
        tp: Option<&str>,
        sl: Option<&str>,
        cid: &'a ClientId,
    ) -> OkxProtective<'a> {
        OkxProtective {
            rules,
            target,
            ord_type,
            size,
            tp_trigger: tp.map(d),
            sl_trigger: sl.map(d),
            trigger_type: TriggerPxType::Mark,
            ref_price: d("60000"),
            client_id: dup(cid),
        }
    }

    const LIMIT_GTC: fn(&str) -> Exec = |p| Exec::Limit {
        price: d(p),
        tif: TimeInForce::Gtc,
    };

    // ── net (one-way) swap ───────────────────────────────────────────────

    #[test]
    fn net_entry_market_limit_and_close() {
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        let cid = id("eAAAA");
        let open = entry(r, swap(net(), Action::Open(Dir::Long)), Exec::Market, &cid);
        assert_eq!(
            body(&place_order(&open).unwrap()),
            r#"{"instId":"BTC-USDT-SWAP","tdMode":"cross","side":"buy","ordType":"market","sz":"3","clOrdId":"eAAAA"}"#
        );
        let lim = entry(
            r,
            swap(net(), Action::Open(Dir::Short)),
            LIMIT_GTC("60000.04"),
            &cid,
        );
        // sell limit rounds up to the 0.1 tick
        assert_eq!(
            body(&place_order(&lim).unwrap()),
            r#"{"instId":"BTC-USDT-SWAP","tdMode":"cross","side":"sell","ordType":"limit","sz":"3","px":"60000.1","clOrdId":"eAAAA"}"#
        );
        let close = entry(r, swap(net(), Action::Close(Dir::Long)), Exec::Market, &cid);
        assert_eq!(
            body(&place_order(&close).unwrap()),
            r#"{"instId":"BTC-USDT-SWAP","tdMode":"cross","side":"sell","ordType":"market","sz":"3","reduceOnly":true,"clOrdId":"eAAAA"}"#
        );
    }

    #[test]
    fn net_entry_with_attached_tp_and_sl() {
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        let (cid, aid) = (id("eAAAA"), id("aBBBB"));
        let mut e = entry(r, swap(net(), Action::Open(Dir::Long)), Exec::Market, &cid);
        e.attach = Some(OkxAttach {
            tp_trigger: Some(d("61000.04")),
            sl_trigger: Some(d("59000")),
            trigger_type: TriggerPxType::Mark,
            client_id: dup(&aid),
        });
        assert_eq!(
            body(&place_order(&e).unwrap()),
            r#"{"instId":"BTC-USDT-SWAP","tdMode":"cross","side":"buy","ordType":"market","sz":"3","clOrdId":"eAAAA","attachAlgoOrds":[{"attachAlgoClOrdId":"aBBBB","tpTriggerPx":"61000","tpOrdPx":"-1","tpTriggerPxType":"mark","slTriggerPx":"59000","slOrdPx":"-1","slTriggerPxType":"mark"}]}"#
        );
        // wrong side for a long: TP below the market
        e.attach = Some(OkxAttach {
            tp_trigger: Some(d("59000")),
            sl_trigger: None,
            trigger_type: TriggerPxType::Last,
            client_id: dup(&aid),
        });
        assert_eq!(place_order(&e).unwrap_err(), TradeError::TriggerOnWrongSide);
    }

    #[test]
    fn a_limit_entry_checks_its_attached_triggers_against_the_limit_price() {
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        let (cid, aid) = (id("eAAAA"), id("aBBBB"));
        // A buy limit far below the market (60000): the take-profit at 51000 is above the
        // limit, which is what matters, although it is below the market.
        let mut e = entry(
            r,
            swap(net(), Action::Open(Dir::Long)),
            LIMIT_GTC("50000"),
            &cid,
        );
        e.attach = Some(OkxAttach {
            tp_trigger: Some(d("51000")),
            sl_trigger: Some(d("49000")),
            trigger_type: TriggerPxType::Mark,
            client_id: dup(&aid),
        });
        assert!(place_order(&e).is_ok());
        // Wrong side of the limit price: refused, whatever the market is.
        e.attach = Some(OkxAttach {
            tp_trigger: Some(d("49500")),
            sl_trigger: None,
            trigger_type: TriggerPxType::Mark,
            client_id: dup(&aid),
        });
        assert_eq!(place_order(&e).unwrap_err(), TradeError::TriggerOnWrongSide);
        // No market price is needed for a limit entry; a market entry still needs one.
        e.ref_price = None;
        e.attach = Some(OkxAttach {
            tp_trigger: Some(d("51000")),
            sl_trigger: None,
            trigger_type: TriggerPxType::Mark,
            client_id: dup(&aid),
        });
        assert!(place_order(&e).is_ok());
    }

    #[test]
    fn net_protective_stop_take_profit_and_oco() {
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        let cid = id("sCCCC");
        let tgt = swap(net(), Action::Close(Dir::Long));
        let stop = algo(
            r,
            tgt,
            OkxAlgoType::Conditional,
            Size::Qty(d("3")),
            None,
            Some("59000.04"),
            &cid,
        );
        assert_eq!(
            body(&place_algo(&stop).unwrap()),
            r#"{"instId":"BTC-USDT-SWAP","tdMode":"cross","side":"sell","ordType":"conditional","sz":"3","reduceOnly":true,"slTriggerPx":"59000","slOrdPx":"-1","slTriggerPxType":"mark","algoClOrdId":"sCCCC"}"#
        );
        let tp = algo(
            r,
            tgt,
            OkxAlgoType::Conditional,
            Size::Qty(d("3")),
            Some("61000"),
            None,
            &cid,
        );
        assert_eq!(
            body(&place_algo(&tp).unwrap()),
            r#"{"instId":"BTC-USDT-SWAP","tdMode":"cross","side":"sell","ordType":"conditional","sz":"3","reduceOnly":true,"tpTriggerPx":"61000","tpOrdPx":"-1","tpTriggerPxType":"mark","algoClOrdId":"sCCCC"}"#
        );
        let oco = algo(
            r,
            tgt,
            OkxAlgoType::Oco,
            Size::Qty(d("3")),
            Some("61000"),
            Some("59000"),
            &cid,
        );
        assert_eq!(
            body(&place_algo(&oco).unwrap()),
            r#"{"instId":"BTC-USDT-SWAP","tdMode":"cross","side":"sell","ordType":"oco","sz":"3","reduceOnly":true,"tpTriggerPx":"61000","tpOrdPx":"-1","tpTriggerPxType":"mark","slTriggerPx":"59000","slOrdPx":"-1","slTriggerPxType":"mark","algoClOrdId":"sCCCC"}"#
        );
        // full close: closeFraction instead of sz, reduceOnly still required in net mode
        let full = algo(
            r,
            tgt,
            OkxAlgoType::Conditional,
            Size::ClosePosition,
            None,
            Some("59000"),
            &cid,
        );
        assert_eq!(
            body(&place_algo(&full).unwrap()),
            r#"{"instId":"BTC-USDT-SWAP","tdMode":"cross","side":"sell","ordType":"conditional","closeFraction":"1","reduceOnly":true,"slTriggerPx":"59000","slOrdPx":"-1","slTriggerPxType":"mark","algoClOrdId":"sCCCC"}"#
        );
    }

    #[test]
    fn conditional_with_both_legs_is_an_error() {
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        let cid = id("sCCCC");
        let tgt = swap(net(), Action::Close(Dir::Long));
        let both = algo(
            r,
            tgt,
            OkxAlgoType::Conditional,
            Size::Qty(d("3")),
            Some("61000"),
            Some("59000"),
            &cid,
        );
        assert_eq!(
            place_algo(&both).unwrap_err(),
            TradeError::OkxConditionalWithBothLegs
        );
        let half = algo(
            r,
            tgt,
            OkxAlgoType::Oco,
            Size::Qty(d("3")),
            Some("61000"),
            None,
            &cid,
        );
        assert_eq!(
            place_algo(&half).unwrap_err(),
            TradeError::OkxOcoNeedsBothLegs
        );
        let none = algo(
            r,
            tgt,
            OkxAlgoType::Conditional,
            Size::Qty(d("3")),
            None,
            None,
            &cid,
        );
        assert_eq!(place_algo(&none).unwrap_err(), TradeError::OkxNoTrigger);
        assert_eq!(OkxAlgoType::for_legs(true, true), Ok(OkxAlgoType::Oco));
        assert_eq!(
            OkxAlgoType::for_legs(false, true),
            Ok(OkxAlgoType::Conditional)
        );
        assert_eq!(
            OkxAlgoType::for_legs(false, false),
            Err(TradeError::OkxNoTrigger)
        );
    }

    #[test]
    fn trailing_stop_is_refused_everywhere() {
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        let cid = id("rDDDD");
        let tgt = swap(net(), Action::Close(Dir::Long));
        let trail = algo(
            r,
            tgt,
            OkxAlgoType::MoveOrderStop,
            Size::Qty(d("3")),
            None,
            Some("59000"),
            &cid,
        );
        assert_eq!(place_algo(&trail).unwrap_err(), TradeError::Unsupported);
        let by_id = OrderRef::exchange("99").unwrap();
        assert_eq!(
            cancel_algos(r, OkxAlgoType::MoveOrderStop, std::slice::from_ref(&by_id)).unwrap_err(),
            TradeError::Unsupported
        );
        assert_eq!(
            query_algo(OkxAlgoType::MoveOrderStop, &by_id).unwrap_err(),
            TradeError::Unsupported
        );
    }

    #[test]
    fn net_cancel_and_query() {
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        let by_id = OrderRef::exchange("680800019749904384").unwrap();
        let by_client = OrderRef::Client(id("eAAAA"));
        assert_eq!(
            body(&cancel_order(r, &by_id).unwrap()),
            r#"{"instId":"BTC-USDT-SWAP","ordId":"680800019749904384"}"#
        );
        assert_eq!(
            body(&cancel_order(r, &by_client).unwrap()),
            r#"{"instId":"BTC-USDT-SWAP","clOrdId":"eAAAA"}"#
        );
        assert_eq!(
            body(
                &cancel_algos(
                    r,
                    OkxAlgoType::Oco,
                    &[
                        OrderRef::exchange("680800019749904384").unwrap(),
                        OrderRef::Client(id("eAAAA"))
                    ]
                )
                .unwrap()
            ),
            r#"[{"instId":"BTC-USDT-SWAP","algoId":"680800019749904384"},{"instId":"BTC-USDT-SWAP","algoClOrdId":"eAAAA"}]"#
        );
        let q = query_order(r, &by_client).unwrap();
        assert_eq!(
            q.path(),
            "/api/v5/trade/order?instId=BTC-USDT-SWAP&clOrdId=eAAAA"
        );
        // GET: the signed content is the path with its query and an empty body
        assert_eq!(body(&q), "");
        assert_eq!(
            query_order(r, &by_id).unwrap().path(),
            "/api/v5/trade/order?instId=BTC-USDT-SWAP&ordId=680800019749904384"
        );
        assert_eq!(
            query_algo(OkxAlgoType::Conditional, &by_client)
                .unwrap()
                .path(),
            "/api/v5/trade/order-algo?algoClOrdId=eAAAA"
        );
        assert_eq!(
            query_algo(OkxAlgoType::Oco, &by_id).unwrap().path(),
            "/api/v5/trade/order-algo?algoId=680800019749904384"
        );
        assert!(cancel_algos(r, OkxAlgoType::Oco, &[]).is_err());
        assert!(
            cancel_algos(
                r,
                OkxAlgoType::Oco,
                &(0..11)
                    .map(|_| OrderRef::exchange("680800019749904384").unwrap())
                    .collect::<Vec<_>>()
            )
            .is_err()
        );
    }

    // ── long_short swap ─────────────────────────────────────────────────

    #[test]
    fn long_short_entry_uses_pos_side_and_never_reduce_only() {
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        let cid = id("eAAAA");
        let open = entry(
            r,
            swap(long_short(), Action::Open(Dir::Long)),
            Exec::Market,
            &cid,
        );
        assert_eq!(
            body(&place_order(&open).unwrap()),
            r#"{"instId":"BTC-USDT-SWAP","tdMode":"cross","side":"buy","posSide":"long","ordType":"market","sz":"3","clOrdId":"eAAAA"}"#
        );
        // close long = sell + long, close short = buy + short, no reduceOnly
        let cl = entry(
            r,
            swap(long_short(), Action::Close(Dir::Long)),
            Exec::Market,
            &cid,
        );
        let b = body(&place_order(&cl).unwrap());
        assert!(b.contains(r#""side":"sell","posSide":"long""#) && !b.contains("reduceOnly"));
        let cs = entry(
            r,
            swap(long_short(), Action::Close(Dir::Short)),
            Exec::Market,
            &cid,
        );
        let b = body(&place_order(&cs).unwrap());
        assert!(b.contains(r#""side":"buy","posSide":"short""#) && !b.contains("reduceOnly"));
    }

    #[test]
    fn long_short_protective_is_full_close_only() {
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        let cid = id("sCCCC");
        let tgt = swap(long_short(), Action::Close(Dir::Long));
        let stop = algo(
            r,
            tgt,
            OkxAlgoType::Conditional,
            Size::ClosePosition,
            None,
            Some("59000"),
            &cid,
        );
        assert_eq!(
            body(&place_algo(&stop).unwrap()),
            r#"{"instId":"BTC-USDT-SWAP","tdMode":"cross","side":"sell","posSide":"long","ordType":"conditional","closeFraction":"1","slTriggerPx":"59000","slOrdPx":"-1","slTriggerPxType":"mark","algoClOrdId":"sCCCC"}"#
        );
        let oco = algo(
            r,
            tgt,
            OkxAlgoType::Oco,
            Size::ClosePosition,
            Some("61000"),
            Some("59000"),
            &cid,
        );
        assert!(
            body(&place_algo(&oco).unwrap()).contains(r#""ordType":"oco","closeFraction":"1""#)
        );
        let qty = algo(
            r,
            tgt,
            OkxAlgoType::Conditional,
            Size::Qty(d("3")),
            None,
            Some("59000"),
            &cid,
        );
        assert_eq!(
            place_algo(&qty).unwrap_err(),
            TradeError::HedgeProtectiveMustClosePosition
        );
        // short position: closing side is buy, trigger above the market
        let short = algo(
            r,
            swap(long_short(), Action::Close(Dir::Short)),
            OkxAlgoType::Conditional,
            Size::ClosePosition,
            None,
            Some("61000"),
            &cid,
        );
        assert!(body(&place_algo(&short).unwrap()).contains(r#""side":"buy","posSide":"short""#));
    }

    #[test]
    fn unknown_position_mode_refuses_swap_orders() {
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        let cid = id("eAAAA");
        let e = entry(r, swap(None, Action::Open(Dir::Long)), Exec::Market, &cid);
        assert_eq!(
            place_order(&e).unwrap_err(),
            TradeError::PositionModeUnknown
        );
        let a = algo(
            r,
            swap(None, Action::Close(Dir::Long)),
            OkxAlgoType::Conditional,
            Size::ClosePosition,
            None,
            Some("59000"),
            &cid,
        );
        assert_eq!(place_algo(&a).unwrap_err(), TradeError::PositionModeUnknown);
    }

    // ── spot ────────────────────────────────────────────────────────────

    #[test]
    fn spot_entry_protective_cancel_and_query() {
        let t = okx_spot_table();
        let r = t.lookup("BTC-USDT").unwrap();
        let cid = id("eAAAA");
        let mut e = entry(r, OkxTarget::Spot { side: Side::Buy }, Exec::Market, &cid);
        e.qty = d("0.001234567");
        // market buy `sz` defaults to the quote currency: tgtCcy must be explicit
        assert_eq!(
            body(&place_order(&e).unwrap()),
            r#"{"instId":"BTC-USDT","tdMode":"cash","side":"buy","ordType":"market","sz":"0.00123456","tgtCcy":"base_ccy","clOrdId":"eAAAA"}"#
        );
        let mut l = entry(
            r,
            OkxTarget::Spot { side: Side::Buy },
            LIMIT_GTC("59999.99"),
            &cid,
        );
        l.qty = d("0.5");
        assert_eq!(
            body(&place_order(&l).unwrap()),
            r#"{"instId":"BTC-USDT","tdMode":"cash","side":"buy","ordType":"limit","sz":"0.5","px":"59999.9","clOrdId":"eAAAA"}"#
        );

        let sc = id("sCCCC");
        let tgt = OkxTarget::Spot { side: Side::Sell };
        let stop = algo(
            r,
            tgt,
            OkxAlgoType::Conditional,
            Size::Qty(d("0.5")),
            None,
            Some("59000"),
            &sc,
        );
        assert_eq!(
            body(&place_algo(&stop).unwrap()),
            r#"{"instId":"BTC-USDT","tdMode":"cash","side":"sell","ordType":"conditional","sz":"0.5","slTriggerPx":"59000","slOrdPx":"-1","slTriggerPxType":"mark","algoClOrdId":"sCCCC"}"#
        );
        let oco = algo(
            r,
            tgt,
            OkxAlgoType::Oco,
            Size::Qty(d("0.5")),
            Some("61000"),
            Some("59000"),
            &sc,
        );
        assert!(body(&place_algo(&oco).unwrap()).contains(r#""ordType":"oco""#));
        // spot has no closeFraction and no short
        let full = algo(
            r,
            tgt,
            OkxAlgoType::Conditional,
            Size::ClosePosition,
            None,
            Some("59000"),
            &sc,
        );
        assert_eq!(
            place_algo(&full).unwrap_err(),
            TradeError::ClosePositionNotAllowed
        );
        let buy_stop = algo(
            r,
            OkxTarget::Spot { side: Side::Buy },
            OkxAlgoType::Conditional,
            Size::Qty(d("0.5")),
            None,
            Some("59000"),
            &sc,
        );
        assert_eq!(place_algo(&buy_stop).unwrap_err(), TradeError::Unsupported);

        let by_client = OrderRef::Client(id("eAAAA"));
        assert_eq!(
            body(&cancel_order(r, &by_client).unwrap()),
            r#"{"instId":"BTC-USDT","clOrdId":"eAAAA"}"#
        );
        assert_eq!(
            query_order(r, &by_client).unwrap().path(),
            "/api/v5/trade/order?instId=BTC-USDT&clOrdId=eAAAA"
        );
    }

    // ── rejections ──────────────────────────────────────────────────────

    #[test]
    fn limits_and_products_are_enforced() {
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        let cid = id("eAAAA");
        let mut e = entry(r, swap(net(), Action::Open(Dir::Long)), Exec::Market, &cid);
        e.qty = d("0.5"); // rounds to 0 with lotSz 1
        assert_eq!(place_order(&e).unwrap_err(), TradeError::NonPositive);
        e.qty = d("20001"); // above maxMktSz
        assert_eq!(place_order(&e).unwrap_err(), TradeError::QtyAboveMax);
        // a spot target with swap rules
        let e = entry(r, OkxTarget::Spot { side: Side::Buy }, Exec::Market, &cid);
        assert_eq!(place_order(&e).unwrap_err(), TradeError::ProductMismatch);
        // rules of a Binance product
        let bt = spot();
        let br = bt.lookup("BTCUSDT").unwrap();
        assert_eq!(
            cancel_order(br, &OrderRef::exchange("1").unwrap()).unwrap_err(),
            TradeError::ProductMismatch
        );
    }

    #[test]
    fn request_debug_hides_ids_and_values() {
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        let q = query_order(r, &OrderRef::Client(id("eSECRETID"))).unwrap();
        assert!(!format!("{q:?}").contains("eSECRETID"));
    }

    // ── attached TP/SL only on entries (B1) ──────────────────────────────

    fn attach(aid: &ClientId, tp: &str) -> Option<OkxAttach> {
        Some(OkxAttach {
            tp_trigger: Some(d(tp)),
            sl_trigger: None,
            trigger_type: TriggerPxType::Last,
            client_id: dup(aid),
        })
    }

    #[test]
    fn attached_orders_are_refused_on_spot_sell_and_swap_close() {
        let (cid, aid) = (id("eAAAA"), id("aBBBB"));

        // spot sell: the attached order would be a buy
        let spot = okx_spot_table();
        let sr = spot.lookup("BTC-USDT").unwrap();
        let mut e = entry(
            sr,
            OkxTarget::Spot { side: Side::Sell },
            LIMIT_GTC("110"),
            &cid,
        );
        e.ref_price = Some(d("100"));
        e.attach = attach(&aid, "105");
        assert_eq!(place_order(&e).unwrap_err(), TradeError::AttachOnlyOnOpen);

        // swap close, net and long_short, both directions
        let swap_t = okx_swap_table();
        let r = swap_t.lookup("BTC-USDT-SWAP").unwrap();
        for mode in [net(), long_short()] {
            for dir in [Dir::Long, Dir::Short] {
                let mut e = entry(r, swap(mode, Action::Close(dir)), LIMIT_GTC("60100"), &cid);
                e.attach = attach(&aid, "60500");
                assert_eq!(place_order(&e).unwrap_err(), TradeError::AttachOnlyOnOpen);
                e.attach = None;
                assert!(place_order(&e).is_ok());
            }
        }
    }

    #[test]
    fn attached_orders_are_allowed_on_spot_buy_and_swap_open() {
        let (cid, aid) = (id("eAAAA"), id("aBBBB"));
        let spot = okx_spot_table();
        let sr = spot.lookup("BTC-USDT").unwrap();
        let mut e = entry(
            sr,
            OkxTarget::Spot { side: Side::Buy },
            LIMIT_GTC("59000"),
            &cid,
        );
        e.attach = attach(&aid, "61000");
        assert!(place_order(&e).is_ok());

        let swap_t = okx_swap_table();
        let r = swap_t.lookup("BTC-USDT-SWAP").unwrap();
        for mode in [net(), long_short()] {
            let mut e = entry(
                r,
                swap(mode, Action::Open(Dir::Long)),
                LIMIT_GTC("59000"),
                &cid,
            );
            e.attach = attach(&aid, "61000");
            assert!(place_order(&e).is_ok());
        }
    }

    // ── spot maxMktSz is a USDT notional (F1) ────────────────────────────

    #[test]
    fn spot_market_cap_is_a_notional_not_a_base_quantity() {
        let t = okx_spot_table();
        let r = t.lookup("BTC-USDT").unwrap();
        let cid = id("eAAAA");
        let buy = OkxTarget::Spot { side: Side::Buy };
        let mut e = entry(r, buy, Exec::Market, &cid);

        // 20 BTC is far below 1,000,000 as a quantity, but 1.2M USDT is above the cap.
        e.qty = d("20");
        e.ref_price = Some(d("60000"));
        assert_eq!(place_order(&e).unwrap_err(), TradeError::NotionalAboveMax);
        // exactly at the cap passes
        e.qty = d("10");
        e.ref_price = Some(d("100000"));
        assert!(place_order(&e).is_ok());
        // a low priced coin: 2,000,000 units is only 200,000 USDT
        e.qty = d("2000000");
        e.ref_price = Some(d("0.1"));
        assert!(place_order(&e).is_ok());
        // no reference price: cannot check, refuse
        e.ref_price = None;
        assert_eq!(
            place_order(&e).unwrap_err(),
            TradeError::MissingReferencePrice
        );
        // the cap does not apply to limit orders (their cap is maxLmtSz in base units)
        e.exec = LIMIT_GTC("60000");
        e.qty = d("20");
        assert!(place_order(&e).is_ok());
    }

    #[test]
    fn swap_market_cap_still_counts_contracts() {
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        let cid = id("eAAAA");
        let mut e = entry(r, swap(net(), Action::Open(Dir::Long)), Exec::Market, &cid);
        e.qty = d("20001");
        assert_eq!(place_order(&e).unwrap_err(), TradeError::QtyAboveMax);
        e.qty = d("20000");
        assert!(place_order(&e).is_ok());
    }

    #[test]
    fn close_position_body_and_mode() {
        let t = okx_swap_table();
        let r = t.lookup("BTC-USDT-SWAP").unwrap();
        let cid = id("eAAAA");
        let net_req = close_position(r, net(), MarginMode::Cross, Dir::Long, &cid).unwrap();
        assert_eq!(net_req.path(), "/api/v5/trade/close-position");
        assert_eq!(net_req.exposure(), super::super::request::Exposure::Reduces);
        let v: serde_json::Value = serde_json::from_str(&body(&net_req)).unwrap();
        assert_eq!(v["instId"], "BTC-USDT-SWAP");
        assert_eq!(v["mgnMode"], "cross");
        assert_eq!(v["clOrdId"], "eAAAA");
        assert!(v.get("posSide").is_none(), "net mode sends no posSide");
        let hedge =
            close_position(r, long_short(), MarginMode::Isolated, Dir::Short, &cid).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body(&hedge)).unwrap();
        assert_eq!(v["posSide"], "short");
        assert_eq!(v["mgnMode"], "isolated");
        // the position mode must be known, and the product must be a swap
        assert!(close_position(r, None, MarginMode::Cross, Dir::Long, &cid).is_err());
        let spot_t = okx_spot_table();
        assert!(
            close_position(
                spot_t.lookup("BTC-USDT").unwrap(),
                net(),
                MarginMode::Cross,
                Dir::Long,
                &cid
            )
            .is_err()
        );
    }

    #[test]
    fn spot_pairs_not_quoted_in_usdt_have_no_cap_value() {
        use super::super::request::Exposure;
        let resp = serde_json::json!({"code": "0", "data": [
            {"instType": "SPOT", "instId": "BTC-EUR", "ctVal": "", "ctType": "",
             "lotSz": "0.00000001", "minSz": "0.00001", "maxLmtSz": "9999999999", "maxMktSz": "1000000",
             "tickSz": "0.1", "state": "live"}
        ]});
        let t = super::super::rules::RulesTable::from_okx_instruments(
            TradeProduct::OkxSpot,
            &resp,
            1_000,
        )
        .unwrap();
        let cid = id("eAAAA");
        let mk = |rules| {
            place_order(&OkxEntry {
                rules,
                target: OkxTarget::Spot { side: Side::Buy },
                qty: d("0.1"),
                exec: Exec::Limit {
                    price: d("60000"),
                    tif: TimeInForce::Gtc,
                },
                ref_price: Some(d("60000")),
                client_id: dup(&cid),
                attach: None,
            })
            .unwrap()
            .exposure()
        };
        assert_eq!(
            mk(t.lookup("BTC-EUR").unwrap()),
            Exposure::Adds { notional: None }
        );
        let usdt = okx_spot_table();
        assert!(matches!(
            mk(usdt.lookup("BTC-USDT").unwrap()),
            Exposure::Adds { notional: Some(_) }
        ));
    }
}
