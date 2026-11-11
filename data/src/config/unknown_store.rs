//! Unresolved sends that survive a restart (review N2, conditions 60 and 61).
//!
//! A send whose outcome is `Unknown` locks new entries for its (account, symbol). That lock
//! lives in memory, so without this file a restart would silently unlock it. The file holds
//! only what is needed to look the order up again: account id, symbol, client id, lookup kind
//! and a time. No order parameters (so a release cannot rebuild the order), no keys, no
//! secrets.
//!
//! - The path is a constant file name inside the data directory; no outside text becomes a
//!   path. The file is written with mode 0600 through a temporary file (a random name, created
//!   with `create_new`) and a rename. A symbolic link at the path is refused, also when it
//!   appears between the check and the open, and the file is read with a size cap.
//! - Every change is a read-modify-write under one process-wide lock, so two threads (the send
//!   path and the resolver) cannot overwrite each other's change.
//! - Loading checks every field again (alphabet, length, entry count). A bad entry is not
//!   skipped quietly: loading fails, so the app can refuse to trade until the file is looked at.
//! - `UnknownStore` is the exchange crate's write-ahead journal (`UnknownJournal`): the record
//!   is written before a send goes out and removed once its outcome is known.
//! - A record whose client id is not valid blocks every new entry of its account until restart
//!   and cannot be released like an unresolved send. [`UnknownStore::discard_invalid`] drops it
//!   after the user's two-step confirmation (`UnknownRecord::begin_discard`); it never drops a
//!   record with a valid id.
//! - The `*_async` methods run the same work on a blocking thread, so the GUI never reads,
//!   writes or syncs files on the iced thread.
//! - Releasing an entry by hand is the exchange crate's two-step confirmation
//!   (`PendingUnknown::begin_manual_release`, then the typed phrase); this store only removes
//!   the saved line afterwards. Saving, loading and releasing are logged with
//!   [`RiskLogEntry::unknown_event`].
//!
//! Nothing here reads a clock: the time is the caller's.

use super::exchange_accounts::AccountMeta;
use super::risk_log::{LogAction, RiskLog, RiskLogEntry};
use exchange::account::AccountKey;
use exchange::trade::{DiscardToken, UnknownRecord};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

const FILE_NAME: &str = "unresolved-orders.json";
/// More than this many unresolved sends is not a state the app should be in.
const MAX_ENTRIES: usize = 256;
const MAX_FIELD: usize = 64;
/// 256 entries of a few hundred bytes each; anything larger is not this program's file.
const MAX_FILE_BYTES: u64 = 256 * 1024;
const VERSION: u32 = 1;

/// Serialises every read-modify-write of the file. A process-wide lock, not one per value:
/// `UnknownStore` is `Clone` and several values can point at the same file.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    // A panic while holding the lock leaves the file as the last complete rename wrote it.
    WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Line {
    account: Uuid,
    symbol: String,
    client_id: String,
    kind: String,
    recorded_at_ms: i64,
}

/// No `serde(default)`: a file without a version or entries is not ours, and loading it as an
/// empty list would silently unlock everything. (The file is new on this branch, so there is no
/// older shape to stay compatible with.)
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: u32,
    entries: Vec<Line>,
}

/// What [`UnknownStore::discard_invalid_logged_async`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscardOutcome {
    /// A line was removed (`false`: it was already gone).
    pub discarded: bool,
    /// The audit line was written, or none was needed (nothing was removed).
    pub logged: bool,
}

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    /// The file is not what this program wrote; nothing was loaded.
    Corrupt(&'static str),
    /// `discard_invalid` was asked to drop a record whose client id is valid.
    StillValid,
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl StoreError {
    /// A short fixed description, safe to log (no path, no client id).
    pub fn reason(&self) -> &'static str {
        match self {
            StoreError::Io(_) => "file system error",
            StoreError::Corrupt(what) => what,
            StoreError::StillValid => "record has a valid client id",
        }
    }
}

fn plain(s: &str, allow: impl Fn(u8) -> bool) -> bool {
    !s.is_empty() && s.len() <= MAX_FIELD && s.bytes().all(allow)
}

impl std::fmt::Debug for Line {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Line")
            .field("symbol", &self.symbol)
            .field("client_id", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl Line {
    fn check(&self) -> Result<(), StoreError> {
        let symbol = plain(&self.symbol, |b| {
            b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-'
        });
        let id = plain(&self.client_id, |b| {
            b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
        });
        let kind = plain(&self.kind, |b| {
            b.is_ascii_lowercase() || b == b'_' || b == b':'
        });
        if !symbol {
            return Err(StoreError::Corrupt("symbol"));
        }
        if !id {
            return Err(StoreError::Corrupt("client id"));
        }
        if !kind {
            return Err(StoreError::Corrupt("kind"));
        }
        Ok(())
    }

    fn same_order(&self, account: Uuid, client_id: &str) -> bool {
        self.account == account && self.client_id == client_id
    }

    fn from_record(r: &UnknownRecord) -> Self {
        Line {
            account: Uuid::from_bytes(r.account.to_bytes()),
            symbol: r.symbol.clone(),
            client_id: r.client_id.clone(),
            kind: r.kind.clone(),
            recorded_at_ms: r.recorded_at_ms,
        }
    }

    fn to_record(&self) -> UnknownRecord {
        UnknownRecord {
            account: AccountKey::from_bytes(*self.account.as_bytes()),
            symbol: self.symbol.clone(),
            client_id: self.client_id.clone(),
            kind: self.kind.clone(),
            recorded_at_ms: self.recorded_at_ms,
        }
    }
}

/// The saved list of unresolved sends.
#[derive(Debug, Clone)]
pub struct UnknownStore {
    dir: PathBuf,
}

impl UnknownStore {
    /// The store in the data directory (`FLOWSURFACE_DATA_PATH` or the platform default).
    pub fn in_data_dir() -> Self {
        Self::in_dir(crate::data_path(None))
    }

    /// A store in `dir`, a directory the program chose (not text from outside).
    pub fn in_dir(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE_NAME)
    }

    /// Every saved record, to be handed to `TradeClient::restore_unknown` at startup. A
    /// missing file is an empty list; a damaged one is an error and nothing is returned.
    pub fn load(&self) -> Result<Vec<UnknownRecord>, StoreError> {
        // Under the same lock as the writers: a rename between the open and the identity check
        // would otherwise look like a swapped file.
        let _guard = lock();
        self.load_unlocked()
    }

    /// `load` for callers that already hold the lock (the mutex is not reentrant).
    fn load_unlocked(&self) -> Result<Vec<UnknownRecord>, StoreError> {
        let path = self.path();
        let text = match read_capped(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let file: File =
            serde_json::from_str(&text).map_err(|_| StoreError::Corrupt("not valid json"))?;
        if file.version != VERSION {
            return Err(StoreError::Corrupt("version"));
        }
        if file.entries.len() > MAX_ENTRIES {
            return Err(StoreError::Corrupt("too many entries"));
        }
        for l in &file.entries {
            l.check()?;
        }
        Ok(file.entries.iter().map(Line::to_record).collect())
    }

    /// Save one more unresolved send (called before the send goes out, see `UnknownJournal`).
    /// The same (account, client id) is never saved twice; a client id is only unique within
    /// its account.
    pub fn add(&self, record: &UnknownRecord) -> Result<(), StoreError> {
        let _guard = lock();
        let mut file = self.read_file()?;
        let line = Line::from_record(record);
        line.check()?;
        if file
            .entries
            .iter()
            .any(|l| l.same_order(line.account, &line.client_id))
        {
            return Ok(());
        }
        if file.entries.len() >= MAX_ENTRIES {
            return Err(StoreError::Corrupt("too many entries"));
        }
        file.entries.push(line);
        self.write_file(&file)
    }

    /// Forget one entry: after a lookup resolved it, or after the user's two-step release.
    /// Returns whether it was there.
    pub fn remove(&self, account: AccountKey, client_id: &str) -> Result<bool, StoreError> {
        let _guard = lock();
        let mut file = self.read_file()?;
        let id = Uuid::from_bytes(account.to_bytes());
        let before = file.entries.len();
        file.entries.retain(|l| !l.same_order(id, client_id));
        let removed = file.entries.len() != before;
        if removed {
            self.write_file(&file)?;
        }
        Ok(removed)
    }

    /// Drop a record whose client id is not valid, with the token of the user's typed
    /// confirmation. Refuses (`StillValid`) when the saved line's id is valid, so this cannot
    /// forget a live unresolved send; those are resolved or released instead. Returns whether
    /// the line was there. The account stays blocked for new entries until the program
    /// restarts: the block lives in memory and nothing here lifts it.
    pub fn discard_invalid(&self, token: DiscardToken) -> Result<bool, StoreError> {
        let _guard = lock();
        let mut file = self.read_file()?;
        let id = Uuid::from_bytes(token.account().to_bytes());
        let Some(line) = file
            .entries
            .iter()
            .find(|l| l.same_order(id, token.client_id()))
        else {
            return Ok(false);
        };
        if line.to_record().has_valid_client_id() {
            return Err(StoreError::StillValid);
        }
        file.entries
            .retain(|l| !l.same_order(id, token.client_id()));
        self.write_file(&file)?;
        Ok(true)
    }

    // ── Off the iced thread ──────────────────────────────────────────────

    /// [`UnknownStore::load`] on a blocking thread. Needs a tokio runtime.
    pub async fn load_async(&self) -> Result<Vec<UnknownRecord>, StoreError> {
        let this = self.clone();
        blocking(move || this.load()).await
    }

    /// [`UnknownStore::add`] on a blocking thread.
    pub async fn add_async(&self, record: UnknownRecord) -> Result<(), StoreError> {
        let this = self.clone();
        blocking(move || this.add(&record)).await
    }

    /// [`UnknownStore::remove`] on a blocking thread.
    pub async fn remove_async(
        &self,
        account: AccountKey,
        client_id: String,
    ) -> Result<bool, StoreError> {
        let this = self.clone();
        blocking(move || this.remove(account, &client_id)).await
    }

    /// [`UnknownStore::discard_invalid`] on a blocking thread.
    pub async fn discard_invalid_async(&self, token: DiscardToken) -> Result<bool, StoreError> {
        let this = self.clone();
        blocking(move || this.discard_invalid(token)).await
    }

    /// [`UnknownStore::discard_invalid_async`] followed by the audit line (review S5): after a
    /// successful removal an `UnknownDiscarded` entry goes to the risk log, with the symbol
    /// and the action only (no client id). The token must belong to the account `meta`
    /// describes, otherwise nothing is touched and `StoreError::Corrupt` comes back. A log
    /// write that fails does not undo the removal: it is reported in
    /// [`DiscardOutcome::logged`] so the caller can show it.
    pub async fn discard_invalid_logged_async(
        &self,
        token: DiscardToken,
        meta: AccountMeta,
        at_ms: i64,
        log: RiskLog,
    ) -> Result<DiscardOutcome, StoreError> {
        if token.account() != AccountKey::from_bytes(*meta.id.as_bytes()) {
            return Err(StoreError::Corrupt("discard token is for another account"));
        }
        let this = self.clone();
        let symbol = token.symbol().to_string();
        blocking(move || {
            let discarded = this.discard_invalid(token)?;
            let logged = !discarded
                || log
                    .append(&RiskLogEntry::unknown_event(
                        &meta,
                        at_ms,
                        &symbol,
                        LogAction::UnknownDiscarded,
                    ))
                    .is_ok();
            Ok(DiscardOutcome { discarded, logged })
        })
        .await
    }

    fn read_file(&self) -> Result<File, StoreError> {
        // Same checks as `load`, so a damaged file is never overwritten by an `add`.
        let records = self.load_unlocked()?;
        Ok(File {
            version: VERSION,
            entries: records.iter().map(Line::from_record).collect(),
        })
    }

    fn write_file(&self, file: &File) -> Result<(), StoreError> {
        fs::create_dir_all(&self.dir)?;
        let path = self.path();
        refuse_unless_regular(&path)?;
        // A random name, created with `create_new` (which never follows a link): nobody can
        // plant a file or a link at a name this process has not chosen yet.
        let tmp = self.dir.join(format!("{FILE_NAME}.{}.tmp", Uuid::new_v4()));
        let text = serde_json::to_string_pretty(file).map_err(io::Error::other)?;
        let written = (|| {
            let mut f = open_private(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            fs::rename(&tmp, &path)?;
            // The rename is only durable once the directory entry is: without this a crash can
            // bring the old file back, and with it a forgotten write-ahead record.
            sync_dir(&self.dir)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written?;
        Ok(())
    }
}

/// Run `f` on a blocking thread. A task that died is an I/O error, never "nothing happened".
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, StoreError> + Send + 'static,
) -> Result<T, StoreError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| StoreError::Io(io::Error::other("file task failed")))?
}

/// Make a rename in `dir` durable.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Windows has no directory handle to sync through `std`; the rename itself is atomic there.
#[cfg(not(unix))]
fn sync_dir(_: &Path) -> io::Result<()> {
    Ok(())
}

/// Read the file as text, refusing a link, a non-regular file and anything over the cap.
fn read_capped(path: &Path) -> io::Result<String> {
    let bad = |m: &'static str| io::Error::new(io::ErrorKind::InvalidInput, m);
    // Before `open`: opening a FIFO for reading blocks until a writer shows up, which would
    // hang the send path (and so a close waiting behind it). Only a regular file is opened.
    refuse_unless_regular(path)?;
    let mut f = open_read(path)?;
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(bad("unresolved orders path is not a regular file"));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(bad("unresolved orders file is too large"));
    }
    // A link swapped in after the first check: the opened file is no longer the one at the path.
    let now = fs::symlink_metadata(path)?;
    if now.file_type().is_symlink() || !same_file(&meta, &now) {
        return Err(bad("unresolved orders path changed while it was opened"));
    }
    let mut text = String::new();
    // The file can grow after the check: read at most one byte past the cap.
    (&mut f)
        .take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(bad("unresolved orders file is too large"));
    }
    Ok(text)
}

fn open_read(path: &Path) -> io::Result<fs::File> {
    OpenOptions::new().read(true).open(path)
}

#[cfg(unix)]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// No stable file id in `std` on other platforms; the link check above is all there is.
#[cfg(not(unix))]
fn same_file(_: &fs::Metadata, _: &fs::Metadata) -> bool {
    true
}

/// Refuse a link and any other file type that is not a regular file (a FIFO, a socket, a
/// device, a directory). A missing path is fine. Checked before anything is opened.
fn refuse_unless_regular(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unresolved orders path is a symbolic link",
        )),
        Ok(m) if !m.is_file() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unresolved orders path is not a regular file",
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn open_private(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Windows keeps the per-user ACL of the data directory; there is no mode to set.
#[cfg(not(unix))]
fn open_private(path: &Path) -> io::Result<fs::File> {
    OpenOptions::new().create_new(true).write(true).open(path)
}

impl exchange::trade::UnknownJournal for UnknownStore {
    fn record(&self, record: &UnknownRecord) -> Result<(), exchange::trade::JournalError> {
        self.add(record).map_err(|e| {
            // The error text can name a path: log the kind only.
            log::error!("unresolved order journal write failed: {}", e.reason());
            exchange::trade::JournalError
        })
    }

    fn clear(
        &self,
        account: AccountKey,
        client_id: &str,
    ) -> Result<(), exchange::trade::JournalError> {
        self.remove(account, client_id).map(|_| ()).map_err(|e| {
            log::error!("unresolved order journal clear failed: {}", e.reason());
            exchange::trade::JournalError
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("flowsurface-unknown-{}", Uuid::new_v4()));
            fs::create_dir_all(&dir).expect("scratch dir");
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn record(id: &str) -> UnknownRecord {
        UnknownRecord {
            account: AccountKey::from_bytes([5; 16]),
            symbol: "BTC-USDT-SWAP".into(),
            client_id: id.into(),
            kind: "okx_order".into(),
            recorded_at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn saved_records_load_again_and_a_missing_file_is_empty() {
        let dir = Scratch::new();
        let store = UnknownStore::in_dir(dir.0.clone());
        assert!(store.load().unwrap().is_empty());
        store.add(&record("eAAAA1")).unwrap();
        store.add(&record("eBBBB2")).unwrap();
        // The same id twice is one line.
        store.add(&record("eAAAA1")).unwrap();
        assert_eq!(store.load().unwrap().len(), 2);
        assert_eq!(store.load().unwrap()[0], record("eAAAA1"));
    }

    #[test]
    fn remove_drops_only_the_named_entry() {
        let dir = Scratch::new();
        let store = UnknownStore::in_dir(dir.0.clone());
        store.add(&record("eAAAA1")).unwrap();
        store.add(&record("eBBBB2")).unwrap();
        let other = AccountKey::from_bytes([6; 16]);
        assert!(!store.remove(other, "eAAAA1").unwrap());
        assert!(store.remove(record("x").account, "eAAAA1").unwrap());
        let left = store.load().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].client_id, "eBBBB2");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private_and_holds_no_secret_or_order_parameters() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Scratch::new();
        let store = UnknownStore::in_dir(dir.0.clone());
        store.add(&record("eAAAA1")).unwrap();
        let mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let text = fs::read_to_string(store.path()).unwrap();
        for forbidden in ["secret", "key", "price", "quantity", "size", "side"] {
            assert!(!text.contains(forbidden), "{forbidden}: {text}");
        }
        // No temporary file is left behind.
        let left: Vec<_> = fs::read_dir(&dir.0).unwrap().flatten().collect();
        assert_eq!(left.len(), 1, "{left:?}");
    }

    #[test]
    fn a_damaged_or_hostile_file_fails_loudly_and_is_not_overwritten() {
        let dir = Scratch::new();
        let store = UnknownStore::in_dir(dir.0.clone());
        fs::write(store.path(), "not json").unwrap();
        assert!(matches!(store.load(), Err(StoreError::Corrupt(_))));
        assert!(store.add(&record("eAAAA1")).is_err());
        assert_eq!(fs::read_to_string(store.path()).unwrap(), "not json");
        // An entry with a symbol that could reach a request is refused too.
        let evil = r#"{"version":1,"entries":[{"account":"00000000-0000-0000-0000-000000000001","symbol":"BTC&x=1","client_id":"eAAAA1","kind":"okx_order","recorded_at_ms":0}]}"#;
        fs::write(store.path(), evil).unwrap();
        assert!(matches!(store.load(), Err(StoreError::Corrupt("symbol"))));
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_at_the_path_is_refused() {
        let dir = Scratch::new();
        let target = dir.0.join("elsewhere");
        fs::write(&target, "{}").unwrap();
        let store = UnknownStore::in_dir(dir.0.clone());
        std::os::unix::fs::symlink(&target, store.path()).unwrap();
        assert!(store.load().is_err());
        assert!(store.add(&record("eAAAA1")).is_err());
    }
    #[test]
    fn the_same_id_of_two_accounts_is_two_entries() {
        let dir = Scratch::new();
        let store = UnknownStore::in_dir(dir.0.clone());
        let mut other = record("eAAAA1");
        other.account = AccountKey::from_bytes([6; 16]);
        store.add(&record("eAAAA1")).unwrap();
        store.add(&other).unwrap();
        store.add(&other).unwrap();
        assert_eq!(store.load().unwrap().len(), 2);
    }

    #[test]
    fn a_file_that_is_not_ours_fails_instead_of_loading_empty() {
        let dir = Scratch::new();
        let store = UnknownStore::in_dir(dir.0.clone());
        for text in [
            "{}",
            r#"{"entries":[]}"#,
            r#"{"version":2,"entries":[]}"#,
            r#"{"version":1}"#,
            r#"{"version":1,"entries":[],"extra":1}"#,
            r#"{"version":1,"entries":[{"account":"00000000-0000-0000-0000-000000000001","symbol":"BTC","client_id":"eAAAA1","kind":"okx_order","recorded_at_ms":0,"price":"1"}]}"#,
        ] {
            fs::write(store.path(), text).unwrap();
            assert!(
                matches!(store.load(), Err(StoreError::Corrupt(_))),
                "{text}"
            );
        }
    }

    #[test]
    fn a_file_over_the_size_cap_is_refused_and_a_directory_is_not_a_file() {
        let dir = Scratch::new();
        let store = UnknownStore::in_dir(dir.0.clone());
        fs::write(store.path(), vec![b' '; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert!(store.load().is_err());
        fs::remove_file(store.path()).unwrap();
        fs::create_dir(store.path()).unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn parallel_adds_and_removes_lose_no_entry() {
        let dir = Scratch::new();
        let store = UnknownStore::in_dir(dir.0.clone());
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let store = store.clone();
                std::thread::spawn(move || {
                    for i in 0..5 {
                        let id = format!("e{t}x{i}");
                        store.add(&record(&id)).unwrap();
                        if i % 2 == 0 {
                            assert!(store.remove(record(&id).account, &id).unwrap());
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        // 8 threads x (5 added - 3 removed)
        assert_eq!(store.load().unwrap().len(), 16);
    }

    #[test]
    fn the_store_is_a_journal_that_writes_ahead_and_clears() {
        use exchange::trade::UnknownJournal;
        let dir = Scratch::new();
        let store = UnknownStore::in_dir(dir.0.clone());
        let r = record("eAAAA1");
        store.record(&r).unwrap();
        assert_eq!(store.load().unwrap(), vec![r.clone()]);
        store.clear(r.account, &r.client_id).unwrap();
        assert!(store.load().unwrap().is_empty());
        // A broken file makes the journal fail, so the send is not made.
        fs::write(store.path(), "not json").unwrap();
        assert!(store.record(&r).is_err());
    }

    #[test]
    fn debug_output_hides_the_client_id() {
        let line = Line::from_record(&record("eAAAA1"));
        assert!(!format!("{line:?}").contains("eAAAA1"));
    }

    #[test]
    fn load_never_fails_while_another_thread_writes() {
        let dir = Scratch::new();
        let store = UnknownStore::in_dir(dir.0.clone());
        store.add(&record("eKEEP1")).unwrap();
        let writer = {
            let store = store.clone();
            std::thread::spawn(move || {
                for i in 0..200 {
                    let id = format!("eW{i}");
                    store.add(&record(&id)).unwrap();
                    store.remove(record(&id).account, &id).unwrap();
                }
            })
        };
        for _ in 0..200 {
            let loaded = store.load().expect("a write in progress is not corruption");
            assert!(loaded.iter().any(|r| r.client_id == "eKEEP1"));
        }
        writer.join().unwrap();
    }

    fn valid_id() -> String {
        exchange::trade::ClientId::generate(exchange::trade::Role::Entry)
            .as_str()
            .to_string()
    }

    #[test]
    fn only_a_record_with_an_invalid_id_is_discarded_and_nothing_else_goes() {
        let dir = Scratch::new();
        let store = UnknownStore::in_dir(dir.0.clone());
        let good = record(&valid_id());
        let bad = record("not-an-id-123");
        store.add(&good).unwrap();
        store.add(&bad).unwrap();

        // A live record cannot even start a discard.
        assert!(good.begin_discard().is_err());
        // The token of an invalid one removes exactly that line.
        let challenge = bad.begin_discard().unwrap();
        let phrase = challenge.phrase().to_string();
        let token = challenge.confirm(&phrase).unwrap();
        assert!(store.discard_invalid(token).unwrap());
        assert_eq!(store.load().unwrap(), vec![good.clone()]);

        // A second time there is nothing to remove.
        let again = bad.begin_discard().unwrap();
        let token = again.confirm(&phrase).unwrap();
        assert!(!store.discard_invalid(token).unwrap());

        assert_eq!(store.load().unwrap().len(), 1);
    }

    #[test]
    fn discard_logs_nothing_about_the_id_and_its_error_is_a_fixed_text() {
        assert_eq!(
            StoreError::StillValid.reason(),
            "record has a valid client id"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_at_the_path_is_refused_without_blocking() {
        let dir = Scratch::new();
        let store = UnknownStore::in_dir(dir.0.clone());
        let made = std::process::Command::new("mkfifo")
            .arg(store.path())
            .status()
            .expect("mkfifo runs");
        assert!(made.success());
        // Opening a FIFO for reading would block forever with no writer: run it on a thread
        // so a regression fails the test instead of hanging it.
        let (tx, rx) = std::sync::mpsc::channel();
        let s = store.clone();
        std::thread::spawn(move || {
            let loaded = s.load().is_err();
            let added = s.add(&record("eAAAA1")).is_err();
            let _ = tx.send((loaded, added));
        });
        let (loaded, added) = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the store must not block on a FIFO");
        assert!(loaded && added);
    }

    #[test]
    fn the_async_wrappers_do_the_same_work_off_the_calling_thread() {
        let dir = Scratch::new();
        let store = UnknownStore::in_dir(dir.0.clone());
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let rec = record(&valid_id());
            assert!(store.load_async().await.unwrap().is_empty());
            store.add_async(rec.clone()).await.unwrap();
            assert_eq!(store.load_async().await.unwrap(), vec![rec.clone()]);
            assert!(
                store
                    .remove_async(rec.account, rec.client_id.clone())
                    .await
                    .unwrap()
            );
            assert!(store.load_async().await.unwrap().is_empty());

            let bad = record("not-an-id-123");
            store.add_async(bad.clone()).await.unwrap();
            let challenge = bad.begin_discard().unwrap();
            let phrase = challenge.phrase().to_string();
            let token = challenge.confirm(&phrase).unwrap();
            assert!(store.discard_invalid_async(token).await.unwrap());
        });
    }

    fn discard_meta(account: [u8; 16]) -> AccountMeta {
        AccountMeta {
            id: Uuid::from_bytes(account),
            alias: "main".into(),
            ..AccountMeta::default()
        }
    }

    fn discard_token(rec: &UnknownRecord) -> DiscardToken {
        let challenge = rec.begin_discard().unwrap();
        let phrase = challenge.phrase().to_string();
        challenge.confirm(&phrase).unwrap()
    }

    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn a_discard_writes_one_audit_line_with_the_symbol_and_no_client_id() {
        let dir = Scratch::new();
        let store = UnknownStore::in_dir(dir.0.clone());
        let log = RiskLog::in_dir(dir.0.clone(), 1 << 20);
        let bad = record("not-an-id-123");
        store.add(&bad).unwrap();

        let out = run(store.discard_invalid_logged_async(
            discard_token(&bad),
            discard_meta([5; 16]),
            42,
            log.clone(),
        ))
        .unwrap();
        assert_eq!(
            out,
            DiscardOutcome {
                discarded: true,
                logged: true
            }
        );
        assert!(store.load().unwrap().is_empty());
        let text = fs::read_to_string(log.path()).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("unknown_discarded"), "{text}");
        assert!(text.contains("BTC-USDT-SWAP"), "{text}");
        assert!(!text.contains("not-an-id"), "{text}");
    }

    #[test]
    fn nothing_is_logged_when_nothing_was_removed_or_the_account_differs() {
        let dir = Scratch::new();
        let store = UnknownStore::in_dir(dir.0.clone());
        let log = RiskLog::in_dir(dir.0.clone(), 1 << 20);
        let bad = record("not-an-id-123");
        // Never saved: nothing to remove, so no audit line.
        let out = run(store.discard_invalid_logged_async(
            discard_token(&bad),
            discard_meta([5; 16]),
            1,
            log.clone(),
        ))
        .unwrap();
        assert!(!out.discarded && out.logged);
        // A token for another account is refused before the file is touched.
        store.add(&bad).unwrap();
        let err = run(store.discard_invalid_logged_async(
            discard_token(&bad),
            discard_meta([9; 16]),
            1,
            log.clone(),
        ))
        .unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(_)));
        assert_eq!(store.load().unwrap().len(), 1);
        assert!(!log.path().exists());
    }
}
