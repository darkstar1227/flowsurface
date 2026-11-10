//! Typed order intents shared by the three venue builders, and the error type.

use super::client_id::ClientId;
use super::decimal::{Decimal, DecimalError};
use crate::signing::SigningError;
use std::fmt;

/// Why a request could not be built. Variants carry no order values, so `Display` and
/// `Debug` are safe to log (condition 38).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeError {
    Decimal(DecimalError),
    Signing(SigningError),
    /// The symbol is not in the trading rules list, or has forbidden characters.
    UnknownSymbol,
    /// The rules table was built for another product than the builder.
    ProductMismatch,
    /// A needed rule is missing or has an unusable value: refuse to send (condition 62).
    RuleMissing,
    /// The rules are older than the allowed age.
    RulesStale,
    /// The exchange has not reported the real position mode: refuse futures orders (condition 58).
    PositionModeUnknown,
    /// Quantity or price is zero, negative, or rounds to zero.
    NonPositive,
    QtyBelowMin,
    QtyAboveMax,
    NotionalBelowMin,
    NotionalAboveMax,
    /// A reference price is needed (notional check, trigger side check) and none was given.
    MissingReferencePrice,
    /// The trigger price is on the wrong side of the reference price.
    TriggerOnWrongSide,
    /// A limit price is required for this order type and is missing, or is given where
    /// it is not allowed.
    PriceInvalid,
    /// Hedge accounts only get full-close market protective orders (user decision).
    HedgeProtectiveMustClosePosition,
    /// `closePosition` is only valid on STOP_MARKET / TAKE_PROFIT_MARKET.
    ClosePositionNotAllowed,
    /// `trailingDelta` is outside the symbol's TRAILING_DELTA filter.
    TrailingDeltaOutOfRange,
    /// This order type is not offered (OKX trailing stop, condition 67).
    Unsupported,
    /// OKX `conditional` with both TP and SL: the exchange would drop the TP (condition 64).
    OkxConditionalWithBothLegs,
    /// OKX `oco` needs both a TP and an SL.
    OkxOcoNeedsBothLegs,
    /// OKX algo order with neither TP nor SL.
    OkxNoTrigger,
    /// OKX `attachAlgoOrds` only protects an order that opens or adds to a position: a spot
    /// buy or a swap open. On a sell or a close the attached order would be on the wrong
    /// side and could add exposure (condition 64).
    AttachOnlyOnOpen,
    /// The client ids of one OCO/OTOCO list (list, working, above, below) must differ.
    ClientIdsNotDistinct,
    /// A client id does not match the venue's pattern (condition 59).
    ClientIdInvalid,
    /// An exchange order id is not a plain number.
    OrderIdInvalid,
    /// The protective sell is larger than the available balance.
    ExceedsAvailable,
    AutoPathMustReduce,
}

impl fmt::Display for TradeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            TradeError::Decimal(e) => return write!(f, "{e}"),
            TradeError::Signing(e) => return write!(f, "{e}"),
            TradeError::UnknownSymbol => "symbol is not in the trading rules",
            TradeError::ProductMismatch => "trading rules belong to another product",
            TradeError::RuleMissing => "a trading rule is missing or unusable",
            TradeError::RulesStale => "trading rules are too old",
            TradeError::PositionModeUnknown => "position mode is unknown",
            TradeError::NonPositive => "quantity or price must be positive",
            TradeError::QtyBelowMin => "quantity is below the minimum",
            TradeError::QtyAboveMax => "quantity is above the maximum",
            TradeError::NotionalBelowMin => "order value is below the minimum",
            TradeError::NotionalAboveMax => "order value is above the maximum",
            TradeError::MissingReferencePrice => "a reference price is required",
            TradeError::TriggerOnWrongSide => "trigger price is on the wrong side of the market",
            TradeError::PriceInvalid => "price is missing or not allowed for this order type",
            TradeError::HedgeProtectiveMustClosePosition => {
                "hedge accounts only support full-close market protective orders"
            }
            TradeError::ClosePositionNotAllowed => "closePosition is not allowed for this type",
            TradeError::TrailingDeltaOutOfRange => "trailing delta is outside the allowed range",
            TradeError::Unsupported => "this order type is not supported",
            TradeError::OkxConditionalWithBothLegs => {
                "an OKX conditional order with both TP and SL must be an oco order"
            }
            TradeError::OkxOcoNeedsBothLegs => "an OKX oco order needs both TP and SL",
            TradeError::OkxNoTrigger => "an OKX algo order needs a TP or an SL",
            TradeError::AttachOnlyOnOpen => {
                "attached TP/SL is only allowed on a spot buy or a swap open"
            }
            TradeError::ClientIdsNotDistinct => "client ids of one order list must differ",
            TradeError::ClientIdInvalid => "client id does not match the venue's pattern",
            TradeError::OrderIdInvalid => "order id must be a plain number",
            TradeError::ExceedsAvailable => "quantity exceeds the available balance",
            TradeError::AutoPathMustReduce => "automatic paths may only reduce, close or cancel",
        };
        f.write_str(text)
    }
}

impl std::error::Error for TradeError {}

impl From<DecimalError> for TradeError {
    fn from(e: DecimalError) -> Self {
        TradeError::Decimal(e)
    }
}

impl From<SigningError> for TradeError {
    fn from(e: SigningError) -> Self {
        TradeError::Signing(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }

    pub(crate) fn binance(self) -> &'static str {
        match self {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        }
    }

    pub(crate) fn okx(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

/// Direction of a derivatives position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Long,
    Short,
}

impl Dir {
    /// The side that opens this direction.
    pub fn opening_side(self) -> Side {
        match self {
            Dir::Long => Side::Buy,
            Dir::Short => Side::Sell,
        }
    }

    /// The side that closes this direction (also the side of its protective orders).
    pub fn closing_side(self) -> Side {
        self.opening_side().opposite()
    }
}

/// What a derivatives order does to the position; the side is derived from it
/// (open long = BUY, open short = SELL, close long = SELL, close short = BUY).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Open(Dir),
    Close(Dir),
}

impl Action {
    pub fn dir(self) -> Dir {
        match self {
            Action::Open(d) | Action::Close(d) => d,
        }
    }

    pub fn side(self) -> Side {
        match self {
            Action::Open(d) => d.opening_side(),
            Action::Close(d) => d.closing_side(),
        }
    }

    pub fn is_close(self) -> bool {
        matches!(self, Action::Close(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeInForce {
    Gtc,
    Ioc,
    Fok,
}

impl TimeInForce {
    pub(crate) fn binance(self) -> &'static str {
        match self {
            TimeInForce::Gtc => "GTC",
            TimeInForce::Ioc => "IOC",
            TimeInForce::Fok => "FOK",
        }
    }
}

/// How an entry order executes. A limit price is the user's price before rounding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exec {
    Limit { price: Decimal, tif: TimeInForce },
    Market,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectiveKind {
    /// Stop that sends a market order when triggered.
    StopMarket,
    TakeProfitMarket,
    /// Stop that sends a limit order when triggered (needs `limit_price`).
    StopLimit,
    TakeProfitLimit,
    /// Trailing stop; the callback is in `Protective::trailing`.
    Trailing,
}

impl ProtectiveKind {
    pub fn is_stop_side(self) -> bool {
        matches!(
            self,
            ProtectiveKind::StopMarket | ProtectiveKind::StopLimit | ProtectiveKind::Trailing
        )
    }

    pub fn is_market(self) -> bool {
        matches!(
            self,
            ProtectiveKind::StopMarket | ProtectiveKind::TakeProfitMarket
        )
    }
}

/// How much a protective order covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    /// A quantity (contracts on OKX swaps, base units elsewhere), rounded by the builder.
    Qty(Decimal),
    /// The whole position (`closePosition` / `closeFraction=1`).
    ClosePosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkingType {
    MarkPrice,
    ContractPrice,
}

impl WorkingType {
    pub(crate) fn binance(self) -> &'static str {
        match self {
            WorkingType::MarkPrice => "MARK_PRICE",
            WorkingType::ContractPrice => "CONTRACT_PRICE",
        }
    }
}

/// Trailing stop parameters. The callback is a percentage on Binance futures
/// (`callbackRate`, e.g. 1.5 = 1.5%) and basis points on Binance spot (`trailingDelta`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trailing {
    /// Binance futures `callbackRate` in percent, with an optional `activatePrice`.
    CallbackRate {
        percent: Decimal,
        activate_price: Option<Decimal>,
    },
    /// Binance spot `trailingDelta`, in basis points (1 = 0.01%).
    DeltaBips(u32),
}

/// A protective order for an existing position (derivatives) or holding (spot, long only).
#[derive(Debug, Clone, Copy)]
pub struct Protective<'a> {
    pub kind: ProtectiveKind,
    /// The position being protected. The order's side is always the opposite.
    pub dir: Dir,
    pub size: Size,
    /// Trigger price, rounded to the nearest tick. Not used by `Trailing`.
    pub trigger_price: Option<Decimal>,
    /// Limit price for the `*Limit` kinds (rounded in the safe direction).
    pub limit_price: Option<Decimal>,
    pub tif: TimeInForce,
    pub working_type: WorkingType,
    pub trailing: Option<Trailing>,
    /// Last or mark price, used to check which side of the market the trigger is on.
    pub ref_price: Decimal,
    pub client_id: &'a ClientId,
}

/// Position mode and margin settings for a derivatives order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginMode {
    Cross,
    Isolated,
}

impl MarginMode {
    pub(crate) fn okx(self) -> &'static str {
        match self {
            MarginMode::Cross => "cross",
            MarginMode::Isolated => "isolated",
        }
    }
}

/// An exchange-assigned order id (digits only) or our own client id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderRef {
    Exchange(String),
    Client(ClientId),
}

impl OrderRef {
    /// Exchange ids are numbers on Binance and OKX; anything else is refused so it cannot
    /// smuggle query or JSON syntax.
    pub fn exchange(id: &str) -> Result<OrderRef, TradeError> {
        let ok = !id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit());
        if ok {
            Ok(OrderRef::Exchange(id.to_string()))
        } else {
            Err(TradeError::OrderIdInvalid)
        }
    }
}
