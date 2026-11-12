//! Manual trading (M5, stages a2 and a3): trading rules, rounding, limit checks, client ids,
//! request builders, and the send layer.
//!
//! The builders are pure functions from typed intents to request parameters. The only way to
//! send an order, cancel or close is `TradeClient::submit` / `submit_auto` (the single gate,
//! condition 72); `LIVE_SENDING_ENABLED` in `data` still keeps Live closed. Sends are never
//! repeated: an `Unknown` outcome is resolved by lookup (`unknown`), not by resending.
//! The indicator crates must not use this module (condition 75).

pub mod auto_attach;
pub mod binance_futures;
pub mod binance_spot;
pub mod bulk;
pub mod checks;
pub mod client;
pub mod client_id;
mod common;
pub mod decimal;
pub mod gate;
pub mod grant;
pub mod journal;
mod locks;
pub mod manual;
pub mod okx;
pub mod outcome;
pub mod preview;
pub mod protect;
pub mod queries;
pub mod replace;
pub mod request;
pub mod risk;
pub mod rules;
pub mod stream;
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod types;
pub mod unknown;

pub use auto_attach::{Stage, StageInput, build_stage, stage_roles};
pub use bulk::{AutoCloseReport, BulkError, BulkReport};
pub use checks::{DEFAULT_DEVIATION_BPS, deviation_exceeds};
pub use client::{
    Balance, OpenKind, OpenOrder, Position, ReadError, Submitted, TradeClient, TradeSettings,
    settings_match,
};
pub use client_id::{ClientId, IdField, Role, looks_like_app_id};
pub use decimal::{Decimal, DecimalError};
pub use gate::{
    ApprovedOrder, AutoIntent, AutoPath, BulkAction, ConfirmToken, GateClass, GateRequest,
    OrderIntent, RiskHook, RiskRefusal, RiskView, SendGate, SubmitError,
};
pub use grant::{AutoGrant, Basis, GrantRefusal, GrantRoles, GrantedAuto, IssuedRef};
pub use journal::{JournalError, UnknownJournal};
pub use manual::{ManualContext, ManualSpec, build_intent, entry_protection};
pub use outcome::{ItemRejection, Outcome, RejectKind};
pub use preview::{OrderPreview, PreviewField};
pub use protect::{
    MAX_TARGETS, Offset, PlanError, PositionProtection, ProtectPlan, ProtectRole, ProtectSpec,
    Protection, Rounding, SpecFingerprint, TargetLeg, TargetSpec, TrailingSpec, TriggerBasis,
    bare_positions, cancellable_by_id, describe_protective, orphan_protectives, plan,
    protected_status, protecting_orders, residual_auto_orders, stop_guarded_position,
};
pub use replace::{
    REPLACE_GRANT_VALID_MS, ReplaceDone, ReplaceError, ReplaceGrant, ReplaceKind, ReplaceOrder,
    ReplaceRefusal, Replacement,
};
pub use request::{BinanceRequest, Exposure, LookupKind, Method, OkxRequest};
pub use risk::{
    AutoCloseDecision, AutoCloseGrant, DrawdownAnchor, EquityBasis, EquityFreshness,
    EquityObservation, EquitySource, EquityTracker, IgnoreReason, Observed, RiskLimits, RiskNotice,
    RiskNoticeKind, RiskPoisoned, RiskSnapshot, SharedRisk, StreamEquityFeed, TripReason,
    local_day,
};
pub use rules::{FreshRules, RulesTable, SymbolRules, TradeProduct, ValidSymbol};
pub use types::{
    Action, Dir, Exec, MarginMode, OrderRef, Protective, ProtectiveKind, Side, Size, TimeInForce,
    TradeError, Trailing, WorkingType,
};
pub use unknown::{
    DEFAULT_SCHEDULE, DiscardChallenge, DiscardError, DiscardToken, ManualReleaseToken,
    PendingUnknown, ReleaseChallenge, ReleaseError, ReleasedUnknown, Resolution, RestoreError,
    UnknownRecord, block_all_entries, lock_from_record,
};

#[cfg(test)]
mod matrix_tests;
#[cfg(test)]
mod send_tests;
#[cfg(test)]
mod source_rules_tests;
