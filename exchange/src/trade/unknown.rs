//! The `Unknown` flow (condition 60): a send whose result is not known is never repeated.
//! The order is looked up by its client id with a backoff until it is found or the window
//! ends; after that it is reported as "not placed (presumed)". While it is unresolved the
//! (account, symbol) is locked against new entries (`TradeClient::is_locked`).
//!
//! "Not placed" needs all of: the last look was answered "no such order" (a verified code, see
//! [`is_not_found`], or an empty OKX `data`), and that look was made no earlier than
//! `sent_at + recvWindow + SETTLE_BUFFER_MS` (OKX: `sent_at + OKX_SETTLE_FLOOR_MS`). Any other answer, or silence, keeps the lock.

use super::binance_futures as fut;
use super::binance_spot as spot;
use super::client::{RECV_WINDOW_MS, ReadError, TradeClient};
use super::client_id::ClientId;
use super::journal::UnknownJournal;
use super::locks;
use super::okx;
use super::request::{Lookup, LookupKind};
use super::rules::SymbolRules;
use super::rules::TradeProduct;
use super::types::OrderRef;
use crate::account::{AccountError, AccountKey};
use std::time::Duration;

/// An order whose send came back `Unknown`. Hand it to `TradeClient::resolve_unknown`.
pub struct PendingUnknown {
    pub(crate) account: AccountKey,
    pub(crate) symbol: String,
    pub(crate) lookup: Lookup,
    pub(crate) rules: SymbolRules,
    /// When the send came back, on the client's monotonic clock (ms): a wall-clock jump or a
    /// clock sync cannot shorten the wait.
    pub(crate) sent_mono_ms: i64,
}

impl PendingUnknown {
    pub fn account(&self) -> AccountKey {
        self.account
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub fn client_id(&self) -> &str {
        &self.lookup.client_id
    }
}

// ── Persistence and manual release (review N2) ─────────────────────────

/// What must survive a restart about an unresolved send: which order to look for, nothing
/// else. No order parameters (size, price, side), no rules and no secrets: after a manual
/// release the order cannot be rebuilt from this. The client id is not a secret.
#[derive(Clone, PartialEq, Eq)]
pub struct UnknownRecord {
    pub account: AccountKey,
    pub symbol: String,
    pub client_id: String,
    /// How to look the order up, as [`UnknownRecord::kind_name`] writes it.
    pub kind: String,
    /// Wall-clock time (ms) the send came back unknown, for the user's eyes only; no wait is
    /// computed from it.
    pub recorded_at_ms: i64,
}

impl UnknownRecord {
    pub fn kind_name(kind: LookupKind) -> String {
        let algo = |t: okx::OkxAlgoType| {
            match t {
                okx::OkxAlgoType::Conditional => "conditional",
                okx::OkxAlgoType::Oco => "oco",
                okx::OkxAlgoType::MoveOrderStop => "move_order_stop",
                okx::OkxAlgoType::Trigger => "trigger",
                okx::OkxAlgoType::Chase => "chase",
                okx::OkxAlgoType::Iceberg => "iceberg",
                okx::OkxAlgoType::Twap => "twap",
                okx::OkxAlgoType::SmartIceberg => "smart_iceberg",
            }
            .to_string()
        };
        match kind {
            LookupKind::FuturesOrder => "futures_order".into(),
            LookupKind::FuturesAlgo => "futures_algo".into(),
            LookupKind::SpotOrder => "spot_order".into(),
            LookupKind::SpotList => "spot_list".into(),
            LookupKind::OkxOrder => "okx_order".into(),
            LookupKind::OkxAlgo(t) => format!("okx_algo:{}", algo(t)),
        }
    }

    /// The record of a send that has just been made (or is about to be).
    pub(crate) fn for_send(
        account: AccountKey,
        symbol: &str,
        lookup: &Lookup,
        now_ms: i64,
    ) -> UnknownRecord {
        UnknownRecord {
            account,
            symbol: symbol.to_string(),
            client_id: lookup.client_id.clone(),
            kind: UnknownRecord::kind_name(lookup.kind),
            recorded_at_ms: now_ms,
        }
    }

    /// Whether the client id is one this program could have made. A record that fails this
    /// cannot be tied to a symbol lock (its account stays blocked for new entries), cannot be
    /// looked up and cannot be released like an unresolved send: it can only be discarded
    /// ([`UnknownRecord::begin_discard`]).
    pub fn has_valid_client_id(&self) -> bool {
        ClientId::parse(&self.client_id).is_ok()
    }

    /// Step one of discarding a record whose client id is invalid (review N4): a challenge
    /// naming it. Refused for a record with a valid id, which must go through a lookup or the
    /// manual release instead, so this path cannot be used to forget a live unresolved send.
    pub fn begin_discard(&self) -> Result<DiscardChallenge, DiscardError> {
        if self.has_valid_client_id() {
            return Err(DiscardError::StillValid);
        }
        let tail: String = self
            .client_id
            .chars()
            .skip(self.client_id.chars().count().saturating_sub(5))
            .collect();
        Ok(DiscardChallenge {
            phrase: format!("discard {} {tail}", self.symbol),
            account: self.account,
            symbol: self.symbol.clone(),
            client_id: self.client_id.clone(),
        })
    }

    pub(crate) fn parse_kind(name: &str) -> Option<LookupKind> {
        Some(match name {
            "futures_order" => LookupKind::FuturesOrder,
            "futures_algo" => LookupKind::FuturesAlgo,
            "spot_order" => LookupKind::SpotOrder,
            "spot_list" => LookupKind::SpotList,
            "okx_order" => LookupKind::OkxOrder,
            other => {
                let t = other.strip_prefix("okx_algo:")?;
                LookupKind::OkxAlgo(okx::OkxAlgoType::ALL_PENDING.into_iter().find(|a| {
                    Self::kind_name(LookupKind::OkxAlgo(*a)) == format!("okx_algo:{t}")
                })?)
            }
        })
    }
}

/// Why a record could not be turned back into a pending unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreError {
    /// The record belongs to another account than this client.
    WrongAccount,
    /// The kind is not one this build knows.
    UnknownKind,
    /// The rules are for another symbol or product than the record.
    RulesMismatch,
    /// The client id is not one this program could have made.
    BadClientId,
}

fn kind_fits(kind: LookupKind, product: TradeProduct) -> bool {
    match kind {
        LookupKind::FuturesOrder | LookupKind::FuturesAlgo => product == TradeProduct::BinanceUsdM,
        LookupKind::SpotOrder | LookupKind::SpotList => product == TradeProduct::BinanceSpot,
        LookupKind::OkxOrder | LookupKind::OkxAlgo(_) => {
            matches!(product, TradeProduct::OkxSpot | TradeProduct::OkxSwap)
        }
    }
}

/// The user's second, separate confirmation that an unresolved send may be forgotten.
/// Step one is [`PendingUnknown::begin_manual_release`], which returns a challenge naming the
/// order; step two is typing its phrase into [`ReleaseChallenge::confirm`].
pub struct ReleaseChallenge {
    account: AccountKey,
    symbol: String,
    client_id: String,
    phrase: String,
}

impl ReleaseChallenge {
    /// The text the user must type: the symbol and the end of the client id.
    pub fn phrase(&self) -> &str {
        &self.phrase
    }

    pub fn account(&self) -> AccountKey {
        self.account
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// `Ok` only when `typed` equals the phrase exactly; otherwise the challenge comes back.
    pub fn confirm(self, typed: &str) -> Result<ManualReleaseToken, Box<ReleaseChallenge>> {
        if typed == self.phrase {
            Ok(ManualReleaseToken {
                account: self.account,
                symbol: self.symbol,
                client_id: self.client_id,
            })
        } else {
            Err(Box::new(self))
        }
    }
}

/// Why a record cannot be discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardError {
    /// The record's client id is valid. It stands for a real unresolved send: look it up or
    /// release it by hand, do not discard it.
    StillValid,
}

/// The user's second, separate confirmation that a saved record with an invalid client id may be
/// dropped from the file (review N4). Same shape as [`ReleaseChallenge`]: the phrase must be
/// typed by the user, never filled in or confirmed by the program.
pub struct DiscardChallenge {
    account: AccountKey,
    symbol: String,
    client_id: String,
    phrase: String,
}

impl DiscardChallenge {
    /// The text the user must type: `discard`, the symbol and the end of the client id.
    pub fn phrase(&self) -> &str {
        &self.phrase
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    /// `Ok` only when `typed` equals the phrase exactly; otherwise the challenge comes back.
    pub fn confirm(self, typed: &str) -> Result<DiscardToken, Box<DiscardChallenge>> {
        if typed == self.phrase {
            Ok(DiscardToken {
                account: self.account,
                symbol: self.symbol,
                client_id: self.client_id,
            })
        } else {
            Err(Box::new(self))
        }
    }
}

/// Single use, no `Clone`: made only by [`DiscardChallenge::confirm`]. The store removes the
/// named line with it (and only if that line's id is still invalid).
pub struct DiscardToken {
    account: AccountKey,
    symbol: String,
    client_id: String,
}

impl DiscardToken {
    pub fn account(&self) -> AccountKey {
        self.account
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }
}

/// Single use, no `Clone`: made only by [`ReleaseChallenge::confirm`]. It names the account it
/// was made for: another account's client refuses it.
pub struct ManualReleaseToken {
    account: AccountKey,
    symbol: String,
    client_id: String,
}

/// Why a manual release did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseError {
    /// The token was made for another account than this client.
    WrongAccount,
    /// This account has no unresolved send with that id (already resolved or released).
    NotLocked,
}

/// An unresolved send that the user chose to forget. It carries no order parameters: the
/// order is not rebuilt and nothing is resent; a new entry is a new order with a new id.
#[derive(PartialEq, Eq)]
pub struct ReleasedUnknown {
    pub account: AccountKey,
    pub symbol: String,
    pub client_id: String,
}

impl PendingUnknown {
    /// Step one of a manual release. Consumes the pending value: from here on the order is
    /// either confirmed away or looked up again from its record.
    pub fn begin_manual_release(self) -> ReleaseChallenge {
        ReleaseChallenge::new(self.account, self.symbol, self.lookup.client_id)
    }
}

impl ReleaseChallenge {
    fn new(account: AccountKey, symbol: String, client_id: String) -> Self {
        let tail: String = client_id
            .chars()
            .skip(client_id.chars().count().saturating_sub(5))
            .collect();
        ReleaseChallenge {
            phrase: format!("{symbol} {tail}"),
            account,
            symbol,
            client_id,
        }
    }
}

/// Default window: look at once, then after 1, 2, 4, 8, 15 and 30 seconds (about one minute).
pub const DEFAULT_SCHEDULE: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(15),
    Duration::from_secs(30),
];

/// Extra wait after `recvWindow` before a "no such order" answer may settle the question: the
/// exchange can still be placing an order it accepted inside the window.
pub const SETTLE_BUFFER_MS: i64 = 10_000;

/// Floor for OKX orders. They carry no `recvWindow` or expiry time here, so the exchange has
/// no time limit for placing one; this is a longer, conservative wait of its own.
pub const OKX_SETTLE_FLOOR_MS: i64 = 60_000;

/// The earliest time (client monotonic clock) at which "not found" counts.
fn settled_at(kind: LookupKind, sent_mono_ms: i64) -> i64 {
    let floor = match kind {
        LookupKind::OkxOrder | LookupKind::OkxAlgo(_) => OKX_SETTLE_FLOOR_MS,
        LookupKind::SpotOrder
        | LookupKind::SpotList
        | LookupKind::FuturesOrder
        | LookupKind::FuturesAlgo => i64::from(RECV_WINDOW_MS).saturating_add(SETTLE_BUFFER_MS),
    };
    sent_mono_ms.saturating_add(floor)
}

/// Error codes that mean "this order does not exist", per lookup. Everything else (rate
/// limits, `50004` timeouts, bad keys, `-1001`, `-1008`, ...) is not an answer about the
/// order. Only codes verified against the docs are listed: Binance spot `GET order` `-2013`
/// (`errors.md`), OKX `GET order` `51603`. USDⓈ-M `order` / `algoOrder`, spot `orderList` and
/// OKX `order-algo` are unverified (testnet to-do), so their errors are never "not found".
fn is_not_found(kind: LookupKind, code: &str) -> bool {
    match kind {
        LookupKind::SpotOrder => code == "-2013",
        LookupKind::OkxOrder => code == "51603",
        LookupKind::FuturesOrder
        | LookupKind::FuturesAlgo
        | LookupKind::SpotList
        | LookupKind::OkxAlgo(_) => false,
    }
}

#[derive(Debug)]
pub enum Resolution {
    /// The exchange knows the order. This is not a fill: it may be open, filled, partly
    /// filled or already cancelled. The lock is released; read the order or the position to
    /// learn its state.
    Found,
    /// The exchange answered "no such order" within the window and never showed it: presumed
    /// not placed (未成立（推定）). Nothing was resent.
    NotPlacedPresumed,
    /// No lookup got an answer (the exchange or the network stayed silent). The symbol stays
    /// locked and nothing may be assumed. This is not "not placed": try `resolve_unknown`
    /// again with this value, or confirm by hand (see the persisted unknown list).
    Unresolved(Box<PendingUnknown>),
}

/// What one lookup learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    Found,
    /// The exchange answered and does not know the order.
    NotFound,
    /// Timeout, transport error, bad response, throttling: nothing learned.
    NoAnswer,
}

/// Lock the (account, symbol) of a saved record without a [`TradeClient`] (review B6). A client
/// needs credentials, which can be unavailable at startup (a locked Keychain); the lock must not
/// depend on them. Call it for every record **before** any client is built.
///
/// The record names its own account and that is the one locked: there is no second account to
/// pass, so a record can never be checked against, and then left unlocked for, another one.
/// Every `Err` means the record is kept and shown, never skipped or deleted: the lock (or, for
/// a bad id, the block of the whole account) is already in place when it returns.
pub fn lock_from_record(record: &UnknownRecord) -> Result<(), RestoreError> {
    let account = record.account;
    if ClientId::parse(&record.client_id).is_err() {
        locks::block_entries(account);
        log::error!("a saved unresolved order has a bad id: new entries are blocked");
        return Err(RestoreError::BadClientId);
    }
    if !locks::has_unresolved_id(account, &record.symbol, &record.client_id) {
        locks::lock_unknown(account, &record.symbol, Some(&record.client_id));
    }
    log::warn!(
        "restored an unresolved order for {}: symbol stays locked until it is resolved",
        record.symbol
    );
    Ok(())
}

/// Refuse every new entry of every account until the program restarts (review B6). For a startup
/// where the saved unresolved sends could not be read (`UnknownStore::load` failed): nothing is
/// known about which symbols are locked, so none may open. Closes and cancels still go out.
pub fn block_all_entries() {
    locks::block_all();
    log::error!("saved unresolved orders unreadable: all new entries are blocked");
}

impl TradeClient {
    /// What to save for `pending` so a restart does not forget it.
    pub fn unknown_record(&self, pending: &PendingUnknown) -> UnknownRecord {
        UnknownRecord::for_send(
            self.account,
            &pending.symbol,
            &pending.lookup,
            self.now_ms(),
        )
    }

    /// First step of loading a saved record at startup: lock the (account, symbol) against new
    /// entries. It needs only the account and a client id this program could have made, so it
    /// works before any trading rules are known (no network, no delisted symbol can stop it).
    /// If the id is not valid, the record cannot be tied to a symbol lock, so **all** new
    /// entries of the account stay refused until restart, and the record must be kept for the
    /// user to see. Nothing is unlocked by loading, and nothing is resent.
    pub fn lock_from_record(&self, record: &UnknownRecord) -> Result<(), RestoreError> {
        // A record of another account is not this client's to lock. (The free function does
        // lock it, by its own account.)
        if record.account != self.account {
            return Err(RestoreError::WrongAccount);
        }
        lock_from_record(record)
    }

    /// Second step: build the value to hand to [`TradeClient::resolve_unknown`] once fresh
    /// `rules` of the record's symbol are known (it uses [`DEFAULT_SCHEDULE`] and waits the
    /// full settle time again, counted from now, so a restart can only lengthen the wait).
    /// Locks nothing itself; call [`TradeClient::lock_from_record`] first. A record that fails
    /// here must be kept: its lock stays, and the user can release it by hand
    /// ([`TradeClient::begin_manual_release_of_record`]).
    pub fn pending_from_record(
        &self,
        record: &UnknownRecord,
        rules: &SymbolRules,
    ) -> Result<PendingUnknown, RestoreError> {
        if record.account != self.account {
            return Err(RestoreError::WrongAccount);
        }
        let kind = UnknownRecord::parse_kind(&record.kind).ok_or(RestoreError::UnknownKind)?;
        if rules.symbol().as_str() != record.symbol || !kind_fits(kind, rules.product()) {
            return Err(RestoreError::RulesMismatch);
        }
        ClientId::parse(&record.client_id).map_err(|_| RestoreError::BadClientId)?;
        Ok(PendingUnknown {
            account: self.account,
            symbol: record.symbol.clone(),
            lookup: Lookup {
                kind,
                client_id: record.client_id.clone(),
            },
            rules: rules.clone(),
            sent_mono_ms: self.mono_ms(),
        })
    }

    /// Both steps at once. The lock is made first, so a record that fails the second step still
    /// leaves its symbol locked.
    pub fn restore_unknown(
        &self,
        record: &UnknownRecord,
        rules: &SymbolRules,
    ) -> Result<PendingUnknown, RestoreError> {
        self.lock_from_record(record)?;
        self.pending_from_record(record, rules)
    }

    /// Step one of a manual release for a record that has no pending value (its rules could not
    /// be loaded, or it failed validation). Only a locked record of this account qualifies.
    pub fn begin_manual_release_of_record(
        &self,
        record: &UnknownRecord,
    ) -> Result<ReleaseChallenge, RestoreError> {
        if record.account != self.account {
            return Err(RestoreError::WrongAccount);
        }
        Ok(ReleaseChallenge::new(
            self.account,
            record.symbol.clone(),
            record.client_id.clone(),
        ))
    }

    /// Step two of a manual release: forget the unresolved send and unlock new entries for
    /// that symbol. The caller must also remove the saved record. Never resends anything. The
    /// token must be for this client's account, and the send must still be locked; otherwise
    /// nothing is released and the error says why (nothing is logged as released).
    pub fn release_unknown_manually(
        &self,
        token: ManualReleaseToken,
    ) -> Result<ReleasedUnknown, ReleaseError> {
        if token.account != self.account {
            return Err(ReleaseError::WrongAccount);
        }
        if !self.unlock(&token.symbol, &token.client_id) {
            return Err(ReleaseError::NotLocked);
        }
        log::warn!(
            "an unresolved order on {} was released by the user",
            token.symbol
        );
        Ok(ReleasedUnknown {
            account: token.account,
            symbol: token.symbol,
            client_id: token.client_id,
        })
    }

    /// Look the order up by client id, waiting [`DEFAULT_SCHEDULE`] between looks (the settle
    /// floor below applies on top). Never sends an order.
    /// The result is `NotPlacedPresumed` only when the last look was answered "no such order"
    /// and was made after `sent_at + recvWindow + SETTLE_BUFFER_MS` (OKX: `sent_at + OKX_SETTLE_FLOOR_MS`); if the schedule ends
    /// earlier, one more look is made at that time. Otherwise, with no conclusive answer, the
    /// result is `Unresolved` and the lock stays.
    ///
    /// `journal`: a `Found` or `NotPlacedPresumed` result clears the record of the send. A
    /// failed clear is only logged; the record stays and the next start looks the order up again.
    pub async fn resolve_unknown(
        &self,
        pending: PendingUnknown,
        journal: &dyn UnknownJournal,
    ) -> Resolution {
        let (account, client_id) = (pending.account, pending.lookup.client_id.clone());
        let resolution = self
            .resolve_unknown_with_schedule(pending, &DEFAULT_SCHEDULE)
            .await;
        if !matches!(resolution, Resolution::Unresolved(_))
            && journal.clear(account, &client_id).is_err()
        {
            log::warn!("could not clear the journal record of a resolved send");
        }
        resolution
    }

    /// Same as [`TradeClient::resolve_unknown`] with the waits given by the caller. Crate-private
    /// on purpose: the public entry point always uses [`DEFAULT_SCHEDULE`], so no caller can
    /// pass a window shorter than the default. Tests use it to inject short waits.
    pub(crate) async fn resolve_unknown_with_schedule(
        &self,
        pending: PendingUnknown,
        schedule: &[Duration],
    ) -> Resolution {
        let settled = settled_at(pending.lookup.kind, pending.sent_mono_ms);
        let mut waits = schedule.iter().copied();
        let mut extended = false;
        let (last, at) = loop {
            // Taken before the query goes out: an answer reflects the exchange at the time the
            // request arrived, not at the time the reply came back.
            let at = self.mono_ms();
            let look = self.lookup_once(&pending).await;
            if look == Look::Found {
                self.unlock(&pending.symbol, pending.client_id());
                return Resolution::Found;
            }
            let next = match waits.next() {
                Some(w) => Some(w),
                None if look == Look::NotFound && at < settled && !extended => {
                    extended = true;
                    let rest = u64::try_from(settled - at).unwrap_or(0);
                    Some(Duration::from_millis(rest))
                }
                None => None,
            };
            match next {
                Some(w) => self.wait(w).await,
                None => break (look, at),
            }
        };
        if last == Look::NotFound && at >= settled {
            self.unlock(&pending.symbol, pending.client_id());
            Resolution::NotPlacedPresumed
        } else {
            Resolution::Unresolved(Box::new(pending))
        }
    }

    /// One query by client id. Only a verified "no such order" answer counts as "not found"
    /// (see [`is_not_found`]); a timeout, a transport error, throttling or any other exchange
    /// error teaches nothing.
    async fn lookup_once(&self, pending: &PendingUnknown) -> Look {
        match self.query_lookup(&pending.rules, &pending.lookup).await {
            Ok(true) => Look::Found,
            Ok(false) => Look::NotFound,
            Err(_) => Look::NoAnswer,
        }
    }

    /// `Ok(true)`: the exchange knows the order. `Ok(false)`: it answered that it does not
    /// (verified code or empty data). Any other failure is an `Err`.
    pub(crate) async fn query_lookup(
        &self,
        rules: &SymbolRules,
        lookup: &Lookup,
    ) -> Result<bool, ReadError> {
        match self.query_lookup_raw(rules, lookup).await {
            Err(ReadError::Transport(AccountError::Exchange { code, .. }))
                if is_not_found(lookup.kind, &code) =>
            {
                Ok(false)
            }
            other => other,
        }
    }

    async fn query_lookup_raw(
        &self,
        rules: &SymbolRules,
        lookup: &Lookup,
    ) -> Result<bool, ReadError> {
        let by_client =
            OrderRef::Client(ClientId::parse(&lookup.client_id).map_err(ReadError::from)?);
        match lookup.kind {
            LookupKind::FuturesOrder => {
                let v = self
                    .read_binance(&fut::query_order(rules, &by_client)?)
                    .await?;
                Ok(v.is_object())
            }
            LookupKind::FuturesAlgo => {
                let v = self
                    .read_binance(&fut::query_algo_order(&by_client)?)
                    .await?;
                Ok(v.is_object())
            }
            LookupKind::SpotOrder => {
                let v = self
                    .read_binance(&spot::query_order(rules, &by_client)?)
                    .await?;
                Ok(v.is_object())
            }
            LookupKind::SpotList => {
                let v = self
                    .read_binance(&spot::query_order_list(&by_client)?)
                    .await?;
                Ok(v.is_object())
            }
            LookupKind::OkxOrder => {
                let d = self.read_okx(&okx::query_order(rules, &by_client)?).await?;
                Ok(!d.is_empty())
            }
            LookupKind::OkxAlgo(t) => {
                let d = self.read_okx(&okx::query_algo(t, &by_client)?).await?;
                Ok(!d.is_empty())
            }
        }
    }
}

// Debug output never carries a client id or the typed phrase (which ends in one).
impl std::fmt::Debug for PendingUnknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingUnknown")
            .field("symbol", &self.symbol)
            .field("client_id", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for UnknownRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnknownRecord")
            .field("symbol", &self.symbol)
            .field("client_id", &"<redacted>")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ReleaseChallenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReleaseChallenge")
            .field("symbol", &self.symbol)
            .field("client_id", &"<redacted>")
            .field("phrase", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ManualReleaseToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManualReleaseToken")
            .field("symbol", &self.symbol)
            .field("client_id", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for DiscardChallenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscardChallenge")
            .field("symbol", &self.symbol)
            .field("client_id", &"<redacted>")
            .field("phrase", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for DiscardToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscardToken")
            .field("symbol", &self.symbol)
            .field("client_id", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ReleasedUnknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReleasedUnknown")
            .field("symbol", &self.symbol)
            .field("client_id", &"<redacted>")
            .finish_non_exhaustive()
    }
}
