//! Client order ids (condition 59).
//!
//! One format satisfies every venue field: a role letter plus 20 random base62 characters
//! (about 119 bits), `[A-Za-z0-9]`, 21 characters. It holds no account id, time or counter,
//! so it leaks nothing and cannot repeat after a restart.

use super::types::TradeError;
use std::fmt;

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
/// 20 characters = 119 bits, above the 96 bit floor.
const RANDOM_LEN: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Entry,
    StopLoss,
    TakeProfit,
    Trailing,
}

impl Role {
    fn prefix(self) -> char {
        match self {
            Role::Entry => 'e',
            Role::StopLoss => 's',
            Role::TakeProfit => 't',
            Role::Trailing => 'r',
        }
    }
}

/// Which field the id goes into; each has its own pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdField {
    /// `newClientOrderId`, `clientAlgoId` on Binance USDⓈ-M: `^[.A-Z:/a-z0-9_-]{1,36}$`.
    BinanceFutures,
    /// `newClientOrderId`, `listClientOrderId`, `above/belowClientOrderId`,
    /// `working/pending*ClientOrderId` on spot: `^[a-zA-Z0-9-_]{1,36}$`.
    BinanceSpot,
    /// `clOrdId`, `algoClOrdId`, `attachAlgoClOrdId`: `^[A-Za-z0-9]{1,32}$`.
    Okx,
}

/// Check `id` against a venue field's pattern.
pub fn validate_client_id(id: &str, field: IdField) -> Result<(), TradeError> {
    let (max, allowed): (usize, fn(u8) -> bool) = match field {
        IdField::BinanceFutures => (36, |b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b':' | b'/' | b'_' | b'-')
        }),
        IdField::BinanceSpot => (36, |b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')
        }),
        IdField::Okx => (32, |b| b.is_ascii_alphanumeric()),
    };
    if !id.is_empty() && id.len() <= max && id.bytes().all(allowed) {
        Ok(())
    } else {
        Err(TradeError::ClientIdInvalid)
    }
}

/// A client id that already satisfies the strictest pattern (`[A-Za-z0-9]{1,32}`).
/// No `Default`, `From<String>` or `Deserialize`: the only ways in are [`ClientId::generate`]
/// and [`ClientId::parse`], both of which check.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ClientId(String);

impl ClientId {
    pub fn generate(role: Role) -> ClientId {
        ClientId::from_bytes(role, rand::random::<u8>)
    }

    /// Re-validate an id restored from storage or from the exchange.
    pub fn parse(s: &str) -> Result<ClientId, TradeError> {
        validate_client_id(s, IdField::Okx)?;
        Ok(ClientId(s.to_string()))
    }

    /// Sample base62 characters without modulo bias: bytes of 248 or more are discarded,
    /// because 248 = 4 * 62.
    fn from_bytes(role: Role, mut next_byte: impl FnMut() -> u8) -> ClientId {
        let mut id = String::with_capacity(1 + RANDOM_LEN);
        id.push(role.prefix());
        while id.len() < 1 + RANDOM_LEN {
            let b = next_byte();
            if b < 248 {
                id.push(BASE62[(b % 62) as usize] as char);
            }
        }
        ClientId(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Check the id against a field's pattern (the builders call this for every id field).
    pub fn check(&self, field: IdField) -> Result<&str, TradeError> {
        validate_client_id(&self.0, field)?;
        Ok(&self.0)
    }
}

// An id is not secret, but it ties a log line to an order: keep it out of `{:?}` output.
impl fmt::Debug for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClientId(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generated_ids_match_every_venue_pattern() {
        for role in [
            Role::Entry,
            Role::StopLoss,
            Role::TakeProfit,
            Role::Trailing,
        ] {
            for _ in 0..200 {
                let id = ClientId::generate(role);
                for field in [IdField::BinanceFutures, IdField::BinanceSpot, IdField::Okx] {
                    assert!(id.check(field).is_ok());
                }
                assert_eq!(id.as_str().len(), 21);
                assert!(id.as_str().bytes().all(|b| b.is_ascii_alphanumeric()));
            }
        }
    }

    #[test]
    fn prefix_names_the_role() {
        assert!(ClientId::generate(Role::Entry).as_str().starts_with('e'));
        assert!(ClientId::generate(Role::StopLoss).as_str().starts_with('s'));
        assert!(
            ClientId::generate(Role::TakeProfit)
                .as_str()
                .starts_with('t')
        );
        assert!(ClientId::generate(Role::Trailing).as_str().starts_with('r'));
    }

    #[test]
    fn has_at_least_96_bits_of_randomness() {
        // 62^17 > 2^96, and the random part is 20 characters.
        assert!(62f64.powi(RANDOM_LEN as i32) > 2f64.powi(96));
        let ids: HashSet<String> = (0..2000)
            .map(|_| ClientId::generate(Role::Entry).as_str().to_string())
            .collect();
        assert_eq!(ids.len(), 2000);
    }

    #[test]
    fn sampling_discards_biased_bytes() {
        // 248..=255 would favour the first eight characters; they must be skipped.
        let mut bytes = [255u8, 248, 0, 61, 62, 247].into_iter().cycle();
        let id = ClientId::from_bytes(Role::Entry, || bytes.next().unwrap());
        // First usable bytes: 0, 61, 62(->0), 247(->61), then the cycle repeats.
        assert!(id.as_str().starts_with("e0z0z"));
    }

    #[test]
    fn patterns_per_field() {
        assert!(validate_client_id("a.b:c/d_e-f", IdField::BinanceFutures).is_ok());
        assert!(validate_client_id("a.b", IdField::BinanceSpot).is_err());
        assert!(validate_client_id("a-b_c", IdField::BinanceSpot).is_ok());
        assert!(validate_client_id("a-b", IdField::Okx).is_err());
        assert!(validate_client_id(&"a".repeat(36), IdField::BinanceSpot).is_ok());
        assert!(validate_client_id(&"a".repeat(37), IdField::BinanceSpot).is_err());
        assert!(validate_client_id(&"a".repeat(32), IdField::Okx).is_ok());
        assert!(validate_client_id(&"a".repeat(33), IdField::Okx).is_err());
        assert!(validate_client_id("", IdField::Okx).is_err());
        assert!(validate_client_id("a&b=c", IdField::BinanceFutures).is_err());
    }

    #[test]
    fn parse_only_accepts_the_strict_pattern() {
        assert!(ClientId::parse("eAbc123").is_ok());
        assert!(ClientId::parse("e-abc").is_err());
        assert!(ClientId::parse("").is_err());
        assert!(ClientId::parse(&"x".repeat(33)).is_err());
    }

    #[test]
    fn one_hundred_thousand_ids_are_all_different() {
        const N: usize = 100_000;
        let ids: HashSet<String> = (0..N)
            .map(|_| ClientId::generate(Role::Entry).as_str().to_string())
            .collect();
        assert_eq!(ids.len(), N);
    }
}
