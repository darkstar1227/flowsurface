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
}

static TABLE: LazyLock<Mutex<HashMap<AccountKey, Entry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Tokens start at 1; 0 is reserved for the test setter.
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

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
        m.insert(account, Entry { token: 0, state });
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
}
