//! Manual trading (M5, stages a2 and a3): trading rules, rounding, limit checks, client ids,
//! request builders, and the send layer.
//!
//! The builders are pure functions from typed intents to request parameters. The only way to
//! send an order, cancel or close is `TradeClient::submit` / `submit_auto` (the single gate,
//! condition 72); `LIVE_SENDING_ENABLED` in `data` still keeps Live closed. Sends are never
//! repeated: an `Unknown` outcome is resolved by lookup (`unknown`), not by resending.
//! The indicator crates must not use this module (condition 75).

pub mod binance_futures;
pub mod binance_spot;
pub mod bulk;
pub mod checks;
pub mod client;
pub mod client_id;
mod common;
pub mod decimal;
pub mod gate;
mod locks;
pub mod okx;
pub mod outcome;
pub mod queries;
pub mod request;
pub mod risk;
pub mod rules;
pub mod stream;
pub mod types;
pub mod unknown;

pub use bulk::{AutoCloseReport, BulkError, BulkReport};
pub use checks::{DEFAULT_DEVIATION_BPS, deviation_exceeds};
pub use client::{
    Balance, OpenKind, OpenOrder, Position, ReadError, Submitted, TradeClient, TradeSettings,
    settings_match,
};
pub use client_id::{ClientId, IdField, Role};
pub use decimal::{Decimal, DecimalError};
pub use gate::{
    ApprovedOrder, AutoIntent, AutoPath, ConfirmToken, GateClass, GateRequest, OrderIntent,
    RiskHook, RiskRefusal, RiskView, SendGate, SubmitError,
};
pub use outcome::{ItemRejection, Outcome, RejectKind};
pub use request::{BinanceRequest, Exposure, LookupKind, Method, OkxRequest};
pub use risk::{
    AutoCloseDecision, AutoCloseGrant, DrawdownAnchor, EquityBasis, EquityFreshness,
    EquityObservation, EquitySource, EquityTracker, IgnoreReason, Observed, RiskLimits, RiskNotice,
    RiskNoticeKind, RiskSnapshot, TripReason, local_day,
};
pub use rules::{FreshRules, RulesTable, SymbolRules, TradeProduct, ValidSymbol};
pub use types::{
    Action, Dir, Exec, MarginMode, OrderRef, Protective, ProtectiveKind, Side, Size, TimeInForce,
    TradeError, Trailing, WorkingType,
};
pub use unknown::{DEFAULT_SCHEDULE, PendingUnknown, Resolution};

#[cfg(test)]
mod matrix_tests;
#[cfg(test)]
mod send_tests;
