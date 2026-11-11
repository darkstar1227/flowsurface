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
use exchange::account::{
    AccountEnv, AccountKey, AccountVenue, CheckReport, PositionMode, Verdict, is_usable,
};
use exchange::trade::{GateClass, GateRequest, SendGate};
use std::collections::HashMap;
use uuid::Uuid;

/// While `false`, every order for a `Live` account is refused, whatever else passes.
///
/// Development-order limit: LIVE sending must not exist on `main` before the local risk
/// controls (segment e, trade-risk) are merged. The local risk hook is part of the exchange
/// crate's submit path now, but the GUI does not build the risk snapshot or call `submit` yet:
/// remove this switch only after segment c, with N2, E1–E3, C1–C5 and the testnet
/// verification done.
// TODO(trade-risk): delete once the conditions above are met.
const LIVE_SENDING_ENABLED: bool = false;

/// Whether LIVE orders may be sent at all (for the ticket to say so; the gate still decides).
pub const fn live_sending_enabled() -> bool {
    LIVE_SENDING_ENABLED
}

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

    /// Forget an account's verdict: it was edited, deleted, failed a test, or the exchange
    /// stream refused its login.
    pub fn invalidate(&mut self, id: Uuid) {
        self.entries.remove(&id);
    }

    pub fn get(&self, id: Uuid) -> Option<&SessionEntry> {
        self.entries.get(&id)
    }
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
    /// Contract order while the exchange's position mode is unknown.
    PositionModeUnknown,
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
    order: OrderRequest,
) -> Result<(), GateRejection> {
    can_send_with(
        LIVE_SENDING_ENABLED,
        session,
        keychain,
        saved,
        proxy_ok,
        order,
    )
}

/// The F2 gate as seen by `exchange::trade::TradeClient::submit`: one account's session,
/// Keychain and proxy, checked by [`can_send`]. Build one per order, after
/// re-reading the Keychain item. The Live switch stays [`LIVE_SENDING_ENABLED`].
///
/// This is the only implementation of `SendGate` the app may use. The exchange crate cannot
/// stop another implementation from approving everything, so review must keep it that way.
pub struct SessionGate<'a> {
    pub session: Option<&'a SessionEntry>,
    pub keychain: &'a Result<StoredIdentity, StoreError>,
    pub saved: &'a AccountMeta,
    pub proxy_ok: bool,
}

impl SendGate for SessionGate<'_> {
    type Refusal = GateRejection;

    fn check(&self, request: GateRequest) -> Result<(), GateRejection> {
        // The client is bound to one account; an order for another one is not this gate's.
        if request.account != AccountKey::from_bytes(*self.saved.id.as_bytes()) {
            return Err(GateRejection::IdentityMismatch);
        }
        // The request says which venue, environment and product the client will really sign
        // for. It must be the saved account's: a Testnet verdict never lets a Live client send
        // (B2), and the Live switch below is judged on this environment.
        if (request.venue, request.env, request.product)
            != (self.saved.venue, self.saved.env, self.saved.product)
        {
            return Err(GateRejection::IdentityMismatch);
        }
        can_send(
            self.session,
            self.keychain,
            self.saved,
            self.proxy_ok,
            OrderRequest {
                kind: match request.class {
                    GateClass::Entry => OrderKind::Entry,
                    GateClass::Reduce => OrderKind::Reduce,
                },
                contract: request.contract,
            },
        )
    }
}

/// [`can_send`] with the Live switch as a parameter, so the full rule table stays tested
/// while the switch is off.
fn can_send_with(
    live_enabled: bool,
    session: Option<&SessionEntry>,
    keychain: &Result<StoredIdentity, StoreError>,
    saved: &AccountMeta,
    proxy_ok: bool,
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
    // The builder reads the exchange-reported mode; without it the order parameters
    // (positionSide / reduceOnly) cannot be chosen correctly.
    if order.contract && entry.actual_mode.is_none() {
        return Err(PositionModeUnknown);
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
        can_send_with(live_enabled, session, &Ok(m.identity()), m, true, ENTRY)
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
        let r = can_send(Some(&good(&m)), &Ok(m.identity()), &m, true, ENTRY);
        assert_eq!(r, Err(GateRejection::LiveTradingNotYetEnabled));
        // With the switch on, the same session passes: the rest of the table is live.
        assert_eq!(run(true, &m, Some(&good(&m))), Ok(()));
    }

    /// Cancel all and close all send reducing requests: a Live account is refused for those too
    /// while the switch is off, so `cancel_all` / `flatten` under this gate write nothing.
    #[test]
    fn a_live_account_is_refused_for_reducing_requests_too() {
        let m = meta(
            AccountVenue::Binance,
            AccountEnv::Live,
            AccountProduct::UsdM,
        );
        let entry = good(&m);
        let keychain = Ok(m.identity());
        let gate = SessionGate {
            session: Some(&entry),
            keychain: &keychain,
            saved: &m,
            proxy_ok: true,
        };
        for class in [GateClass::Reduce, GateClass::Entry] {
            let request = GateRequest {
                account: AccountKey::from_bytes(*m.id.as_bytes()),
                venue: m.venue,
                env: m.env,
                product: m.product,
                class,
                contract: true,
            };
            assert_eq!(
                gate.check(request),
                Err(GateRejection::LiveTradingNotYetEnabled)
            );
        }
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
            can_send_with(true, Some(&s), &keychain, &m, true, ENTRY)
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
        let r = can_send_with(true, Some(&s), &Ok(m.identity()), &edited, true, ENTRY);
        assert_eq!(r, Err(GateRejection::IdentityMismatch));
        // Same edit with a Keychain item that agrees: the old session verdict is stale.
        let r = can_send_with(true, Some(&s), &Ok(edited.identity()), &edited, true, ENTRY);
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
    fn the_proxy_condition_and_reduce_orders() {
        let m = meta(
            AccountVenue::Binance,
            AccountEnv::Testnet,
            AccountProduct::UsdM,
        );
        let s = good(&m);
        let ask =
            |proxy_ok, order| can_send_with(true, Some(&s), &Ok(m.identity()), &m, proxy_ok, order);
        let reduce = OrderRequest {
            kind: OrderKind::Reduce,
            contract: true,
        };
        assert_eq!(ask(false, ENTRY), Err(GateRejection::ProxyNotOk));
        assert_eq!(ask(true, ENTRY), Ok(()));
        // The private stream state is not judged here: the exchange crate reads it from its
        // per-account registry inside `approve`, the only place it is looked at. Local risk is
        // not judged here either (one source of truth: the risk hook).
        assert_eq!(ask(true, reduce), Ok(()));
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

    #[test]
    fn session_gate_follows_can_send_and_keeps_live_closed() {
        let live = meta(
            AccountVenue::Binance,
            AccountEnv::Live,
            AccountProduct::UsdM,
        );
        let entry = good(&live);
        let kc = Ok(live.identity());
        let gate = SessionGate {
            session: Some(&entry),
            keychain: &kc,
            saved: &live,
            proxy_ok: true,
        };
        let req = GateRequest {
            account: AccountKey::from_bytes(*live.id.as_bytes()),
            venue: live.venue,
            env: live.env,
            product: live.product,
            class: GateClass::Entry,
            contract: true,
        };
        assert_eq!(
            gate.check(req),
            Err(GateRejection::LiveTradingNotYetEnabled)
        );
        assert_eq!(
            gate.check(GateRequest {
                class: GateClass::Reduce,
                ..req
            }),
            Err(GateRejection::LiveTradingNotYetEnabled)
        );
        // another account's order is refused before anything else
        let other = GateRequest {
            account: AccountKey::from_bytes([9; 16]),
            ..req
        };
        assert_eq!(gate.check(other), Err(GateRejection::IdentityMismatch));
        // testnet passes the same checks
        let test = meta(
            AccountVenue::Binance,
            AccountEnv::Testnet,
            AccountProduct::UsdM,
        );
        let entry = good(&test);
        let kc = Ok(test.identity());
        let gate = SessionGate {
            session: Some(&entry),
            keychain: &kc,
            saved: &test,
            proxy_ok: true,
        };
        let req = GateRequest {
            account: AccountKey::from_bytes(*test.id.as_bytes()),
            env: test.env,
            ..req
        };
        assert_eq!(gate.check(req), Ok(()));
        // The Testnet verdict cannot be used for a request that will be signed for Live, or
        // for another venue or product (B2).
        for wrong in [
            GateRequest {
                env: AccountEnv::Live,
                ..req
            },
            GateRequest {
                venue: AccountVenue::Okx,
                ..req
            },
            GateRequest {
                product: AccountProduct::Spot,
                ..req
            },
        ] {
            assert_eq!(gate.check(wrong), Err(GateRejection::IdentityMismatch));
        }
    }

    /// The source of a file without its `#[cfg(test)] mod ... {` block. Same algorithm as the
    /// exchange crate's `source_rules_tests`: braces in string and char literals do not count.
    fn production_part(src: &str) -> String {
        let lines: Vec<&str> = src.lines().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let is_test_mod = lines[i].trim() == "#[cfg(test)]"
                && lines
                    .get(i + 1)
                    .is_some_and(|l| l.trim_start().starts_with("mod ") || l.contains(" mod "));
            if is_test_mod {
                let mut depth = 0i32;
                let mut seen = false;
                i += 1;
                while i < lines.len() {
                    let mut in_str = false;
                    let mut prev = ' ';
                    for c in lines[i].chars() {
                        if in_str {
                            if c == '"' && prev != '\\' {
                                in_str = false;
                            }
                        } else {
                            match c {
                                '"' => in_str = true,
                                '{' if prev != '\'' => {
                                    depth += 1;
                                    seen = true;
                                }
                                '}' if prev != '\'' => depth -= 1,
                                _ => {}
                            }
                        }
                        prev = c;
                    }
                    i += 1;
                    if (seen && depth <= 0) || (!seen && lines[i - 1].trim_end().ends_with(';')) {
                        break;
                    }
                }
                continue;
            }
            out.push(lines[i]);
            i += 1;
        }
        out.join("\n")
    }

    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                rust_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    #[test]
    fn the_scan_helper_skips_test_modules_and_braces_in_strings() {
        let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    const S: &str = \"}\";\n    impl SendGate for X {}\n}\nfn b() {}\n";
        let prod = production_part(src);
        assert!(prod.contains("fn a()") && prod.contains("fn b()"), "{prod}");
        assert!(!prod.contains("SendGate for"), "{prod}");
    }

    /// Review item C3: `SessionGate` is the only `SendGate` in the app. A trait cannot stop
    /// another implementation, so this reads the production source of the app and of `data`.
    /// A source directory that cannot be found fails the scan instead of passing it.
    #[test]
    fn session_gate_is_the_only_send_gate_implementation() {
        let data_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&data_src, &mut files);
        assert!(files.len() > 10, "data sources not found: {}", files.len());
        // The GUI crate sits next to `data/` in the workspace; a packaged crate has no
        // workspace above it, and only then may it be missing.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let in_workspace = std::fs::read_to_string(root.join("Cargo.toml"))
            .is_ok_and(|t| t.contains("[workspace]"));
        let before = files.len();
        rust_files(&root.join("src"), &mut files);
        if in_workspace {
            assert!(files.len() > before, "GUI sources not found");
        }
        let mut found = Vec::new();
        for f in files {
            let Ok(src) = std::fs::read_to_string(&f) else {
                continue;
            };
            for (n, line) in production_part(&src).lines().enumerate() {
                if !line.trim_start().starts_with("//") && line.contains("SendGate for") {
                    found.push(format!("{}:{}", f.display(), n + 1));
                }
            }
        }
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("trade_gate.rs"), "{found:?}");
    }

    /// Review N3: `UnknownStore` is the only `UnknownJournal` in the app. The trait cannot be
    /// sealed (data must implement it), so a no-op implementation is caught here instead.
    #[test]
    fn the_unknown_store_is_the_only_journal_implementation() {
        let data_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&data_src, &mut files);
        assert!(files.len() > 10, "data sources not found: {}", files.len());
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let in_workspace = std::fs::read_to_string(root.join("Cargo.toml"))
            .is_ok_and(|t| t.contains("[workspace]"));
        let before = files.len();
        rust_files(&root.join("src"), &mut files);
        if in_workspace {
            assert!(files.len() > before, "GUI sources not found");
        }
        let mut found = Vec::new();
        for f in files {
            let Ok(src) = std::fs::read_to_string(&f) else {
                continue;
            };
            for (n, line) in production_part(&src).lines().enumerate() {
                if !line.trim_start().starts_with("//") && line.contains("UnknownJournal for") {
                    found.push(format!("{}:{}", f.display(), n + 1));
                }
            }
        }
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("unknown_store.rs"), "{found:?}");
    }
}
