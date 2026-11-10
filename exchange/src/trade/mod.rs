//! Order construction for manual trading (M5, stage a2): trading rules, rounding, limit
//! checks, client ids and request builders.
//!
//! Everything here is a pure function from typed intents to request parameters. Nothing
//! sends: sending, cancelling over the network and the Unknown-outcome flow belong to a3.
//! The indicator crates must not use this module (condition 75).

pub mod binance_futures;
pub mod binance_spot;
pub mod checks;
pub mod client_id;
mod common;
pub mod decimal;
pub mod okx;
pub mod request;
pub mod rules;
pub mod types;

pub use checks::{DEFAULT_DEVIATION_BPS, deviation_exceeds};
pub use client_id::{ClientId, IdField, Role};
pub use decimal::{Decimal, DecimalError};
pub use request::{BinanceRequest, Method, OkxRequest};
pub use rules::{RulesTable, SymbolRules, TradeProduct, ValidSymbol};
pub use types::{
    Action, Dir, Exec, MarginMode, OrderRef, Protective, ProtectiveKind, Side, Size, TimeInForce,
    TradeError, Trailing, WorkingType,
};

#[cfg(test)]
mod matrix_tests;
