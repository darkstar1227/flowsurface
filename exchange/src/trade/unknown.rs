//! The `Unknown` flow (condition 60): a send whose result is not known is never repeated.
//! The order is looked up by its client id with a backoff until it is found or the window
//! ends; after that it is reported as "not placed (presumed)". While it is unresolved the
//! (account, symbol) is locked against new entries (`TradeClient::is_locked`).
//!
//! "Not placed" needs all of: the last look was answered "no such order" (a verified code, see
//! [`is_not_found`], or an empty OKX `data`), and that look was made no earlier than
//! `sent_at + recvWindow + SETTLE_BUFFER_MS` (OKX: `sent_at + OKX_SETTLE_FLOOR_MS`). Any other answer, or silence, keeps the lock.

use super::binance_futures as fut;
use super::binance_spot as spot;
use super::client::{RECV_WINDOW_MS, ReadError, TradeClient};
use super::client_id::ClientId;
use super::okx;
use super::request::{Lookup, LookupKind};
use super::rules::SymbolRules;
use super::types::OrderRef;
use crate::account::AccountError;
use std::time::Duration;

/// An order whose send came back `Unknown`. Hand it to `TradeClient::resolve_unknown`.
#[derive(Debug)]
pub struct PendingUnknown {
    pub(crate) symbol: String,
    pub(crate) lookup: Lookup,
    pub(crate) rules: SymbolRules,
    /// When the send came back, on the client's monotonic clock (ms): a wall-clock jump or a
    /// clock sync cannot shorten the wait.
    pub(crate) sent_mono_ms: i64,
}

impl PendingUnknown {
    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub fn client_id(&self) -> &str {
        &self.lookup.client_id
    }
}

/// Default window: look at once, then after 1, 2, 4, 8, 15 and 30 seconds (about one minute).
pub const DEFAULT_SCHEDULE: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(15),
    Duration::from_secs(30),
];

/// Extra wait after `recvWindow` before a "no such order" answer may settle the question: the
/// exchange can still be placing an order it accepted inside the window.
pub const SETTLE_BUFFER_MS: i64 = 10_000;

/// Floor for OKX orders. They carry no `recvWindow` or expiry time here, so the exchange has
/// no time limit for placing one; this is a longer, conservative wait of its own.
pub const OKX_SETTLE_FLOOR_MS: i64 = 60_000;

/// The earliest time (client monotonic clock) at which "not found" counts.
fn settled_at(kind: LookupKind, sent_mono_ms: i64) -> i64 {
    let floor = match kind {
        LookupKind::OkxOrder | LookupKind::OkxAlgo(_) => OKX_SETTLE_FLOOR_MS,
        LookupKind::SpotOrder
        | LookupKind::SpotList
        | LookupKind::FuturesOrder
        | LookupKind::FuturesAlgo => i64::from(RECV_WINDOW_MS).saturating_add(SETTLE_BUFFER_MS),
    };
    sent_mono_ms.saturating_add(floor)
}

/// Error codes that mean "this order does not exist", per lookup. Everything else (rate
/// limits, `50004` timeouts, bad keys, `-1001`, `-1008`, ...) is not an answer about the
/// order. Only codes verified against the docs are listed: Binance spot `GET order` `-2013`
/// (`errors.md`), OKX `GET order` `51603`. USDⓈ-M `order` / `algoOrder`, spot `orderList` and
/// OKX `order-algo` are unverified (testnet to-do), so their errors are never "not found".
fn is_not_found(kind: LookupKind, code: &str) -> bool {
    match kind {
        LookupKind::SpotOrder => code == "-2013",
        LookupKind::OkxOrder => code == "51603",
        LookupKind::FuturesOrder
        | LookupKind::FuturesAlgo
        | LookupKind::SpotList
        | LookupKind::OkxAlgo(_) => false,
    }
}

#[derive(Debug)]
pub enum Resolution {
    /// The order exists on the exchange.
    Found,
    /// The exchange answered "no such order" within the window and never showed it: presumed
    /// not placed (未成立（推定）). Nothing was resent.
    NotPlacedPresumed,
    /// No lookup got an answer (the exchange or the network stayed silent). The symbol stays
    /// locked and nothing may be assumed: try `resolve_unknown` again with this value.
    Unresolved(Box<PendingUnknown>),
}

/// What one lookup learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    Found,
    /// The exchange answered and does not know the order.
    NotFound,
    /// Timeout, transport error, bad response, throttling: nothing learned.
    NoAnswer,
}

impl TradeClient {
    /// Look the order up by client id; `schedule` is the wait before each further look (it
    /// can only lengthen the window, never shorten it below the floor). Never sends an order.
    /// The result is `NotPlacedPresumed` only when the last look was answered "no such order"
    /// and was made after `sent_at + recvWindow + SETTLE_BUFFER_MS` (OKX: `sent_at + OKX_SETTLE_FLOOR_MS`); if the schedule ends
    /// earlier, one more look is made at that time. Otherwise, with no conclusive answer, the
    /// result is `Unresolved` and the lock stays.
    pub async fn resolve_unknown(
        &self,
        pending: PendingUnknown,
        schedule: &[Duration],
    ) -> Resolution {
        let settled = settled_at(pending.lookup.kind, pending.sent_mono_ms);
        let mut waits = schedule.iter().copied();
        let mut extended = false;
        let (last, at) = loop {
            // Taken before the query goes out: an answer reflects the exchange at the time the
            // request arrived, not at the time the reply came back.
            let at = self.mono_ms();
            let look = self.lookup_once(&pending).await;
            if look == Look::Found {
                self.unlock(&pending.symbol, pending.client_id());
                return Resolution::Found;
            }
            let next = match waits.next() {
                Some(w) => Some(w),
                None if look == Look::NotFound && at < settled && !extended => {
                    extended = true;
                    let rest = u64::try_from(settled - at).unwrap_or(0);
                    Some(Duration::from_millis(rest))
                }
                None => None,
            };
            match next {
                Some(w) => self.wait(w).await,
                None => break (look, at),
            }
        };
        if last == Look::NotFound && at >= settled {
            self.unlock(&pending.symbol, pending.client_id());
            Resolution::NotPlacedPresumed
        } else {
            Resolution::Unresolved(Box::new(pending))
        }
    }

    /// One query by client id. Only a verified "no such order" answer counts as "not found"
    /// (see [`is_not_found`]); a timeout, a transport error, throttling or any other exchange
    /// error teaches nothing.
    async fn lookup_once(&self, pending: &PendingUnknown) -> Look {
        match self.query_lookup(&pending.rules, &pending.lookup).await {
            Ok(true) => Look::Found,
            Ok(false) => Look::NotFound,
            Err(_) => Look::NoAnswer,
        }
    }

    /// `Ok(true)`: the exchange knows the order. `Ok(false)`: it answered that it does not
    /// (verified code or empty data). Any other failure is an `Err`.
    pub(crate) async fn query_lookup(
        &self,
        rules: &SymbolRules,
        lookup: &Lookup,
    ) -> Result<bool, ReadError> {
        match self.query_lookup_raw(rules, lookup).await {
            Err(ReadError::Transport(AccountError::Exchange { code, .. }))
                if is_not_found(lookup.kind, &code) =>
            {
                Ok(false)
            }
            other => other,
        }
    }

    async fn query_lookup_raw(
        &self,
        rules: &SymbolRules,
        lookup: &Lookup,
    ) -> Result<bool, ReadError> {
        let by_client =
            OrderRef::Client(ClientId::parse(&lookup.client_id).map_err(ReadError::from)?);
        match lookup.kind {
            LookupKind::FuturesOrder => {
                let v = self
                    .read_binance(&fut::query_order(rules, &by_client)?)
                    .await?;
                Ok(v.is_object())
            }
            LookupKind::FuturesAlgo => {
                let v = self
                    .read_binance(&fut::query_algo_order(&by_client)?)
                    .await?;
                Ok(v.is_object())
            }
            LookupKind::SpotOrder => {
                let v = self
                    .read_binance(&spot::query_order(rules, &by_client)?)
                    .await?;
                Ok(v.is_object())
            }
            LookupKind::SpotList => {
                let v = self
                    .read_binance(&spot::query_order_list(&by_client)?)
                    .await?;
                Ok(v.is_object())
            }
            LookupKind::OkxOrder => {
                let d = self.read_okx(&okx::query_order(rules, &by_client)?).await?;
                Ok(!d.is_empty())
            }
            LookupKind::OkxAlgo(t) => {
                let d = self.read_okx(&okx::query_algo(t, &by_client)?).await?;
                Ok(!d.is_empty())
            }
        }
    }
}
