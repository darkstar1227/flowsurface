//! Read requests and bulk cancels (stage a3): open orders, algo orders, order lists,
//! positions, balances, leverage and margin mode, and the cancel-all endpoints.
//!
//! All of them are `Exposure::Neutral`. Endpoints are checked against the official docs copy
//! unless noted: Binance spot `openOrders`, `openOrderList`, `account`, `DELETE openOrders`;
//! USDⓈ-M `openOrders`, `openAlgoOrders`, `allOpenOrders`, `algoOpenOrders`, `positionRisk`
//! v3, `balance` v3 (explorer report); OKX `orders-pending`, `orders-algo-pending`,
//! `account/positions`, `account/balance`, `account/leverage-info`.

use super::okx::OkxAlgoType;
use super::request::{BinanceRequest, Method, OkxRequest};
use super::rules::ValidSymbol;
use super::types::{MarginMode, TradeError};

/// USDⓈ-M open orders; `symbol = None` reads every symbol (weight 40 instead of 1).
pub fn futures_open_orders(symbol: Option<&ValidSymbol>) -> BinanceRequest {
    with_symbol(
        BinanceRequest::new(Method::Get, "/fapi/v1/openOrders"),
        symbol,
    )
}

/// USDⓈ-M open algo (conditional) orders.
pub fn futures_open_algo_orders(symbol: Option<&ValidSymbol>) -> BinanceRequest {
    with_symbol(
        BinanceRequest::new(Method::Get, "/fapi/v1/openAlgoOrders"),
        symbol,
    )
}

/// USDⓈ-M positions with leverage and margin type (`positionRisk` v3).
pub fn futures_positions() -> BinanceRequest {
    BinanceRequest::new(Method::Get, "/fapi/v3/positionRisk")
}

/// USDⓈ-M account information v3: `totalMarginBalance` is the equity figure of the risk
/// controls (checked against the SDK docs copy, not a testnet response).
pub fn futures_account() -> BinanceRequest {
    BinanceRequest::new(Method::Get, "/fapi/v3/account")
}

pub fn futures_balance() -> BinanceRequest {
    BinanceRequest::new(Method::Get, "/fapi/v3/balance")
}

/// `DELETE /fapi/v1/allOpenOrders`: plain orders only, not algo orders.
pub fn futures_cancel_all_open(symbol: &ValidSymbol) -> BinanceRequest {
    with_symbol(
        BinanceRequest::new(Method::Delete, "/fapi/v1/allOpenOrders"),
        Some(symbol),
    )
}

/// `DELETE /fapi/v1/algoOpenOrders`: algo orders of one symbol.
pub fn futures_cancel_all_algo(symbol: &ValidSymbol) -> BinanceRequest {
    with_symbol(
        BinanceRequest::new(Method::Delete, "/fapi/v1/algoOpenOrders"),
        Some(symbol),
    )
}

pub fn spot_open_orders(symbol: Option<&ValidSymbol>) -> BinanceRequest {
    with_symbol(
        BinanceRequest::new(Method::Get, "/api/v3/openOrders"),
        symbol,
    )
}

pub fn spot_open_order_lists() -> BinanceRequest {
    BinanceRequest::new(Method::Get, "/api/v3/openOrderList")
}

pub fn spot_account() -> BinanceRequest {
    BinanceRequest::new(Method::Get, "/api/v3/account")
}

/// `DELETE /api/v3/openOrders`: every open order of the symbol, order lists included.
pub fn spot_cancel_all(symbol: &ValidSymbol) -> BinanceRequest {
    with_symbol(
        BinanceRequest::new(Method::Delete, "/api/v3/openOrders"),
        Some(symbol),
    )
}

fn with_symbol(mut r: BinanceRequest, symbol: Option<&ValidSymbol>) -> BinanceRequest {
    if let Some(s) = symbol {
        r.push("symbol", s.as_str());
    }
    r
}

/// `GET /api/v5/trade/orders-pending` (plain orders) for an instrument type.
pub fn okx_pending_orders(inst_type: OkxInstType) -> OkxRequest {
    OkxRequest::new(
        Method::Get,
        format!(
            "/api/v5/trade/orders-pending?instType={}",
            inst_type.as_str()
        ),
        None,
    )
}

/// `GET /api/v5/trade/orders-algo-pending` (`ordType` is required). Cancel-all reads every
/// type in `OkxAlgoType::ALL_PENDING`, in the groups of `OkxAlgoType::PENDING_READS`, so no
/// kind of order is missed.
pub fn okx_pending_algos(algo_types: &[OkxAlgoType]) -> Result<OkxRequest, TradeError> {
    if algo_types.is_empty() {
        return Err(TradeError::Unsupported);
    }
    let names: Vec<&str> = algo_types.iter().map(|t| t.pending_name()).collect();
    Ok(OkxRequest::new(
        Method::Get,
        format!(
            "/api/v5/trade/orders-algo-pending?ordType={}",
            names.join(",")
        ),
        None,
    ))
}

pub fn okx_positions() -> OkxRequest {
    OkxRequest::new(Method::Get, "/api/v5/account/positions".to_string(), None)
}

pub fn okx_balance() -> OkxRequest {
    OkxRequest::new(Method::Get, "/api/v5/account/balance".to_string(), None)
}

/// `GET /api/v5/account/leverage-info`.
pub fn okx_leverage_info(symbol: &ValidSymbol, margin: MarginMode) -> OkxRequest {
    OkxRequest::new(
        Method::Get,
        format!(
            "/api/v5/account/leverage-info?instId={}&mgnMode={}",
            symbol.as_str(),
            margin.okx()
        ),
        None,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OkxInstType {
    Spot,
    Swap,
}

impl OkxInstType {
    fn as_str(self) -> &'static str {
        match self {
            OkxInstType::Spot => "SPOT",
            OkxInstType::Swap => "SWAP",
        }
    }
}
