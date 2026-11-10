//! The per-(account, symbol) lock of condition 60, shared by every `TradeClient` of the same
//! account (F2): a client rebuilt for each order (the Keychain item is re-read every time)
//! must not forget an unresolved `Unknown`.
//!
//! Two counts per key: `unknown` (sends whose outcome is not known yet) and `inflight` (a
//! new entry that has been handed to the transport and has not returned). Marking an entry
//! in flight *before* it is sent keeps two concurrent entries on one symbol from both passing.

use crate::account::AccountKey;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, PoisonError};

type Key = (AccountKey, String);

#[derive(Default)]
struct State {
    unknown: usize,
    inflight: usize,
    /// Client ids of the unresolved sends counted in `unknown` (those that have one). The
    /// private stream or the lookup removes an id and releases its count in one step, so
    /// the two ways of resolving one send can never release it twice.
    ids: HashSet<String>,
    /// Client ids of entries that are in flight (registered before the send goes out).
    pending: HashSet<String>,
    /// In-flight ids the stream has already reported: the exchange knows the order, so an
    /// Unknown result for it does not lock the symbol.
    seen: HashSet<String>,
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
pub(crate) fn lock_unknown(account: AccountKey, symbol: &str, client_id: Option<&str>) {
    with(|m| {
        let s = m.entry((account, symbol.to_string())).or_default();
        s.unknown += 1;
        if let Some(id) = client_id {
            s.ids.insert(id.to_string());
        }
    });
}

/// The exchange knows the send with `client_id` (a private-stream order event, or a lookup
/// that found it): release its count. `false` when the id is not registered as unresolved,
/// for example because it was already released; nothing is released then. An id that is
/// still in flight is noted as seen, so that [`Inflight::finish`] does not count it as
/// unknown when the send comes back with no answer.
pub(crate) fn resolve_client_id(account: AccountKey, symbol: &str, client_id: &str) -> bool {
    with(|m| {
        let key = (account, symbol.to_string());
        let released = m.get_mut(&key).is_some_and(|s| {
            if s.ids.remove(client_id) {
                s.unknown = s.unknown.saturating_sub(1);
                true
            } else {
                if s.pending.contains(client_id) {
                    s.seen.insert(client_id.to_string());
                }
                false
            }
        });
        release(m, &key);
        released
    })
}

#[cfg(test)]
pub(crate) fn unlock_unknown(account: AccountKey, symbol: &str) {
    with(|m| {
        let key = (account, symbol.to_string());
        if let Some(s) = m.get_mut(&key) {
            s.unknown = s.unknown.saturating_sub(1);
        }
        release(m, &key);
    });
}

/// Mark an entry in flight, or refuse when the symbol is locked or already has one. The
/// entry's client id is registered now, before the send, so an order event that beats the
/// send's answer is not lost.
pub(crate) fn reserve(
    account: AccountKey,
    symbol: &str,
    client_id: Option<&str>,
) -> Result<Inflight, Locked> {
    with(|m| {
        let key = (account, symbol.to_string());
        let s = m.entry(key.clone()).or_default();
        if !s.idle() {
            return Err(Locked);
        }
        s.inflight += 1;
        if let Some(id) = client_id {
            s.pending.insert(id.to_string());
        }
        Ok(Inflight {
            key,
            client_id: client_id.map(str::to_string),
            armed: true,
        })
    })
}

/// An entry on its way to the exchange. If the future that owns it is dropped before the
/// send returned (cancelled), the order may be out there: it turns into an unresolved
/// `Unknown` that only a restart clears (fail closed).
pub(crate) struct Inflight {
    key: Key,
    client_id: Option<String>,
    armed: bool,
}

impl Inflight {
    /// The send returned. `unknown`: its outcome is not known, keep the symbol locked.
    pub(crate) fn finish(mut self, unknown: bool, client_id: Option<&str>) {
        self.armed = false;
        with(|m| {
            if let Some(s) = m.get_mut(&self.key) {
                s.inflight = s.inflight.saturating_sub(1);
                let seen = self.client_id.as_ref().is_some_and(|id| {
                    s.pending.remove(id);
                    s.seen.remove(id)
                });
                // Seen on the stream while in flight: the exchange has the order.
                if unknown && !seen {
                    s.unknown += 1;
                    if let Some(id) = client_id {
                        s.ids.insert(id.to_string());
                    }
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
                    if let Some(id) = &self.client_id {
                        s.pending.remove(id);
                        s.seen.remove(id);
                    }
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
        let first = reserve(a, "BTCUSDT", None).unwrap();
        assert_eq!(reserve(a, "BTCUSDT", None).err(), Some(Locked));
        assert!(reserve(a, "ETHUSDT", None).is_ok());
        first.finish(false, None);
        assert!(!is_locked(a, "BTCUSDT"));
        assert!(reserve(a, "BTCUSDT", None).is_ok());
    }

    #[test]
    fn the_lock_is_shared_by_account_not_by_client() {
        let a = key(202);
        reserve(a, "BTCUSDT", None).unwrap().finish(true, None);
        assert!(is_locked(a, "BTCUSDT"));
        assert!(!is_locked(key(203), "BTCUSDT"));
        unlock_unknown(a, "BTCUSDT");
        assert!(!is_locked(a, "BTCUSDT"));
    }

    #[test]
    fn a_dropped_send_stays_locked() {
        let a = key(204);
        drop(reserve(a, "BTCUSDT", None).unwrap());
        assert!(is_locked(a, "BTCUSDT"));
        unlock_unknown(a, "BTCUSDT");
        assert!(!is_locked(a, "BTCUSDT"));
    }

    #[test]
    fn a_client_id_releases_its_count_once() {
        let a = key(205);
        reserve(a, "BTCUSDT", None)
            .unwrap()
            .finish(true, Some("eabc"));
        lock_unknown(a, "BTCUSDT", Some("eother"));
        assert!(resolve_client_id(a, "BTCUSDT", "eabc"));
        // Already released: a second resolve, by the lookup or by the stream, does nothing.
        assert!(!resolve_client_id(a, "BTCUSDT", "eabc"));
        assert!(is_locked(a, "BTCUSDT"));
        assert!(!resolve_client_id(a, "BTCUSDT", "eunknown"));
        assert!(!resolve_client_id(a, "ETHUSDT", "eother"));
        assert!(resolve_client_id(a, "BTCUSDT", "eother"));
        assert!(!is_locked(a, "BTCUSDT"));
    }

    #[test]
    fn an_event_before_the_send_returns_does_not_unlock_later() {
        let a = key(206);
        // Not registered before the send: the early event finds nothing, the send's Unknown
        // locks, and only a later event or the lookup resolves it.
        let inflight = reserve(a, "BTCUSDT", None).unwrap();
        assert!(!resolve_client_id(a, "BTCUSDT", "eearly"));
        inflight.finish(true, Some("eearly"));
        assert!(is_locked(a, "BTCUSDT"));
        assert!(resolve_client_id(a, "BTCUSDT", "eearly"));
        assert!(!is_locked(a, "BTCUSDT"));
    }

    #[test]
    fn an_event_seen_while_in_flight_does_not_lock_on_unknown() {
        let a = key(207);
        let inflight = reserve(a, "BTCUSDT", Some("eseen")).unwrap();
        // The stream reports the order before the send returns.
        assert!(!resolve_client_id(a, "BTCUSDT", "eseen"));
        inflight.finish(true, Some("eseen"));
        assert!(!is_locked(a, "BTCUSDT"));
        // An id the stream did not report still locks.
        let inflight = reserve(a, "BTCUSDT", Some("eother")).unwrap();
        assert!(!resolve_client_id(a, "BTCUSDT", "eelse"));
        inflight.finish(true, Some("eother"));
        assert!(is_locked(a, "BTCUSDT"));
        assert!(resolve_client_id(a, "BTCUSDT", "eother"));
        assert!(!is_locked(a, "BTCUSDT"));
        // A seen id that is dropped half way leaves no stale id behind.
        drop(reserve(a, "ETHUSDT", Some("edrop")).unwrap());
        assert!(is_locked(a, "ETHUSDT"));
    }
}
