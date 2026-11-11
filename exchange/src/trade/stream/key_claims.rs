//! One private stream per exchange account (review b-2 item 22, in depth).
//!
//! The same API key bound to two app accounts would share one Binance listenKey and split the
//! per-account locks and limits in two. The settings form refuses such a binding; this is the
//! second line: a process-wide table of the SHA-256 digests of the keys that streams are
//! using, so a second app account cannot open a stream on a key another account holds.
//!
//! The digest lives in memory only. It is never logged, printed (`Debug` is hand-written),
//! saved or sent anywhere, and it is not salted because it is never stored.

use crate::account::AccountKey;
use ring::digest::{SHA256, digest};
use std::collections::HashMap;
use std::fmt;
use std::sync::{LazyLock, Mutex, PoisonError};

type Digest = [u8; 32];

static CLAIMS: LazyLock<Mutex<HashMap<Digest, Owner>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Copy)]
struct Owner {
    account: AccountKey,
    token: u64,
}

fn with<T>(f: impl FnOnce(&mut HashMap<Digest, Owner>) -> T) -> T {
    f(&mut CLAIMS.lock().unwrap_or_else(PoisonError::into_inner))
}

fn digest_of(api_key: &str) -> Digest {
    let mut out = [0u8; 32];
    out.copy_from_slice(digest(&SHA256, api_key.as_bytes()).as_ref());
    out
}

/// The claim of one stream on one key. Dropping it gives the key back.
pub(crate) struct KeyClaim {
    digest: Digest,
    token: u64,
}

impl fmt::Debug for KeyClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyClaim(<redacted>)")
    }
}

impl Drop for KeyClaim {
    fn drop(&mut self) {
        with(|m| {
            if m.get(&self.digest).is_some_and(|o| o.token == self.token) {
                m.remove(&self.digest);
            }
        });
    }
}

/// Why a claim was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// A stream of another account (or an older stream of this one) holds the key. May mend, so
    /// the caller retries with a backoff.
    Held,
    /// A newer stream of the same account holds the key: this stream was replaced and must end.
    Superseded,
}

/// Claim `api_key` for the stream `token` of `account`. `None` when a stream of another
/// account already holds the same key, or a newer stream of the same account does. A stream of
/// the same account (a replacement that started before the old one ended) takes the claim over
/// from an older one, never the other way round.
#[cfg(test)] // only the tests ask for a bare claim; the stream goes through `claim_into`
pub(crate) fn claim(account: AccountKey, token: u64, api_key: &str) -> Option<KeyClaim> {
    try_claim(account, token, api_key).ok()
}

fn try_claim(account: AccountKey, token: u64, api_key: &str) -> Result<KeyClaim, Refused> {
    let digest = digest_of(api_key);
    with(|m| match m.get(&digest) {
        Some(o) if o.account != account => Err(Refused::Held),
        Some(o) if o.token > token => Err(Refused::Superseded),
        _ => {
            m.insert(digest, Owner { account, token });
            Ok(KeyClaim { digest, token })
        }
    })
}

/// Claim `api_key` into `slot` (the stream's one held claim) without ever giving the key back
/// first. A reconnect re-claims the same key; releasing before claiming would let another
/// account take the key in between. The old claim is dropped only after the new one is held
/// (a no-op when it is the same key, whose claim stays as it is). On a refusal the slot is left
/// unchanged.
pub(crate) fn claim_into(
    slot: &mut Option<KeyClaim>,
    account: AccountKey,
    token: u64,
    api_key: &str,
) -> Result<(), Refused> {
    let wanted = digest_of(api_key);
    if slot
        .as_ref()
        .is_some_and(|c| c.digest == wanted && c.token == token)
    {
        // Re-check under the lock: the entry must still be ours. A same-account entry with
        // another token (a newer stream took over) is left as it is: writing our token back
        // would make our drop delete the newer stream's entry.
        return with(|m| match m.get(&wanted) {
            Some(o) if o.account != account => Err(Refused::Held),
            Some(o) if o.token > token => Err(Refused::Superseded),
            Some(o) if o.token < token => Err(Refused::Held),
            Some(_) => Ok(()),
            None => {
                m.insert(wanted, Owner { account, token });
                Ok(())
            }
        });
    }
    // Dropping the old claim (another key, or an older token) removes only its own entry; the
    // new one was inserted first.
    *slot = Some(try_claim(account, token, api_key)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(n: u8) -> AccountKey {
        AccountKey::from_bytes([n; 16])
    }

    #[test]
    fn a_second_account_cannot_take_a_key_that_is_in_use_until_it_is_released() {
        let key = "claims-test-key-1";
        let first = claim(acct(1), 1, key).expect("free key");
        assert!(claim(acct(2), 2, key).is_none());
        // Another key is unaffected.
        assert!(claim(acct(2), 2, "claims-test-key-2").is_some());
        drop(first);
        assert!(claim(acct(2), 2, key).is_some());
    }

    #[test]
    fn the_same_account_may_take_over_and_the_old_claim_cannot_release_the_new_one() {
        let key = "claims-test-key-3";
        let old = claim(acct(3), 10, key).expect("free key");
        let new = claim(acct(3), 11, key).expect("same account");
        drop(old);
        // The newer claim still holds the key.
        assert!(claim(acct(4), 12, key).is_none());
        drop(new);
        assert!(claim(acct(4), 12, key).is_some());
    }

    #[test]
    fn debug_shows_nothing_derived_from_the_key() {
        let c = claim(acct(5), 1, "claims-test-key-4").expect("free key");
        assert_eq!(format!("{c:?}"), "KeyClaim(<redacted>)");
    }

    #[test]
    fn a_reclaim_of_the_same_key_never_leaves_a_gap() {
        let key = "claims-test-key-5";
        let mut slot = None;
        assert!(claim_into(&mut slot, acct(6), 1, key).is_ok());
        // Re-claiming the same key keeps it held; another account is still refused.
        assert!(claim_into(&mut slot, acct(6), 1, key).is_ok());
        assert!(claim(acct(7), 2, key).is_none());
        // A replaced key is released only after the new one is held.
        assert!(claim_into(&mut slot, acct(6), 1, "claims-test-key-6").is_ok());
        assert!(claim(acct(7), 2, key).is_some());
        assert!(claim(acct(7), 3, "claims-test-key-6").is_none());
    }

    #[test]
    fn a_reclaim_does_not_take_back_a_key_a_newer_stream_of_the_same_account_holds() {
        let key = "claims-test-key-9";
        let mut slot = None;
        assert!(claim_into(&mut slot, acct(11), 1, key).is_ok());
        // A replacement stream of the same account takes the key over.
        let newer = claim(acct(11), 2, key).expect("same account");
        // The old stream reconnecting does not overwrite the entry.
        assert_eq!(
            claim_into(&mut slot, acct(11), 1, key),
            Err(Refused::Superseded)
        );
        // Dropping the old claim leaves the newer entry in place.
        drop(slot);
        assert!(claim(acct(12), 3, key).is_none());
        drop(newer);
        assert!(claim(acct(12), 3, key).is_some());
    }

    #[test]
    fn a_refused_reclaim_leaves_the_slot_unchanged() {
        let other = claim(acct(8), 1, "claims-test-key-7").expect("free key");
        let mut slot = None;
        assert!(claim_into(&mut slot, acct(9), 1, "claims-test-key-8").is_ok());
        assert_eq!(
            claim_into(&mut slot, acct(9), 1, "claims-test-key-7"),
            Err(Refused::Held)
        );
        // The old key is still held by the slot.
        assert!(claim(acct(10), 2, "claims-test-key-8").is_none());
        drop(other);
    }

    #[test]
    fn an_older_stream_arriving_late_cannot_overwrite_a_newer_claim() {
        let key = "claims-test-key-10";
        let newer = claim(acct(13), 5, key).expect("free key");
        // The old stream (lower token) with an empty slot, or after its key was replaced.
        assert!(claim(acct(13), 4, key).is_none());
        let mut slot = None;
        assert_eq!(
            claim_into(&mut slot, acct(13), 4, key),
            Err(Refused::Superseded)
        );
        assert!(slot.is_none());
        let mut other_key = None;
        assert!(claim_into(&mut other_key, acct(13), 4, "claims-test-key-11").is_ok());
        assert_eq!(
            claim_into(&mut other_key, acct(13), 4, key),
            Err(Refused::Superseded)
        );
        // The newer entry is intact: another account is still refused.
        assert!(claim(acct(14), 6, key).is_none());
        drop(newer);
        assert!(claim(acct(14), 6, key).is_some());
    }

    #[test]
    fn another_account_is_held_not_superseded() {
        let key = "claims-test-key-12";
        let _held = claim(acct(15), 9, key).expect("free key");
        let mut slot = None;
        assert_eq!(claim_into(&mut slot, acct(16), 1, key), Err(Refused::Held));
    }
}
