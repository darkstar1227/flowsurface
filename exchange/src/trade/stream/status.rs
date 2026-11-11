//! The stream state the send gate reads (condition 51, user decision 3).
//!
//! One process-wide table, keyed by account, like the symbol locks: a `TradeClient` rebuilt for
//! every order (the Keychain item is re-read each time, condition 57) still sees what the
//! account's private stream last reported. Only the stream driver writes. [`run`] registers a
//! stream under a fresh token and every later write must present it, so a stream that was
//! replaced, dropped or shut down can no longer change the entry. Dropping a stream removes its
//! entry, and an account with no entry reads as `Reconnecting`: a missing stream never looks
//! connected. Binance spot has no private stream until verified on testnet (user decision 3),
//! so it reads as `Unavailable` whatever the table says.
//!
//! [`run`]: super::run

use super::binance::SPOT_PRIVATE_STREAM_VERIFIED;
use super::machine::StreamState;
use crate::account::{AccountKey, AccountProduct, AccountVenue};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, PoisonError};

struct Entry {
    token: u64,
    state: StreamState,
    /// Which snapshot of this stream the consumer must have applied (see [`sync_epoch`]).
    epoch: u64,
}

static TABLE: LazyLock<Mutex<HashMap<AccountKey, Entry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Tokens start at 1; 0 is reserved for the test setter.
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
/// Sync epochs start at 1 and are unique across every stream, so an epoch a consumer applied
/// for an earlier stream can never equal the one a later stream publishes. 0 means "no
/// snapshot sent yet".
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

fn with<T>(f: impl FnOnce(&mut HashMap<AccountKey, Entry>) -> T) -> T {
    f(&mut TABLE.lock().unwrap_or_else(PoisonError::into_inner))
}

/// A new stream for `account` takes over the entry, as `Reconnecting`. Whatever stream held it
/// before can no longer write.
pub(crate) fn register(account: AccountKey) -> u64 {
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    with(|m| {
        m.insert(
            account,
            Entry {
                token,
                state: StreamState::Reconnecting,
                epoch: 0,
            },
        );
    });
    token
}

/// Record the stream's state. Ignored unless `token` still owns the entry; never creates one.
pub(crate) fn publish(account: AccountKey, token: u64, state: StreamState) {
    with(|m| {
        if let Some(e) = m.get_mut(&account).filter(|e| e.token == token) {
            e.state = state;
        }
    });
}

/// The stream is about to hand its consumer a new snapshot: take a new epoch for it. Returns the
/// epoch to put into the snapshot (0 when `token` no longer owns the entry, which no consumer
/// can match).
pub(crate) fn begin_sync(account: AccountKey, token: u64) -> u64 {
    with(|m| match m.get_mut(&account).filter(|e| e.token == token) {
        Some(e) => {
            e.epoch = NEXT_EPOCH.fetch_add(1, Ordering::Relaxed);
            e.epoch
        }
        None => 0,
    })
}

/// The epoch of the account's latest snapshot, or 0 when none was sent. A consumer's risk
/// tracker must have applied exactly this snapshot before an entry passes: the stream state
/// turns `Connected` as soon as the snapshot is *sent*, which can be before the consumer has
/// *applied* it (review N1).
pub fn sync_epoch(account: AccountKey) -> u64 {
    with(|m| m.get(&account).map_or(0, |e| e.epoch))
}

/// The supervisor ended: a stream that was `Connected` or `Resyncing` is `Reconnecting` now.
/// Terminal states (`AuthFailed`, `Unavailable`) are kept, so the reason a stream stopped is
/// not overwritten by the exit itself (review N6). Ignored unless `token` owns the entry.
pub(crate) fn demote(account: AccountKey, token: u64) {
    with(|m| {
        if let Some(e) = m.get_mut(&account).filter(|e| e.token == token)
            && matches!(e.state, StreamState::Connected | StreamState::Resyncing)
        {
            e.state = StreamState::Reconnecting;
        }
    });
}

/// The stream ended (dropped or shut down): remove its entry, if it still owns it.
pub(crate) fn retire(account: AccountKey, token: u64) {
    with(|m| {
        if m.get(&account).is_some_and(|e| e.token == token) {
            m.remove(&account);
        }
    });
}

/// What the account's private stream last reported; `Reconnecting` when no stream is
/// registered. This is the account's raw entry: for the answer a gate must use, with the spot
/// rule applied, see [`stream_state_for`].
pub fn stream_state(account: AccountKey) -> StreamState {
    stream_state_for(account, AccountVenue::Okx, AccountProduct::UsdM)
}

/// Same as [`stream_state`] for an account of this venue and product: Binance spot reads as
/// `Unavailable` (no verified private stream, so no spot trading).
pub fn stream_state_for(
    account: AccountKey,
    venue: AccountVenue,
    product: AccountProduct,
) -> StreamState {
    let entry = with(|m| m.get(&account).map(|e| (e.token, e.state)));
    // Token 0 is the test setter: tests of spot sends need a connected spot client.
    #[cfg(test)]
    if let Some((0, state)) = entry {
        return state;
    }
    if venue == AccountVenue::Binance
        && product == AccountProduct::Spot
        && !SPOT_PRIVATE_STREAM_VERIFIED
    {
        return StreamState::Unavailable;
    }
    entry.map_or(StreamState::Reconnecting, |(_, s)| s)
}

/// Test setter: writes the entry as a stream would, under the reserved token 0.
#[cfg(test)]
pub(crate) fn set_for_test(account: AccountKey, state: StreamState) {
    with(|m| {
        m.insert(
            account,
            Entry {
                token: 0,
                state,
                epoch: 0,
            },
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> AccountKey {
        AccountKey::from_bytes([n; 16])
    }

    #[test]
    fn an_account_with_no_stream_reads_as_reconnecting() {
        assert_eq!(stream_state(key(110)), StreamState::Reconnecting);
    }

    #[test]
    fn the_state_is_published_by_the_registered_stream_only() {
        let a = key(111);
        let t = register(a);
        assert_eq!(stream_state(a), StreamState::Reconnecting);
        publish(a, t, StreamState::Connected);
        assert_eq!(stream_state(a), StreamState::Connected);
        // A wrong token and an account that has no entry change nothing.
        publish(a, t + 1000, StreamState::AuthFailed);
        assert_eq!(stream_state(a), StreamState::Connected);
        publish(key(112), t, StreamState::Connected);
        assert_eq!(stream_state(key(112)), StreamState::Reconnecting);
    }

    #[test]
    fn two_accounts_do_not_affect_each_other() {
        let (a, b) = (key(113), key(114));
        let (ta, tb) = (register(a), register(b));
        publish(a, ta, StreamState::Connected);
        assert_eq!(stream_state(b), StreamState::Reconnecting);
        publish(b, tb, StreamState::AuthFailed);
        assert_eq!(stream_state(a), StreamState::Connected);
        retire(b, tb);
        assert_eq!(stream_state(a), StreamState::Connected);
    }

    #[test]
    fn a_retired_stream_leaves_the_account_not_connected_and_cannot_write_again() {
        let a = key(115);
        let t = register(a);
        publish(a, t, StreamState::Connected);
        retire(a, t);
        assert_eq!(stream_state(a), StreamState::Reconnecting);
        // A late write of the dying task is ignored.
        publish(a, t, StreamState::Connected);
        assert_eq!(stream_state(a), StreamState::Reconnecting);
    }

    #[test]
    fn only_the_newest_stream_of_an_account_can_write_or_retire() {
        let a = key(116);
        let old = register(a);
        let new = register(a);
        publish(a, new, StreamState::Connected);
        publish(a, old, StreamState::AuthFailed);
        assert_eq!(stream_state(a), StreamState::Connected);
        // The old stream ending must not take the new stream's entry with it.
        retire(a, old);
        assert_eq!(stream_state(a), StreamState::Connected);
        retire(a, new);
        assert_eq!(stream_state(a), StreamState::Reconnecting);
    }

    #[test]
    fn binance_spot_is_never_available() {
        let a = key(117);
        let t = register(a);
        publish(a, t, StreamState::Connected);
        assert_eq!(
            stream_state_for(a, AccountVenue::Binance, AccountProduct::Spot),
            StreamState::Unavailable
        );
        assert_eq!(
            stream_state_for(a, AccountVenue::Binance, AccountProduct::UsdM),
            StreamState::Connected
        );
        assert_eq!(
            stream_state_for(a, AccountVenue::Okx, AccountProduct::Spot),
            StreamState::Connected
        );
    }

    #[test]
    fn a_new_snapshot_takes_an_epoch_nobody_else_has() {
        let (a, b) = (key(118), key(119));
        let (ta, tb) = (register(a), register(b));
        assert_eq!((sync_epoch(a), sync_epoch(b)), (0, 0));
        let e1 = begin_sync(a, ta);
        let e2 = begin_sync(b, tb);
        let e3 = begin_sync(a, ta);
        assert!(e1 > 0 && e2 > e1 && e3 > e2);
        assert_eq!(sync_epoch(a), e3);
        assert_eq!(sync_epoch(b), e2);
        // A stream that lost the entry cannot take one.
        let newer = register(a);
        assert_eq!(begin_sync(a, ta), 0);
        // The newer stream starts from "no snapshot": nothing the old one's consumer applied
        // matches it.
        assert_eq!(sync_epoch(a), 0);
        assert!(begin_sync(a, newer) > e3);
    }

    #[test]
    fn exiting_keeps_a_terminal_state() {
        let a = key(120);
        let t = register(a);
        for terminal in [StreamState::AuthFailed, StreamState::Unavailable] {
            publish(a, t, terminal);
            demote(a, t);
            assert_eq!(stream_state(a), terminal);
        }
        for live in [StreamState::Connected, StreamState::Resyncing] {
            publish(a, t, live);
            demote(a, t);
            assert_eq!(stream_state(a), StreamState::Reconnecting);
        }
        // Another stream's guard changes nothing.
        publish(a, t, StreamState::Connected);
        demote(a, t + 1000);
        assert_eq!(stream_state(a), StreamState::Connected);
    }
}
