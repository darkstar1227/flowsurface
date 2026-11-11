//! Exchange accounts: non-secret metadata (persisted in `saved-state.json`) and the
//! Keychain store for the credentials.
//!
//! Nothing secret is ever part of [`AccountMeta`]. Credentials live in one Keychain item
//! per account (service `flowsurface.exchange`, account `<venue>:<uuid>`) holding a JSON
//! payload, so deleting an account is a single atomic `delete_credential`.

use exchange::account::{AccountEnv, AccountProduct, AccountVenue, ApiCredentials, PositionMode};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

const KEYCHAIN_SERVICE: &str = "flowsurface.exchange";
const PAYLOAD_VERSION: u8 = 1;

/// Non-secret account settings. Venue, environment and product are fixed after creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AccountMeta {
    pub id: Uuid,
    pub alias: String,
    pub venue: AccountVenue,
    pub env: AccountEnv,
    /// Binance product (spot or USDⓈ-M). Missing in accounts saved before M5, which only
    /// supported futures, so the default is USDⓈ-M. OKX ignores it and keeps the default.
    pub product: AccountProduct,
    /// Position mode the user expects the exchange account to be in.
    pub position_mode: PositionMode,
    /// Local risk limits and the automatic-close switch (off by default). Settings only; the
    /// equity peak and lock state are never saved.
    pub risk: super::risk::AccountRiskSettings,
}

impl AccountMeta {
    pub fn new(
        alias: String,
        venue: AccountVenue,
        env: AccountEnv,
        product: AccountProduct,
        position_mode: PositionMode,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            alias,
            venue,
            env,
            // One OKX key serves every product, so only Binance accounts carry one.
            product: if venue == AccountVenue::Binance {
                product
            } else {
                AccountProduct::default()
            },
            position_mode,
            risk: Default::default(),
        }
    }

    /// The (venue, env, product) triple that decides which hosts the key may reach.
    pub fn identity(&self) -> StoredIdentity {
        StoredIdentity {
            venue: self.venue,
            env: self.env,
            product: self.product,
        }
    }

    fn keychain_account(&self) -> String {
        format!("{}:{}", self.venue.as_str(), self.id)
    }
}

/// The persisted account list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ExchangeAccounts {
    pub accounts: Vec<AccountMeta>,
}

impl ExchangeAccounts {
    /// Drop entries with a nil id (a missing `id` deserialises to nil) and later entries
    /// repeating an id, so no two accounts can share one Keychain item.
    pub fn sanitized(mut self) -> Self {
        let mut seen = std::collections::HashSet::new();
        self.accounts
            .retain(|a| !a.id.is_nil() && seen.insert(a.id));
        self
    }
}

/// What the Keychain item says the key belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredIdentity {
    pub venue: AccountVenue,
    pub env: AccountEnv,
    pub product: AccountProduct,
}

// ── Keychain store ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// No Keychain item exists for the account.
    NoEntry,
    /// The Keychain item is not in the expected format.
    InvalidFormat,
    /// The Keychain item's venue, environment or product differs from the account's.
    Mismatch,
    /// Keychain access failed (locked, denied, unavailable...). Display text only.
    Keychain(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::NoEntry => f.write_str("credentials missing from the Keychain"),
            StoreError::InvalidFormat => f.write_str("Keychain item has an invalid format"),
            StoreError::Mismatch => {
                f.write_str("Keychain item does not match the account's exchange or environment")
            }
            StoreError::Keychain(e) => write!(f, "Keychain error: {e}"),
        }
    }
}

fn map_keyring(e: keyring::Error) -> StoreError {
    match e {
        keyring::Error::NoEntry => StoreError::NoEntry,
        // Display only: the Debug of `BadEncoding` would print the raw bytes.
        other => StoreError::Keychain(other.to_string()),
    }
}

/// JSON stored in the Keychain item. Wiped on drop.
#[derive(Serialize, Deserialize)]
struct Payload {
    v: u8,
    venue: AccountVenue,
    env: AccountEnv,
    /// Missing in M4 items, which were futures-only.
    #[serde(default)]
    product: AccountProduct,
    key: String,
    secret: String,
    #[serde(default)]
    passphrase: Option<String>,
}

impl Drop for Payload {
    fn drop(&mut self) {
        self.key.zeroize();
        self.secret.zeroize();
        self.passphrase.zeroize();
    }
}

fn encode(meta: &AccountMeta, creds: &ApiCredentials) -> Result<Zeroizing<String>, StoreError> {
    let payload = Payload {
        v: PAYLOAD_VERSION,
        venue: meta.venue,
        env: meta.env,
        product: meta.product,
        key: creds.key().expose_secret().to_string(),
        secret: creds.secret().expose_secret().to_string(),
        passphrase: creds.passphrase().map(|p| p.expose_secret().to_string()),
    };
    serde_json::to_string(&payload)
        .map(Zeroizing::new)
        .map_err(|_| StoreError::InvalidFormat)
}

fn decode(meta: &AccountMeta, raw: &str) -> Result<ApiCredentials, StoreError> {
    decode_with_identity(meta, raw).map(|(creds, _)| creds)
}

fn decode_with_identity(
    meta: &AccountMeta,
    raw: &str,
) -> Result<(ApiCredentials, StoredIdentity), StoreError> {
    // The serde error text can quote parts of the input, so it is dropped.
    let mut payload: Payload = serde_json::from_str(raw).map_err(|_| StoreError::InvalidFormat)?;
    if payload.v != PAYLOAD_VERSION {
        return Err(StoreError::InvalidFormat);
    }
    let stored = StoredIdentity {
        venue: payload.venue,
        env: payload.env,
        product: payload.product,
    };
    if stored != meta.identity() {
        return Err(StoreError::Mismatch);
    }
    Ok((
        ApiCredentials::new(
            std::mem::take(&mut payload.key),
            std::mem::take(&mut payload.secret),
            payload.passphrase.take(),
        ),
        stored,
    ))
}

fn entry(meta: &AccountMeta) -> Result<keyring::Entry, StoreError> {
    keyring::Entry::new(KEYCHAIN_SERVICE, &meta.keychain_account()).map_err(map_keyring)
}

fn read_raw(entry: &keyring::Entry) -> Result<Zeroizing<String>, StoreError> {
    entry
        .get_password()
        .map(Zeroizing::new)
        .map_err(map_keyring)
}

/// Write the credentials, then read them back and compare. On any failure the new item
/// is removed again, so no half-saved account is left behind.
pub fn save_credentials(meta: &AccountMeta, creds: &ApiCredentials) -> Result<(), StoreError> {
    let entry = entry(meta)?;
    let raw = encode(meta, creds)?;
    entry.set_password(&raw).map_err(map_keyring)?;

    let verified = read_raw(&entry)
        .and_then(|stored| decode(meta, &stored))
        .is_ok_and(|stored| stored.same_as(creds));
    if verified {
        Ok(())
    } else {
        let message = if entry.delete_credential().is_ok() {
            "the saved item could not be read back".to_string()
        } else {
            // No keyring error detail on purpose; the user has to clean up by hand.
            format!(
                "the saved item could not be read back and could not be removed; \
                 delete the '{KEYCHAIN_SERVICE}' item in Keychain Access manually"
            )
        };
        Err(StoreError::Keychain(message))
    }
}

pub fn load_credentials(meta: &AccountMeta) -> Result<ApiCredentials, StoreError> {
    let raw = read_raw(&entry(meta)?)?;
    decode(meta, &raw)
}

/// [`load_credentials`] plus what the Keychain item says it belongs to, for the send gate:
/// it compares that against the saved account on every order.
pub fn load_credentials_with_identity(
    meta: &AccountMeta,
) -> Result<(ApiCredentials, StoredIdentity), StoreError> {
    let raw = read_raw(&entry(meta)?)?;
    decode_with_identity(meta, &raw)
}

// ── One exchange account, one app account ───────────────────────────────

/// Why a key may not be bound to an account. Neither case carries the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DuplicateKeyError {
    /// Another account of the same venue, environment and product already uses this key.
    /// Only its alias is named.
    AlreadyBound { alias: String },
    /// Another account's Keychain item could not be read, so the check could not be made.
    /// Refused rather than allowed: try again once the Keychain is available.
    CannotVerify,
}

impl fmt::Display for DuplicateKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DuplicateKeyError::AlreadyBound { alias } => write!(
                f,
                "this API key is already used by the account \"{alias}\" \
                 (one exchange account must not be bound twice: locks and limits would be split)"
            ),
            DuplicateKeyError::CannotVerify => f.write_str(
                "could not check the other accounts' keys (Keychain unavailable); \
                 nothing was saved, try again",
            ),
        }
    }
}

/// Refuse binding a key that another account of the same venue, environment and product
/// already uses (one exchange account bound twice would split the unresolved-send locks and
/// the risk limits in two). Reads the other accounts' Keychain items and compares the keys in
/// memory only: nothing derived from a key is saved or logged, and the credentials read are
/// dropped (zeroized) at once. The secret is not compared; the key identifies the account.
///
/// A Binance key bound to both Spot and USDⓈ-M is a legitimate use and passes (`product`
/// differs); the UI should say that the two accounts share one exchange account. For OKX the
/// product is fixed, so the same key in the same environment is always refused.
pub fn check_key_not_bound(
    others: &[AccountMeta],
    candidate: &AccountMeta,
    creds: &ApiCredentials,
) -> Result<(), DuplicateKeyError> {
    check_key_not_bound_with(others, candidate, creds, load_credentials)
}

/// [`check_key_not_bound`] with the Keychain read injected (for tests).
pub(crate) fn check_key_not_bound_with(
    others: &[AccountMeta],
    candidate: &AccountMeta,
    creds: &ApiCredentials,
    load: impl Fn(&AccountMeta) -> Result<ApiCredentials, StoreError>,
) -> Result<(), DuplicateKeyError> {
    for other in others.iter().filter(|o| {
        o.id != candidate.id
            && o.venue == candidate.venue
            && o.env == candidate.env
            && o.product == candidate.product
    }) {
        match load(other) {
            Ok(theirs) if theirs.same_key_as(creds) => {
                return Err(DuplicateKeyError::AlreadyBound {
                    alias: other.alias.clone(),
                });
            }
            Ok(_) => {}
            // That account cannot trade anyway: nothing to collide with.
            Err(StoreError::NoEntry) => {}
            Err(StoreError::InvalidFormat | StoreError::Mismatch) => {
                log::warn!("an account's Keychain item is not valid; skipped by the key check");
            }
            Err(StoreError::Keychain(_)) => return Err(DuplicateKeyError::CannotVerify),
        }
    }
    Ok(())
}

/// A missing item counts as deleted; any other error is returned.
pub fn delete_credentials(meta: &AccountMeta) -> Result<(), StoreError> {
    match entry(meta)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(map_keyring(e)),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_account_saved_before_risk_settings_loads_with_automatic_close_off() {
        let old = r#"{"id":"00000000-0000-0000-0000-000000000009","alias":"a","venue":"Okx","env":"Testnet"}"#;
        let m: AccountMeta = serde_json::from_str(old).expect("old account loads");
        assert_eq!(m.risk, crate::config::risk::AccountRiskSettings::default());
        assert!(!m.risk.auto_close);
        // The new field is settings only: no secret-looking content in the JSON.
        let json = serde_json::to_string(&m).expect("json");
        assert!(json.contains("\"auto_close\":false"));
    }

    use super::*;

    const FAKE_KEY: &str = "fake-key-AAAA1111";
    const FAKE_SECRET: &str = "fake-secret-BBBB2222";
    const FAKE_PASS: &str = "fake-pass-CCCC3333";

    fn meta(venue: AccountVenue, env: AccountEnv) -> AccountMeta {
        AccountMeta::new(
            "main".into(),
            venue,
            env,
            AccountProduct::UsdM,
            PositionMode::Hedge,
        )
    }

    fn creds() -> ApiCredentials {
        ApiCredentials::new(FAKE_KEY.into(), FAKE_SECRET.into(), Some(FAKE_PASS.into()))
    }

    #[test]
    fn keychain_account_name_uses_venue_and_uuid_not_alias() {
        let m = meta(AccountVenue::Okx, AccountEnv::Live);
        assert_eq!(m.keychain_account(), format!("okx:{}", m.id));
        assert!(!m.keychain_account().contains("main"));
    }

    #[test]
    fn payload_roundtrip() {
        let m = meta(AccountVenue::Okx, AccountEnv::Testnet);
        let raw = encode(&m, &creds()).unwrap();
        assert!(decode(&m, &raw).unwrap().same_as(&creds()));
    }

    #[test]
    fn payload_venue_or_env_mismatch_is_refused() {
        let m = meta(AccountVenue::Okx, AccountEnv::Testnet);
        let raw = encode(&m, &creds()).unwrap();
        for other in [
            meta(AccountVenue::Binance, AccountEnv::Testnet),
            meta(AccountVenue::Okx, AccountEnv::Live),
        ] {
            assert_eq!(decode(&other, &raw).unwrap_err(), StoreError::Mismatch);
        }
    }

    #[test]
    fn invalid_payload_error_does_not_quote_input() {
        let m = meta(AccountVenue::Binance, AccountEnv::Live);
        let raw =
            format!(r#"{{"v":1,"venue":"Binance","env":"Live","key":"{FAKE_KEY}","secret":5}}"#);
        let err = decode(&m, &raw).unwrap_err();
        assert_eq!(err, StoreError::InvalidFormat);
        let shown = format!("{err} {err:?}");
        assert!(!shown.contains(FAKE_KEY));
        assert_eq!(
            decode(&m, "not json").unwrap_err(),
            StoreError::InvalidFormat
        );
    }

    #[test]
    fn debug_output_has_no_secrets() {
        let m = meta(AccountVenue::Okx, AccountEnv::Live);
        let all = format!(
            "{m:?} {:?} {:?}",
            creds(),
            ExchangeAccounts {
                accounts: vec![m.clone()]
            }
        );
        for s in [FAKE_KEY, FAKE_SECRET, FAKE_PASS] {
            assert!(!all.contains(s));
        }
    }

    #[test]
    fn saved_state_json_has_no_credentials() {
        let m = meta(AccountVenue::Okx, AccountEnv::Live);
        // Credentials exist in memory while the state is serialised; they must not leak in.
        let _creds = creds();
        let state = crate::State {
            exchange_accounts: ExchangeAccounts { accounts: vec![m] },
            ..crate::State::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        for s in [FAKE_KEY, FAKE_SECRET, FAKE_PASS] {
            assert!(!json.contains(s));
        }
        assert!(json.contains("exchange_accounts"));
    }

    #[test]
    fn old_state_without_accounts_still_loads() {
        let state: crate::State = serde_json::from_str("{}").unwrap();
        assert!(state.exchange_accounts.accounts.is_empty());
        let accounts: ExchangeAccounts = serde_json::from_str("{}").unwrap();
        assert!(accounts.accounts.is_empty());
    }

    #[test]
    fn sanitized_drops_nil_and_duplicate_ids() {
        let a = meta(AccountVenue::Binance, AccountEnv::Live);
        let b = meta(AccountVenue::Okx, AccountEnv::Live);
        let nil: AccountMeta = serde_json::from_str(r#"{"alias":"x"}"#).unwrap();
        assert!(nil.id.is_nil());
        let dup = AccountMeta {
            alias: "dup".into(),
            ..a.clone()
        };
        let all = ExchangeAccounts {
            accounts: vec![a.clone(), nil.clone(), b.clone(), dup, nil],
        }
        .sanitized();
        assert_eq!(all.accounts, vec![a, b]);
    }

    #[test]
    fn m4_account_without_product_loads_as_usdm() {
        let m: AccountMeta = serde_json::from_str(
            r#"{"id":"6f1c2c8e-1b9a-4a5e-9d57-0f3b2f6a1c11","alias":"old","venue":"Binance","env":"Testnet","position_mode":"Hedge"}"#,
        )
        .unwrap();
        assert_eq!(m.product, AccountProduct::UsdM);
        assert_eq!(m.env, AccountEnv::Testnet);
    }

    #[test]
    fn m4_keychain_item_without_product_is_a_usdm_item() {
        let m = meta(AccountVenue::Binance, AccountEnv::Testnet);
        let old = format!(
            r#"{{"v":1,"venue":"Binance","env":"Testnet","key":"{FAKE_KEY}","secret":"{FAKE_SECRET}"}}"#
        );
        let (creds, id) = decode_with_identity(&m, &old).unwrap();
        assert_eq!(id.product, AccountProduct::UsdM);
        assert_eq!(creds.key().expose_secret(), FAKE_KEY);
        // An account that now says Spot must not accept that old (futures) item.
        let spot = AccountMeta {
            product: AccountProduct::Spot,
            ..m
        };
        assert_eq!(decode(&spot, &old).unwrap_err(), StoreError::Mismatch);
    }

    #[test]
    fn keychain_product_must_match_the_saved_product() {
        let spot = AccountMeta::new(
            "s".into(),
            AccountVenue::Binance,
            AccountEnv::Testnet,
            AccountProduct::Spot,
            PositionMode::OneWay,
        );
        let raw = encode(&spot, &creds()).unwrap();
        assert_eq!(
            decode_with_identity(&spot, &raw).unwrap().1.product,
            AccountProduct::Spot
        );
        // The saved state was edited to say USDⓈ-M: the item is refused.
        let edited = AccountMeta {
            product: AccountProduct::UsdM,
            ..spot
        };
        assert_eq!(decode(&edited, &raw).unwrap_err(), StoreError::Mismatch);
    }

    #[test]
    fn okx_accounts_ignore_the_product() {
        let m = AccountMeta::new(
            "o".into(),
            AccountVenue::Okx,
            AccountEnv::Live,
            AccountProduct::Spot,
            PositionMode::OneWay,
        );
        assert_eq!(m.product, AccountProduct::UsdM);
    }

    #[test]
    fn account_meta_tolerates_missing_fields() {
        let m: AccountMeta = serde_json::from_str(r#"{"alias":"a"}"#).unwrap();
        assert_eq!(m.alias, "a");
        assert_eq!(m.venue, AccountVenue::Binance);
        assert_eq!(m.env, AccountEnv::Live);
        assert_eq!(m.position_mode, PositionMode::OneWay);
    }

    // ── Duplicate key check ─────────────────────────────────────────────

    fn account(
        venue: AccountVenue,
        env: AccountEnv,
        product: AccountProduct,
        alias: &str,
    ) -> AccountMeta {
        AccountMeta::new(alias.into(), venue, env, product, PositionMode::Hedge)
    }

    fn with_key(key: &str) -> ApiCredentials {
        ApiCredentials::new(key.into(), "secret".into(), None)
    }

    /// The Keychain of the test: the key each account id holds.
    fn keychain(
        entries: Vec<(Uuid, Result<&'static str, StoreError>)>,
    ) -> impl Fn(&AccountMeta) -> Result<ApiCredentials, StoreError> {
        move |m| match entries.iter().find(|(id, _)| *id == m.id) {
            Some((_, Ok(key))) => Ok(with_key(key)),
            Some((_, Err(e))) => Err(e.clone()),
            None => Err(StoreError::NoEntry),
        }
    }

    #[test]
    fn the_same_key_in_the_same_venue_env_and_product_is_refused_by_alias_only() {
        let first = account(
            AccountVenue::Okx,
            AccountEnv::Live,
            AccountProduct::UsdM,
            "main",
        );
        let new = account(
            AccountVenue::Okx,
            AccountEnv::Live,
            AccountProduct::UsdM,
            "again",
        );
        let load = keychain(vec![(first.id, Ok("KEY-ONE-1234"))]);
        let err =
            check_key_not_bound_with(&[first], &new, &with_key("KEY-ONE-1234"), load).unwrap_err();
        assert_eq!(
            err,
            DuplicateKeyError::AlreadyBound {
                alias: "main".into()
            }
        );
        for text in [format!("{err}"), format!("{err:?}")] {
            assert!(!text.contains("KEY-ONE-1234"), "{text}");
        }
    }

    #[test]
    fn a_different_key_the_account_itself_or_another_environment_passes() {
        let live = account(
            AccountVenue::Okx,
            AccountEnv::Live,
            AccountProduct::UsdM,
            "live",
        );
        let test = account(
            AccountVenue::Okx,
            AccountEnv::Testnet,
            AccountProduct::UsdM,
            "test",
        );
        let load = || {
            keychain(vec![
                (live.id, Ok("SAME-KEY-9999")),
                (test.id, Ok("SAME-KEY-9999")),
            ])
        };
        // Another key.
        assert!(
            check_key_not_bound_with(
                std::slice::from_ref(&live),
                &test,
                &with_key("OTHER"),
                load()
            )
            .is_ok()
        );
        // The same key under a testnet account is a different exchange account.
        let new_test = account(
            AccountVenue::Okx,
            AccountEnv::Testnet,
            AccountProduct::UsdM,
            "t2",
        );
        assert!(
            check_key_not_bound_with(
                std::slice::from_ref(&live),
                &new_test,
                &with_key("SAME-KEY-9999"),
                load()
            )
            .is_ok()
        );
        // Re-saving an account's own key is not a duplicate of itself.
        assert!(
            check_key_not_bound_with(
                std::slice::from_ref(&live),
                &live,
                &with_key("SAME-KEY-9999"),
                load()
            )
            .is_ok()
        );
    }

    #[test]
    fn a_binance_key_may_serve_spot_and_usdm_but_not_twice_in_one_product() {
        let usdm = account(
            AccountVenue::Binance,
            AccountEnv::Live,
            AccountProduct::UsdM,
            "fut",
        );
        let spot = account(
            AccountVenue::Binance,
            AccountEnv::Live,
            AccountProduct::Spot,
            "spot",
        );
        let load = || keychain(vec![(usdm.id, Ok("BN-KEY-5555"))]);
        assert!(
            check_key_not_bound_with(
                std::slice::from_ref(&usdm),
                &spot,
                &with_key("BN-KEY-5555"),
                load()
            )
            .is_ok()
        );
        let usdm2 = account(
            AccountVenue::Binance,
            AccountEnv::Live,
            AccountProduct::UsdM,
            "fut2",
        );
        assert_eq!(
            check_key_not_bound_with(
                std::slice::from_ref(&usdm),
                &usdm2,
                &with_key("BN-KEY-5555"),
                load()
            ),
            Err(DuplicateKeyError::AlreadyBound {
                alias: "fut".into()
            })
        );
    }

    #[test]
    fn an_unreadable_keychain_refuses_and_a_missing_item_is_skipped() {
        let a = account(
            AccountVenue::Okx,
            AccountEnv::Live,
            AccountProduct::UsdM,
            "a",
        );
        let b = account(
            AccountVenue::Okx,
            AccountEnv::Live,
            AccountProduct::UsdM,
            "b",
        );
        let new = account(
            AccountVenue::Okx,
            AccountEnv::Live,
            AccountProduct::UsdM,
            "new",
        );
        let broken = keychain(vec![(a.id, Err(StoreError::Keychain("locked".into())))]);
        assert_eq!(
            check_key_not_bound_with(std::slice::from_ref(&a), &new, &with_key("K"), broken),
            Err(DuplicateKeyError::CannotVerify)
        );
        // Missing, malformed and mismatching items cannot trade: they do not block.
        for e in [
            StoreError::NoEntry,
            StoreError::InvalidFormat,
            StoreError::Mismatch,
        ] {
            let load = keychain(vec![(a.id, Err(e))]);
            assert!(
                check_key_not_bound_with(std::slice::from_ref(&a), &new, &with_key("K"), load)
                    .is_ok()
            );
        }
        // One unreadable item still refuses even if an earlier one was fine.
        let load = keychain(vec![
            (a.id, Ok("OTHER-KEY")),
            (b.id, Err(StoreError::Keychain("denied".into()))),
        ]);
        assert_eq!(
            check_key_not_bound_with(&[a, b], &new, &with_key("K"), load),
            Err(DuplicateKeyError::CannotVerify)
        );
    }
}
