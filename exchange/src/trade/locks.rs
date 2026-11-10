//! The per-(account, symbol) lock of condition 60, shared by every `TradeClient` of the same
//! account (F2): a client rebuilt for each order (the Keychain item is re-read every time)
//! must not forget an unresolved `Unknown`.
//!
//! Two counts per key: `unknown` (sends whose outcome is not known yet) and `inflight` (a
//! new entry that has been handed to the transport and has not returned). Marking an entry
//! in flight *before* it is sent keeps two concurrent entries on one symbol from both passing.

use crate::account::AccountKey;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};

type Key = (AccountKey, String);

#[derive(Default)]
struct State {
    unknown: usize,
    inflight: usize,
}

impl State {
    fn idle(&self) -> bool {
        self.unknown == 0 && self.inflight == 0
    }
}

static LOCKS: LazyLock<Mutex<HashMap<Key, State>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn with<T>(f: impl FnOnce(&mut HashMap<Key, State>) -> T) -> T {
    f(&mut LOCKS.lock().unwrap_or_else(PoisonError::into_inner))
}

fn release(map: &mut HashMap<Key, State>, key: &Key) {
    if map.get(key).is_some_and(State::idle) {
        map.remove(key);
    }
}

/// The new entry was refused: an earlier send is unresolved or still in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Locked;

pub(crate) fn is_locked(account: AccountKey, symbol: &str) -> bool {
    with(|m| {
        m.get(&(account, symbol.to_string()))
            .is_some_and(|s| !s.idle())
    })
}

/// One more unresolved send. Counted, so resolving one of two Unknowns on the same symbol
/// does not release the other.
pub(crate) fn lock_unknown(account: AccountKey, symbol: &str) {
    with(|m| m.entry((account, symbol.to_string())).or_default().unknown += 1);
}

pub(crate) fn unlock_unknown(account: AccountKey, symbol: &str) {
    with(|m| {
        let key = (account, symbol.to_string());
        if let Some(s) = m.get_mut(&key) {
            s.unknown = s.unknown.saturating_sub(1);
        }
        release(m, &key);
    });
}

/// Mark an entry in flight, or refuse when the symbol is locked or already has one.
pub(crate) fn reserve(account: AccountKey, symbol: &str) -> Result<Inflight, Locked> {
    with(|m| {
        let key = (account, symbol.to_string());
        let s = m.entry(key.clone()).or_default();
        if !s.idle() {
            return Err(Locked);
        }
        s.inflight += 1;
        Ok(Inflight { key, armed: true })
    })
}

/// An entry on its way to the exchange. If the future that owns it is dropped before the
/// send returned (cancelled), the order may be out there: it turns into an unresolved
/// `Unknown` that only a restart clears (fail closed).
pub(crate) struct Inflight {
    key: Key,
    armed: bool,
}

impl Inflight {
    /// The send returned. `unknown`: its outcome is not known, keep the symbol locked.
    pub(crate) fn finish(mut self, unknown: bool) {
        self.armed = false;
        with(|m| {
            if let Some(s) = m.get_mut(&self.key) {
                s.inflight = s.inflight.saturating_sub(1);
                if unknown {
                    s.unknown += 1;
                }
            }
            release(m, &self.key);
        });
    }
}

impl Drop for Inflight {
    fn drop(&mut self) {
        if self.armed {
            with(|m| {
                if let Some(s) = m.get_mut(&self.key) {
                    s.inflight = s.inflight.saturating_sub(1);
                    s.unknown += 1;
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> AccountKey {
        AccountKey::from_bytes([n; 16])
    }

    #[test]
    fn a_second_entry_is_refused_while_one_is_in_flight() {
        let a = key(201);
        let first = reserve(a, "BTCUSDT").unwrap();
        assert_eq!(reserve(a, "BTCUSDT").err(), Some(Locked));
        assert!(reserve(a, "ETHUSDT").is_ok());
        first.finish(false);
        assert!(!is_locked(a, "BTCUSDT"));
        assert!(reserve(a, "BTCUSDT").is_ok());
    }

    #[test]
    fn the_lock_is_shared_by_account_not_by_client() {
        let a = key(202);
        reserve(a, "BTCUSDT").unwrap().finish(true);
        assert!(is_locked(a, "BTCUSDT"));
        assert!(!is_locked(key(203), "BTCUSDT"));
        unlock_unknown(a, "BTCUSDT");
        assert!(!is_locked(a, "BTCUSDT"));
    }

    #[test]
    fn a_dropped_send_stays_locked() {
        let a = key(204);
        drop(reserve(a, "BTCUSDT").unwrap());
        assert!(is_locked(a, "BTCUSDT"));
        unlock_unknown(a, "BTCUSDT");
        assert!(!is_locked(a, "BTCUSDT"));
    }
}
