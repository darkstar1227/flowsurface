//! The send gate: a pure function that decides whether an order may leave the app, and the
//! in-memory session verdicts it reads.
//!
//! Nothing here sends anything. A later segment calls [`can_send`] before every order, after
//! re-reading the Keychain item (`load_credentials_with_identity`) so that the identity it
//! passes in is current.
//!
//! The session lives in memory only (owned by the top-level `Flowsurface`, never written to
//! `saved-state.json`): a verdict from an earlier run must not unlock trading, so every
//! session starts untested.

use super::exchange_accounts::{AccountMeta, StoreError, StoredIdentity};
use exchange::account::{AccountEnv, AccountVenue, CheckReport, PositionMode, Verdict, is_usable};
use std::collections::HashMap;
use uuid::Uuid;

/// While `false`, every order for a `Live` account is refused, whatever else passes.
///
/// Development-order limit: LIVE sending must not exist on `main` before the local risk
/// controls (segment e, trade-risk) are merged. Segment e removes this switch.
// TODO(trade-risk): delete once the risk gate (condition 72) is merged.
const LIVE_SENDING_ENABLED: bool = false;

/// What the last connection test of an account in this session found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub venue: AccountVenue,
    pub env: AccountEnv,
    pub product: exchange::account::AccountProduct,
    pub verdict: Verdict,
    /// Trading permission of the key (`None` = unknown).
    pub can_trade: Option<bool>,
    /// Position mode as the exchange reported it, never the value stored in the account.
    pub actual_mode: Option<PositionMode>,
}

/// Per-account session verdicts. Dropped with the process.
#[derive(Debug, Default)]
pub struct TradeSession {
    entries: HashMap<Uuid, SessionEntry>,
}

impl TradeSession {
    /// Store the result of a connection test (replacing any earlier one).
    pub fn record(&mut self, meta: &AccountMeta, report: &CheckReport) {
        self.entries.insert(
            meta.id,
            SessionEntry {
                venue: meta.venue,
                env: meta.env,
                product: meta.product,
                verdict: report.verdict.clone(),
                can_trade: report.can_trade,
                actual_mode: report.actual_mode,
            },
        );
    }

    /// Forget an account's verdict: it was edited, deleted, failed a test, or its private
    /// stream lost authentication.
    pub fn invalidate(&mut self, id: Uuid) {
        self.entries.remove(&id);
    }

    pub fn get(&self, id: Uuid) -> Option<&SessionEntry> {
        self.entries.get(&id)
    }
}

/// State of the account's private stream (segment b). Segment a has no stream yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    NotStarted,
    Connected,
    /// Login was refused: the session verdict is void.
    AuthFailed,
}

/// Local risk state (segment e). `Locked` refuses new entries; reducing is always allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskState {
    Clear,
    Locked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderKind {
    /// Adds exposure.
    Entry,
    /// Closes, reduces, or cancels.
    Reduce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderRequest {
    pub kind: OrderKind,
    /// Contract (futures / swap) order: needs the exchange-reported position mode.
    pub contract: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateRejection {
    /// LIVE sending is not enabled yet (development-order limit, see [`LIVE_SENDING_ENABLED`]).
    LiveTradingNotYetEnabled,
    /// The proxy snapshot no longer matches, or the proxy setting is broken.
    ProxyNotOk,
    /// The Keychain item could not be loaded.
    KeychainUnavailable,
    /// The Keychain item belongs to another venue, environment or product than the saved
    /// account.
    IdentityMismatch,
    /// No connection test in this session.
    NotTestedThisSession,
    /// The session verdict was recorded for an account that has since changed.
    SessionStale,
    /// The test rejected the key (for example it can withdraw).
    AccountRejected,
    /// Live needs a fully verified key; testnet may also be unverified.
    NotUsable,
    /// The key lacks the product's trading permission (or Live could not confirm it).
    NoTradePermission,
    StreamAuthFailed,
    /// Contract order while the exchange's position mode is unknown.
    PositionModeUnknown,
    RiskLocked,
}

/// Decide whether an order may be sent.
///
/// - `session`: this session's test result for the account.
/// - `keychain`: what loading the Keychain item just returned (identity only, no secrets).
/// - `saved`: the account as persisted in `saved-state.json`.
/// - `proxy_ok`: the proxy snapshot matches the current setting and is not broken.
pub fn can_send(
    session: Option<&SessionEntry>,
    keychain: &Result<StoredIdentity, StoreError>,
    saved: &AccountMeta,
    proxy_ok: bool,
    stream: StreamState,
    risk: RiskState,
    order: OrderRequest,
) -> Result<(), GateRejection> {
    can_send_with(
        LIVE_SENDING_ENABLED,
        session,
        keychain,
        saved,
        proxy_ok,
        stream,
        risk,
        order,
    )
}

/// [`can_send`] with the Live switch as a parameter, so the full rule table stays tested
/// while the switch is off.
fn can_send_with(
    live_enabled: bool,
    session: Option<&SessionEntry>,
    keychain: &Result<StoredIdentity, StoreError>,
    saved: &AccountMeta,
    proxy_ok: bool,
    stream: StreamState,
    risk: RiskState,
    order: OrderRequest,
) -> Result<(), GateRejection> {
    use GateRejection::*;

    if saved.env == AccountEnv::Live && !live_enabled {
        return Err(LiveTradingNotYetEnabled);
    }
    if !proxy_ok {
        return Err(ProxyNotOk);
    }
    match keychain {
        Err(_) => return Err(KeychainUnavailable),
        Ok(stored) if *stored != saved.identity() => return Err(IdentityMismatch),
        Ok(_) => {}
    }
    let Some(entry) = session else {
        return Err(NotTestedThisSession);
    };
    if (entry.venue, entry.env, entry.product) != (saved.venue, saved.env, saved.product) {
        return Err(SessionStale);
    }
    if matches!(entry.verdict, Verdict::Rejected(_)) {
        return Err(AccountRejected);
    }
    if !is_usable(saved.env, &entry.verdict) {
        return Err(NotUsable);
    }
    let permission_ok = match saved.env {
        AccountEnv::Live => entry.can_trade == Some(true),
        // Testnet cannot always read permissions: only an explicit "no" blocks.
        AccountEnv::Testnet => entry.can_trade != Some(false),
    };
    if !permission_ok {
        return Err(NoTradePermission);
    }
    if stream == StreamState::AuthFailed {
        return Err(StreamAuthFailed);
    }
    // The builder reads the exchange-reported mode; without it the order parameters
    // (positionSide / reduceOnly) cannot be chosen correctly.
    if order.contract && entry.actual_mode.is_none() {
        return Err(PositionModeUnknown);
    }
    if order.kind == OrderKind::Entry && risk == RiskState::Locked {
        return Err(RiskLocked);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::account::AccountProduct;

    fn meta(venue: AccountVenue, env: AccountEnv, product: AccountProduct) -> AccountMeta {
        AccountMeta::new("a".into(), venue, env, product, PositionMode::OneWay)
    }

    fn report(
        verdict: Verdict,
        can_trade: Option<bool>,
        mode: Option<PositionMode>,
    ) -> CheckReport {
        CheckReport {
            verdict,
            warnings: vec![],
            actual_mode: mode,
            can_trade,
        }
    }

    fn good(meta: &AccountMeta) -> SessionEntry {
        let mut s = TradeSession::default();
        s.record(
            meta,
            &report(Verdict::Usable, Some(true), Some(PositionMode::OneWay)),
        );
        s.get(meta.id).unwrap().clone()
    }

    const ENTRY: OrderRequest = OrderRequest {
        kind: OrderKind::Entry,
        contract: true,
    };

    fn run(
        live_enabled: bool,
        m: &AccountMeta,
        session: Option<&SessionEntry>,
    ) -> Result<(), GateRejection> {
        can_send_with(
            live_enabled,
            session,
            &Ok(m.identity()),
            m,
            true,
            StreamState::Connected,
            RiskState::Clear,
            ENTRY,
        )
    }

    #[test]
    fn untested_session_is_refused_and_a_good_one_passes_on_testnet() {
        let m = meta(
            AccountVenue::Binance,
            AccountEnv::Testnet,
            AccountProduct::UsdM,
        );
        assert_eq!(
            run(false, &m, None),
            Err(GateRejection::NotTestedThisSession)
        );
        assert_eq!(run(false, &m, Some(&good(&m))), Ok(()));
    }

    #[test]
    fn live_orders_are_refused_while_the_switch_is_off() {
        let m = meta(
            AccountVenue::Binance,
            AccountEnv::Live,
            AccountProduct::UsdM,
        );
        // Even a perfect session is refused through the public entry point.
        let r = can_send(
            Some(&good(&m)),
            &Ok(m.identity()),
            &m,
            true,
            StreamState::Connected,
            RiskState::Clear,
            ENTRY,
        );
        assert_eq!(r, Err(GateRejection::LiveTradingNotYetEnabled));
        // With the switch on, the same session passes: the rest of the table is live.
        assert_eq!(run(true, &m, Some(&good(&m))), Ok(()));
    }

    #[test]
    fn verdict_rules_by_environment() {
        let cases = [
            // (env, verdict, can_trade, expected)
            (AccountEnv::Live, Verdict::Usable, Some(true), Ok(())),
            (
                AccountEnv::Live,
                Verdict::Usable,
                Some(false),
                Err(GateRejection::NoTradePermission),
            ),
            (
                AccountEnv::Live,
                Verdict::Usable,
                None,
                Err(GateRejection::NoTradePermission),
            ),
            (
                AccountEnv::Live,
                Verdict::Unverified("x".into()),
                Some(true),
                Err(GateRejection::NotUsable),
            ),
            (
                AccountEnv::Live,
                Verdict::Rejected("x".into()),
                Some(true),
                Err(GateRejection::AccountRejected),
            ),
            (AccountEnv::Testnet, Verdict::Usable, Some(true), Ok(())),
            (
                AccountEnv::Testnet,
                Verdict::Unverified("x".into()),
                None,
                Ok(()),
            ),
            (
                AccountEnv::Testnet,
                Verdict::Unverified("x".into()),
                Some(false),
                Err(GateRejection::NoTradePermission),
            ),
            (
                AccountEnv::Testnet,
                Verdict::Rejected("x".into()),
                None,
                Err(GateRejection::AccountRejected),
            ),
        ];
        for (env, verdict, can_trade, expected) in cases {
            for venue in AccountVenue::ALL {
                for product in AccountProduct::ALL {
                    let m = meta(venue, env, product);
                    let mut s = TradeSession::default();
                    s.record(
                        &m,
                        &report(verdict.clone(), can_trade, Some(PositionMode::OneWay)),
                    );
                    assert_eq!(
                        run(true, &m, s.get(m.id)),
                        expected,
                        "{venue:?} {env:?} {product:?} {verdict:?} {can_trade:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn keychain_must_match_the_saved_account() {
        let m = meta(
            AccountVenue::Binance,
            AccountEnv::Testnet,
            AccountProduct::UsdM,
        );
        let s = good(&m);
        let ask = |keychain: Result<StoredIdentity, StoreError>| {
            can_send_with(
                true,
                Some(&s),
                &keychain,
                &m,
                true,
                StreamState::Connected,
                RiskState::Clear,
                ENTRY,
            )
        };
        assert_eq!(ask(Ok(m.identity())), Ok(()));
        for other in [
            StoredIdentity {
                venue: AccountVenue::Okx,
                ..m.identity()
            },
            StoredIdentity {
                env: AccountEnv::Live,
                ..m.identity()
            },
            StoredIdentity {
                product: AccountProduct::Spot,
                ..m.identity()
            },
        ] {
            assert_eq!(ask(Ok(other)), Err(GateRejection::IdentityMismatch));
        }
        for e in [
            StoreError::NoEntry,
            StoreError::InvalidFormat,
            StoreError::Mismatch,
        ] {
            assert_eq!(ask(Err(e)), Err(GateRejection::KeychainUnavailable));
        }
    }

    #[test]
    fn editing_saved_state_env_after_the_test_is_refused() {
        let m = meta(
            AccountVenue::Binance,
            AccountEnv::Testnet,
            AccountProduct::UsdM,
        );
        let s = good(&m);
        // `saved-state.json` was edited to Live; the Keychain item still says Testnet.
        let edited = AccountMeta {
            env: AccountEnv::Live,
            ..m.clone()
        };
        let r = can_send_with(
            true,
            Some(&s),
            &Ok(m.identity()),
            &edited,
            true,
            StreamState::Connected,
            RiskState::Clear,
            ENTRY,
        );
        assert_eq!(r, Err(GateRejection::IdentityMismatch));
        // Same edit with a Keychain item that agrees: the old session verdict is stale.
        let r = can_send_with(
            true,
            Some(&s),
            &Ok(edited.identity()),
            &edited,
            true,
            StreamState::Connected,
            RiskState::Clear,
            ENTRY,
        );
        assert_eq!(r, Err(GateRejection::SessionStale));
    }

    #[test]
    fn invalidating_the_session_blocks_sending() {
        let m = meta(AccountVenue::Okx, AccountEnv::Testnet, AccountProduct::UsdM);
        let mut s = TradeSession::default();
        s.record(
            &m,
            &report(Verdict::Usable, Some(true), Some(PositionMode::OneWay)),
        );
        assert_eq!(run(true, &m, s.get(m.id)), Ok(()));
        // Edited, deleted, failed re-test, or auth failure: the verdict is dropped.
        s.invalidate(m.id);
        assert_eq!(
            run(true, &m, s.get(m.id)),
            Err(GateRejection::NotTestedThisSession)
        );
        // A new test replaces an older verdict.
        s.record(&m, &report(Verdict::Rejected("x".into()), None, None));
        assert_eq!(
            run(true, &m, s.get(m.id)),
            Err(GateRejection::AccountRejected)
        );
    }

    #[test]
    fn proxy_stream_and_risk_conditions() {
        let m = meta(
            AccountVenue::Binance,
            AccountEnv::Testnet,
            AccountProduct::UsdM,
        );
        let s = good(&m);
        let ask = |proxy_ok, stream, risk, order| {
            can_send_with(
                true,
                Some(&s),
                &Ok(m.identity()),
                &m,
                proxy_ok,
                stream,
                risk,
                order,
            )
        };
        let reduce = OrderRequest {
            kind: OrderKind::Reduce,
            contract: true,
        };
        assert_eq!(
            ask(false, StreamState::Connected, RiskState::Clear, ENTRY),
            Err(GateRejection::ProxyNotOk)
        );
        assert_eq!(
            ask(true, StreamState::AuthFailed, RiskState::Clear, ENTRY),
            Err(GateRejection::StreamAuthFailed)
        );
        // No stream yet (segment a) is fine.
        assert_eq!(
            ask(true, StreamState::NotStarted, RiskState::Clear, ENTRY),
            Ok(())
        );
        // Risk lock refuses entries only; reducing still goes through.
        assert_eq!(
            ask(true, StreamState::Connected, RiskState::Locked, ENTRY),
            Err(GateRejection::RiskLocked)
        );
        assert_eq!(
            ask(true, StreamState::Connected, RiskState::Locked, reduce),
            Ok(())
        );
    }

    #[test]
    fn contract_orders_need_the_exchange_reported_position_mode() {
        let m = meta(
            AccountVenue::Binance,
            AccountEnv::Testnet,
            AccountProduct::UsdM,
        );
        let ask = |s: &TradeSession, contract: bool| {
            can_send_with(
                true,
                s.get(m.id),
                &Ok(m.identity()),
                &m,
                true,
                StreamState::Connected,
                RiskState::Clear,
                OrderRequest {
                    kind: OrderKind::Entry,
                    contract,
                },
            )
        };
        let mut s = TradeSession::default();
        s.record(&m, &report(Verdict::Usable, Some(true), None));
        assert_eq!(ask(&s, true), Err(GateRejection::PositionModeUnknown));
        // Spot orders have no position mode.
        assert_eq!(ask(&s, false), Ok(()));
        // A mode stored in the account never substitutes for the reported one.
        assert_eq!(m.position_mode, PositionMode::OneWay);
        // Hedge and one-way, once reported, both pass.
        for mode in PositionMode::ALL {
            s.record(&m, &report(Verdict::Usable, Some(true), Some(mode)));
            assert_eq!(ask(&s, true), Ok(()));
        }
    }
}
