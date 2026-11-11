//! The local risk log (condition 74): what the risk controls did, one JSON object per line.
//!
//! - Written under the data directory, in a file whose name is a constant of this module:
//!   no outside string ever becomes a path.
//! - Created with mode 0600 (Unix), and the mode is set again on every write, in case an
//!   older file was looser. A symbolic link at the log path is refused.
//! - Bounded: when the file reaches `max_bytes` it is rotated (`risk.log` → `risk.log.1` →
//!   … up to [`KEEP`] older files), so the total stays under about `(KEEP + 1) * max_bytes`.
//! - Only these fields: time, account alias and uuid, venue, environment, symbol, action,
//!   reason and a number. There is no free-text field: a message from an exchange, a key, a
//!   secret, a passphrase or a signature has no place to go. The alias (typed by the user) is
//!   cut to printable characters and 32 characters; symbols and numbers that do not look like
//!   one are replaced by `?`.
//!
//! Nothing here reads a clock: the time is the caller's.

use super::exchange_accounts::AccountMeta;
use exchange::account::{AccountEnv, AccountVenue};
use exchange::trade::{AutoPath, RiskNotice, RiskNoticeKind, RiskRefusal, TripReason};
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const FILE_NAME: &str = "risk.log";
/// Rotated files kept besides the current one.
pub const KEEP: u32 = 3;
pub const DEFAULT_MAX_BYTES: u64 = 512 * 1024;
const ALIAS_MAX_CHARS: usize = 32;
const NUMBER_MAX_CHARS: usize = 40;
const SYMBOL_MAX_CHARS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogAction {
    Tripped,
    EntryRefused,
    AutoCloseStarted,
    AutoCloseSkippedDisabled,
    AutoCloseFinished,
    /// An order sent on one of the three automatic paths (condition 71).
    AutoPathSend,
    /// An unresolved send was saved, loaded again at startup, or released by the user.
    UnknownSaved,
    UnknownRestored,
    UnknownReleased,
    /// A saved record with an invalid client id was dropped by the user (two-step confirmation).
    UnknownDiscarded,
    /// The user said an entry's automatic protection was handled by hand (it left the book).
    ProtectionHandled,
}

/// One log line. The fields are private and the constructors take typed values only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RiskLogEntry {
    at_ms: i64,
    alias: String,
    account: Uuid,
    venue: &'static str,
    env: &'static str,
    symbol: Option<String>,
    action: LogAction,
    reason: &'static str,
    value: Option<String>,
}

fn venue_str(v: AccountVenue) -> &'static str {
    v.as_str()
}

fn env_str(e: AccountEnv) -> &'static str {
    e.as_str()
}

fn clean_alias(alias: &str) -> String {
    alias
        .chars()
        .filter(|c| !c.is_control())
        .take(ALIAS_MAX_CHARS)
        .collect()
}

fn clean_symbol(symbol: &str) -> String {
    // Same alphabet as an order symbol: upper-case letters, digits and `-`.
    let ok = !symbol.is_empty()
        && symbol.len() <= SYMBOL_MAX_CHARS
        && symbol
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        symbol.to_string()
    } else {
        "?".to_string()
    }
}

/// A plain decimal number: digits, at most one dot, an optional leading minus.
fn clean_number(n: &str) -> String {
    let body = n.strip_prefix('-').unwrap_or(n);
    let ok = !body.is_empty()
        && n.len() <= NUMBER_MAX_CHARS
        && body.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && body.bytes().filter(|b| *b == b'.').count() <= 1;
    if ok { n.to_string() } else { "?".to_string() }
}

fn trip_reason(r: TripReason) -> &'static str {
    r.as_str()
}

fn refusal_reason(r: RiskRefusal) -> &'static str {
    match r {
        RiskRefusal::NotionalUnknown => "notional_unknown",
        RiskRefusal::SingleOrderCapExceeded => "single_order_cap",
        RiskRefusal::SymbolCapExceeded => "symbol_cap",
        RiskRefusal::PositionUnknown => "position_unknown",
        RiskRefusal::EquityUnavailable => "equity_unavailable",
        RiskRefusal::Locked => "locked",
        RiskRefusal::SettingsInvalid => "settings_invalid",
        RiskRefusal::WrongAccount => "wrong_account",
        RiskRefusal::SnapshotNotApplied => "snapshot_not_applied",
        RiskRefusal::StateUnavailable => "state_unavailable",
    }
}

fn path_reason(p: AutoPath) -> &'static str {
    match p {
        AutoPath::RiskClose => "risk_close",
        AutoPath::AttachProtective => "attach_protective",
        AutoPath::CancelSibling => "cancel_sibling",
    }
}

impl RiskLogEntry {
    fn base(meta: &AccountMeta, at_ms: i64, action: LogAction, reason: &'static str) -> Self {
        Self {
            at_ms,
            alias: clean_alias(&meta.alias),
            account: meta.id,
            venue: venue_str(meta.venue),
            env: env_str(meta.env),
            symbol: None,
            action,
            reason,
            value: None,
        }
    }

    /// From a notice the risk tracker or the automatic close produced.
    pub fn from_notice(meta: &AccountMeta, notice: &RiskNotice) -> Self {
        match notice.kind {
            RiskNoticeKind::Tripped { reason, equity, .. } => {
                let mut e = Self::base(meta, notice.at_ms, LogAction::Tripped, trip_reason(reason));
                e.value = Some(clean_number(&equity.to_plain_string()));
                e
            }
            RiskNoticeKind::AutoCloseSkippedDisabled { reason } => Self::base(
                meta,
                notice.at_ms,
                LogAction::AutoCloseSkippedDisabled,
                trip_reason(reason),
            ),
            RiskNoticeKind::AutoCloseStarted { reason } => Self::base(
                meta,
                notice.at_ms,
                LogAction::AutoCloseStarted,
                trip_reason(reason),
            ),
            RiskNoticeKind::AutoCloseFinished {
                clear,
                remaining_orders,
                remaining_positions,
                reread_failed,
            } => {
                let reason = if reread_failed {
                    "reread_failed"
                } else if clear {
                    "clear"
                } else {
                    "remaining"
                };
                let mut e = Self::base(meta, notice.at_ms, LogAction::AutoCloseFinished, reason);
                // Orders and positions left, as "orders/positions"; `?` when that read failed.
                let count = |n: Option<usize>| n.map_or("?".to_string(), |n| n.to_string());
                e.value = Some(format!(
                    "{}/{}",
                    count(remaining_orders),
                    count(remaining_positions)
                ));
                e
            }
        }
    }

    /// A new entry the risk hook refused.
    pub fn entry_refused(
        meta: &AccountMeta,
        at_ms: i64,
        symbol: &str,
        refusal: RiskRefusal,
    ) -> Self {
        let mut e = Self::base(
            meta,
            at_ms,
            LogAction::EntryRefused,
            refusal_reason(refusal),
        );
        e.symbol = Some(clean_symbol(symbol));
        e
    }

    /// An order sent on an automatic path.
    pub fn auto_path_send(meta: &AccountMeta, at_ms: i64, symbol: &str, path: AutoPath) -> Self {
        let mut e = Self::base(meta, at_ms, LogAction::AutoPathSend, path_reason(path));
        e.symbol = Some(clean_symbol(symbol));
        e
    }

    /// An unresolved-send event (saved, restored at startup, released by hand). Carries the
    /// symbol only: no client id, no order parameters.
    pub fn unknown_event(meta: &AccountMeta, at_ms: i64, symbol: &str, action: LogAction) -> Self {
        let reason = match action {
            LogAction::UnknownSaved => "unknown_saved",
            LogAction::UnknownRestored => "unknown_restored",
            LogAction::UnknownDiscarded => "unknown_discarded_invalid_record",
            _ => "unknown_released_by_user",
        };
        let mut e = Self::base(meta, at_ms, action, reason);
        e.symbol = Some(clean_symbol(symbol));
        e
    }

    /// The user removed an entry of the automatic protection from the book ("I've handled
    /// it"). The symbol only: no client id, no order parameters, no failure text.
    pub fn protection_handled(meta: &AccountMeta, at_ms: i64, symbol: &str) -> Self {
        let mut e = Self::base(
            meta,
            at_ms,
            LogAction::ProtectionHandled,
            "protection_handled_by_user",
        );
        e.symbol = Some(clean_symbol(symbol));
        e
    }

    /// Test helper for the sanitising rules: a line with caller-chosen symbol and number.
    #[cfg(test)]
    fn raw_for_test(meta: &AccountMeta, symbol: &str, value: &str) -> Self {
        let mut e = Self::base(meta, 0, LogAction::Tripped, "daily_loss");
        e.symbol = Some(clean_symbol(symbol));
        e.value = Some(clean_number(value));
        e
    }
}

/// An append-only, size-bounded log file.
#[derive(Debug, Clone)]
pub struct RiskLog {
    dir: PathBuf,
    max_bytes: u64,
}

impl RiskLog {
    /// The log in the data directory (`FLOWSURFACE_DATA_PATH` or the platform default).
    pub fn in_data_dir() -> Self {
        Self::in_dir(crate::data_path(None), DEFAULT_MAX_BYTES)
    }

    /// A log in `dir`, a directory the program chose (not text from outside).
    pub fn in_dir(dir: PathBuf, max_bytes: u64) -> Self {
        Self {
            dir,
            max_bytes: max_bytes.max(1024),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE_NAME)
    }

    fn rotated(&self, n: u32) -> PathBuf {
        self.dir.join(format!("{FILE_NAME}.{n}"))
    }

    pub fn append(&self, entry: &RiskLogEntry) -> io::Result<()> {
        let mut line = serde_json::to_string(entry).map_err(io::Error::other)?;
        line.push('\n');
        fs::create_dir_all(&self.dir)?;
        let path = self.path();
        refuse_symlink(&path)?;
        self.rotate_if_full(&path, line.len() as u64)?;
        let mut file = open_private(&path)?;
        file.write_all(line.as_bytes())?;
        file.flush()
    }

    fn rotate_if_full(&self, path: &Path, incoming: u64) -> io::Result<()> {
        let len = match fs::metadata(path) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if len + incoming <= self.max_bytes {
            return Ok(());
        }
        let _ = fs::remove_file(self.rotated(KEEP));
        for n in (1..KEEP).rev() {
            let from = self.rotated(n);
            if from.exists() {
                fs::rename(&from, self.rotated(n + 1))?;
            }
        }
        fs::rename(path, self.rotated(1))
    }
}

fn refuse_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "risk log path is a symbolic link",
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

// Known limit (review L1): `refuse_symlink` and this `open` are two steps, and `open` does
// not pass `O_NOFOLLOW`, so a symlink swapped in between is followed. Closing that gap needs
// the platform's `O_NOFOLLOW` value (it differs by architecture), i.e. a `libc` dependency
// this crate does not have. Exploiting it needs write access to the user's data directory,
// which already allows far more than this.
#[cfg(unix)]
fn open_private(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    // A file made earlier, or under a different umask, is tightened as well.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Windows keeps the per-user ACL of the data directory; there is no mode to set.
#[cfg(not(unix))]
fn open_private(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::account::AccountProduct;
    use exchange::trade::Decimal;

    /// A scratch directory that is removed when the test ends, pass or fail.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("flowsurface-risk-log-{}", Uuid::new_v4()));
            fs::create_dir_all(&dir).expect("scratch dir");
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn meta(alias: &str) -> AccountMeta {
        AccountMeta {
            id: Uuid::from_u128(0x1234),
            alias: alias.to_string(),
            venue: AccountVenue::Okx,
            env: AccountEnv::Testnet,
            product: AccountProduct::UsdM,
            ..AccountMeta::default()
        }
    }

    fn notice(kind: RiskNoticeKind) -> RiskNotice {
        RiskNotice {
            account: exchange::account::AccountKey::from_bytes([1; 16]),
            at_ms: 1_700_000_000_000,
            kind,
        }
    }

    #[test]
    fn an_event_with_a_fake_secret_is_logged_without_it() {
        const SECRET: &str = "FAKE-SECRET-abcdef0123456789";
        let dir = Scratch::new();
        let log = RiskLog::in_dir(dir.0.clone(), DEFAULT_MAX_BYTES);
        // Everything that could carry text from outside: symbol, number, alias.
        let a = RiskLogEntry::raw_for_test(&meta("main"), &format!("BTC&secret={SECRET}"), SECRET);
        let b = RiskLogEntry::entry_refused(
            &meta("main\nINJECTED"),
            1,
            &format!("btc {SECRET}"),
            RiskRefusal::Locked,
        );
        log.append(&a).expect("write");
        log.append(&b).expect("write");
        let text = fs::read_to_string(log.path()).expect("read back");
        assert!(!text.contains(SECRET), "{text}");
        assert!(!text.contains("secret"), "{text}");
        assert_eq!(text.lines().count(), 2, "one line per entry: {text}");
        // The control character of the alias is gone, so it cannot start a fake line.
        assert!(text.contains("mainINJECTED"));
        for line in text.lines() {
            let v: serde_json::Value = serde_json::from_str(line).expect("each line is JSON");
            assert!(v.get("account").is_some());
        }
    }

    #[test]
    fn entries_carry_the_documented_fields_only() {
        let e = RiskLogEntry::from_notice(
            &meta("acct"),
            &notice(RiskNoticeKind::Tripped {
                reason: TripReason::Drawdown,
                equity: Decimal::parse("912.5").expect("decimal"),
                reference: Decimal::parse("1000").expect("decimal"),
            }),
        );
        let v = serde_json::to_value(&e).expect("json");
        let mut keys: Vec<_> = v.as_object().expect("object").keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "account", "action", "alias", "at_ms", "env", "reason", "symbol", "value", "venue"
            ]
        );
        assert_eq!(v["action"], "tripped");
        assert_eq!(v["reason"], "drawdown");
        assert_eq!(v["value"], "912.5");
        assert_eq!(v["venue"], "okx");
        assert_eq!(v["env"], "testnet");
    }

    #[test]
    fn long_aliases_and_odd_numbers_are_cut() {
        let long = "x".repeat(100);
        let e = RiskLogEntry::raw_for_test(&meta(&long), "BTC-USDT", "1e5");
        assert_eq!(e.alias.chars().count(), ALIAS_MAX_CHARS);
        assert_eq!(e.value.as_deref(), Some("?"));
        assert_eq!(clean_number("-12.50"), "-12.50");
        assert_eq!(clean_number("1.2.3"), "?");
        assert_eq!(clean_number(""), "?");
        assert_eq!(clean_symbol("BTC-USDT-SWAP"), "BTC-USDT-SWAP");
        assert_eq!(clean_symbol("btc"), "?");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private_even_if_it_existed_with_looser_rights() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Scratch::new();
        let log = RiskLog::in_dir(dir.0.clone(), DEFAULT_MAX_BYTES);
        let e = RiskLogEntry::raw_for_test(&meta("a"), "BTC-USDT", "1");
        log.append(&e).expect("write");
        let mode = |p: &Path| fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&log.path()), 0o600);
        // Loosen it, write again: tightened.
        fs::set_permissions(log.path(), fs::Permissions::from_mode(0o644)).expect("chmod");
        log.append(&e).expect("write");
        assert_eq!(mode(&log.path()), 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_log_path_is_refused() {
        let dir = Scratch::new();
        let target = dir.0.join("elsewhere");
        fs::write(&target, b"keep").expect("write");
        let log = RiskLog::in_dir(dir.0.clone(), DEFAULT_MAX_BYTES);
        std::os::unix::fs::symlink(&target, log.path()).expect("symlink");
        let e = RiskLogEntry::raw_for_test(&meta("a"), "BTC-USDT", "1");
        assert!(log.append(&e).is_err());
        assert_eq!(fs::read(&target).expect("read"), b"keep");
    }

    #[test]
    fn the_log_rotates_and_stays_bounded() {
        let dir = Scratch::new();
        let max = 2048;
        let log = RiskLog::in_dir(dir.0.clone(), max);
        let e = RiskLogEntry::raw_for_test(&meta("a"), "BTC-USDT", "1");
        for _ in 0..200 {
            log.append(&e).expect("write");
        }
        let size = |p: PathBuf| fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        assert!(size(log.path()) <= max);
        for n in 1..=KEEP {
            assert!(size(log.rotated(n)) <= max, "rotated file {n}");
        }
        assert!(log.rotated(1).exists());
        assert!(
            !log.rotated(KEEP + 1).exists(),
            "no more than KEEP rotated files"
        );
        // Rotated files are private too.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = fs::metadata(log.rotated(1))
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(m & 0o777, 0o600);
        }
    }

    #[test]
    fn a_handled_protection_logs_the_symbol_and_nothing_else() {
        let e = RiskLogEntry::protection_handled(&meta("main"), 7, "BTCUSDT");
        let v = serde_json::to_value(&e).expect("json");
        assert_eq!(v["action"], "protection_handled");
        assert_eq!(v["reason"], "protection_handled_by_user");
        assert_eq!(v["symbol"], "BTCUSDT");
        assert!(v["value"].is_null());
        // A symbol that does not look like one is replaced.
        let odd = RiskLogEntry::protection_handled(&meta("main"), 7, "BTC USDT; rm");
        assert_eq!(serde_json::to_value(&odd).expect("json")["symbol"], "?");
    }

    #[test]
    fn the_path_is_built_by_the_program() {
        let log = RiskLog::in_dir(PathBuf::from("/data"), DEFAULT_MAX_BYTES);
        assert_eq!(log.path(), PathBuf::from("/data/risk.log"));
    }

    #[test]
    fn notices_map_to_actions() {
        let m = meta("a");
        let act = |k| {
            serde_json::to_value(RiskLogEntry::from_notice(&m, &notice(k))).expect("json")["action"]
                .clone()
        };
        assert_eq!(
            act(RiskNoticeKind::AutoCloseSkippedDisabled {
                reason: TripReason::DailyLoss
            }),
            "auto_close_skipped_disabled"
        );
        assert_eq!(
            act(RiskNoticeKind::AutoCloseFinished {
                clear: false,
                remaining_orders: Some(2),
                remaining_positions: Some(0),
                reread_failed: false
            }),
            "auto_close_finished"
        );
    }
}
