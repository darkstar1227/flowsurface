//! The two identities a private stream deals in: the listenKey (a secret) and the key under
//! which the app tells streams apart (never the secret).

use crate::account::{AccountEnv, AccountKey, AccountProduct, AccountVenue};
use secrecy::{ExposeSecret, SecretString};
use std::fmt;

const MAX_LISTEN_KEY_LEN: usize = 256;

/// A Binance futures listenKey. Whoever holds it can read the account's orders and positions
/// until it expires, so it is a secret (condition 45): masked `Debug`, no `Display`, no
/// `Serialize`, no `Clone`, no `Hash`/`Eq`. It lives in the stream task only, and is never
/// placed in a log line, an error, a `Message` or a subscription identity.
pub struct ListenKey(SecretString);

impl ListenKey {
    /// Accepts the key the exchange returned: 1 to 256 ASCII letters and digits.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let ok = !raw.is_empty()
            && raw.len() <= MAX_LISTEN_KEY_LEN
            && raw.bytes().all(|b| b.is_ascii_alphanumeric());
        ok.then(|| Self(SecretString::from(raw.to_string())))
    }

    pub(crate) fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for ListenKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ListenKey(***)")
    }
}

/// Identifies one account's private stream: (account, venue, environment, product,
/// generation). The generation is bumped when the account is edited or re-enabled, so the
/// old task is replaced. This is what an iced subscription hashes; it holds no secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamKey {
    pub account: AccountKey,
    pub venue: AccountVenue,
    pub env: AccountEnv,
    pub product: AccountProduct,
    pub generation: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_key_is_masked_everywhere_it_can_be_printed() {
        let key =
            ListenKey::parse("pqia91ma19a5s61cv6a81va65sdf19v8a65a1a5s61cv6a81va65sdf19v8a65a1")
                .unwrap();
        assert_eq!(format!("{key:?}"), "ListenKey(***)");
        assert_eq!(format!("{key:#?}"), "ListenKey(***)");
        // Wrapping types keep the mask.
        assert!(!format!("{:?}", Some(&key)).contains("pqia91"));
    }

    #[test]
    fn listen_key_rejects_what_the_exchange_does_not_send() {
        assert!(ListenKey::parse("").is_none());
        assert!(ListenKey::parse("a b").is_none());
        assert!(ListenKey::parse("a/b").is_none());
        assert!(ListenKey::parse("a?b=c").is_none());
        assert!(ListenKey::parse(&"a".repeat(257)).is_none());
        assert!(ListenKey::parse(&"a".repeat(256)).is_some());
    }

    #[test]
    fn stream_key_identity_has_no_secret_and_tells_generations_apart() {
        let mk = |generation| StreamKey {
            account: AccountKey::from_bytes([7; 16]),
            venue: AccountVenue::Binance,
            env: AccountEnv::Testnet,
            product: AccountProduct::UsdM,
            generation,
        };
        assert_ne!(mk(1), mk(2));
        assert_eq!(mk(1), mk(1));
        let text = format!("{:?}", mk(1));
        assert!(text.contains("generation"));
    }
}
