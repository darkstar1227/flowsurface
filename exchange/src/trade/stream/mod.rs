//! Private WebSocket streams and the REST resync behind them (M5 stage b, conditions 40, 42,
//! 45–52, 57). Pure parts (`events`, `binance`, `okx`, `machine`, `secret`) are tested without
//! a socket; `driver` is the thin I/O layer that connects them.

pub mod binance;
pub mod driver;
pub mod events;
mod key_claims;
mod listen_key;
pub mod machine;
pub mod okx;
mod secret;
pub mod status;

pub use driver::{
    CredentialLoader, CredentialsUnavailable, PrivateStream, Snapshot, StreamContext, StreamOutput,
    run,
};
pub use events::{
    AccountReason, BalanceUpdate, EquityUpdate, Fill, OrderCategory, OrderStatus, OrderUpdate,
    PositionSide, PositionUpdate, PrivateEvent, ResyncReason,
};
pub use machine::{
    Command, DownCause, InvalidReason, StreamState, TradeAction, allows, lists_may_be_stale,
    one_cancels_other_paused,
};
pub use secret::{ListenKey, StreamKey};
pub use status::{stream_state, stream_state_for, sync_epoch};
