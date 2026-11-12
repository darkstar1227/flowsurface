//! Cancel-all, close-all and unknown results (M5 c2b-3): the background steps and the texts.
//!
//! This is the one file of the GUI that calls `cancel_all`, `flatten`, `resolve_unknown`,
//! `release_unknown_manually` and `discard_invalid_logged_async`. Every one of them is given a
//! token the user's confirmation made (`modal::bulk_confirm`, `modal::account_panel`): nothing
//! here makes one. Requests go through `SignedHttp` only. The Keychain is read inside each step
//! and the client is dropped at its end. Nothing here uses `submit_auto` or `AutoIntent`.
//!
//! Every step returns a `Clone` summary: tokens, challenges and `PendingUnknown` never cross a
//! message. A summary holds no credential and no full client id (an `UnknownRecord` is kept
//! whole because the state needs it, and its `Debug` hides the id).

use crate::order_flow::{
    PREPARE_TIMEOUT, clip, describe_read_error, describe_store_error, market_of, trade_market,
    within,
};
use crate::trading::{LookupEnd, account_key};
use crate::widget::BannerKind;
use data::config::exchange_accounts::{
    AccountMeta, StoreError, load_credentials, load_credentials_with_identity,
};
use data::config::risk_log::{LogAction, RiskLog, RiskLogEntry};
use data::config::trade_gate::{SessionEntry, SessionGate};
use data::config::unknown_store::{DiscardOutcome, UnknownStore};
use exchange::account::{ProxyState, check_proxy_snapshot};
use exchange::signed_http::SignedHttp;
use exchange::trade::{
    BulkAction, BulkError, BulkReport, ConfirmToken, DiscardToken, ManualReleaseToken, MarginMode,
    PendingUnknown, ReleaseChallenge, ReleaseError, Resolution, RestoreError, SharedRisk,
    TradeClient, UnknownRecord,
};
use iced::futures::future::join_all;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use uuid::Uuid;

pub const PREPARE_BULK_TIMED_OUT: &str =
    "Reading the account took too long and was stopped. Nothing was sent. Try again.";
pub const BEGIN_RELEASE_TIMED_OUT: &str =
    "Preparing the release took too long and was stopped. Nothing was changed. Try again.";

/// R10: a bulk confirmation that does not fit the account and action. Not "unsupported".
pub const BULK_MISMATCH: &str = "This confirmation was not made for this account and action. \
     Nothing was sent. Open the confirmation again.";

/// What the background steps need to make a client: nothing secret, the Keychain is read inside.
pub struct ClientInput {
    pub meta: AccountMeta,
    pub signed: SignedHttp,
    pub proxy: Arc<dyn Fn() -> ProxyState + Send + Sync>,
}

/// Keychain → client → clock. Under the overall limit (I1): a Keychain prompt nobody answers
/// ends here instead of freezing the form for good.
async fn client_for(input: ClientInput, timed_out: &str) -> Result<TradeClient, String> {
    within(
        PREPARE_TIMEOUT,
        async {
            let ClientInput {
                meta,
                signed,
                proxy,
            } = input;
            let creds = {
                let meta = meta.clone();
                tokio::task::spawn_blocking(move || load_credentials(&meta))
                    .await
                    .map_err(|_| "Keychain access did not complete.".to_string())?
                    .map_err(|e| describe_store_error(&e))?
            };
            let client = TradeClient::new(
                signed,
                meta.venue,
                meta.env,
                meta.product,
                account_key(meta.id),
                creds,
                proxy,
            );
            // Stores the measured offset in the client; a failure keeps offset zero.
            let _ = client.sync_clock().await;
            Ok(client)
        },
        timed_out,
    )
    .await
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ── Messages of the steps ───────────────────────────────────────────────

/// The result of a step, back in `main`. No credential, no token, no full client id.
#[derive(Debug, Clone)]
pub enum Event {
    /// The reads before a bulk confirmation came back (the number tells which request).
    /// The window the confirmation opens in comes along with the answer.
    BulkPrepared(u64, iced::window::Id, Result<Box<BulkPreview>, String>),
    BulkDone(Box<BulkSummary>),
    /// The saved unresolved sends, read from the file.
    RecordsLoaded(Result<Vec<UnknownRecord>, String>),
    ReleaseBegun(Box<UnknownRecord>, Result<ChallengeSlot, String>),
    Released(Box<ReleaseSummary>),
    Discarded(Box<DiscardSummary>),
    Looked(Box<LookupSummary>),
}

// ── Cancel all / close all ──────────────────────────────────────────────

/// What the user asked for: the account is the one the confirmation was opened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BulkRequest {
    pub account: Uuid,
    pub action: BulkAction,
}

/// What a fresh read of the account found, for the confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkPreview {
    pub request: BulkRequest,
    pub orders: usize,
    pub order_symbols: Vec<String>,
    pub positions: usize,
    pub position_symbols: Vec<String>,
}

fn symbols_line(label: &str, symbols: &[String]) -> Option<String> {
    const SHOWN: usize = 8;
    if symbols.is_empty() {
        return None;
    }
    let mut shown: Vec<&str> = symbols.iter().take(SHOWN).map(String::as_str).collect();
    if symbols.len() > SHOWN {
        shown.push("…");
    }
    Some(format!("{label}: {}", shown.join(", ")))
}

impl BulkPreview {
    /// There is nothing for the action to do: the confirmation says so and cannot be pressed.
    pub fn nothing_to_do(&self) -> bool {
        match self.request.action {
            BulkAction::CancelAll => self.orders == 0,
            BulkAction::Flatten => self.positions == 0,
        }
    }

    /// The count lines of the confirmation. The value is said to be unknown and never shown as
    /// zero: open orders are read without prices, and this build has no mark price for positions.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self.request.action {
            BulkAction::CancelAll => {
                out.push(format!(
                    "Cancel ALL open orders: {} order(s) on {} symbol(s).",
                    self.orders,
                    self.order_symbols.len()
                ));
                out.extend(symbols_line("Symbols", &self.order_symbols));
                if self.orders > 0 {
                    out.push(
                        "Total value of the orders: unknown (open orders are read without \
                         prices)."
                            .to_string(),
                    );
                }
                out.push(format!("Positions ({}) are not touched.", self.positions));
            }
            BulkAction::Flatten => {
                out.push(format!(
                    "Close ALL positions at market: {} position(s) on {} symbol(s).",
                    self.positions,
                    self.position_symbols.len()
                ));
                out.extend(symbols_line("Symbols", &self.position_symbols));
                if self.positions > 0 {
                    out.push(
                        "Total value of the positions: unknown (this build has no mark price). \
                         It is not zero."
                            .to_string(),
                    );
                }
                out.push(format!("Open orders ({}) are not cancelled.", self.orders));
            }
        }
        if self.nothing_to_do() {
            out.push("There is nothing to do for this action.".to_string());
        }
        out.push(
            "The result is decided by a fresh read after sending, not by the requests.".to_string(),
        );
        out
    }
}

pub struct BulkPrepareInput {
    pub client: ClientInput,
    pub request: BulkRequest,
}

/// Read the account (open orders, and positions unless spot) for the confirmation. Reads only,
/// under the overall limit.
pub async fn prepare_bulk(input: BulkPrepareInput) -> Result<BulkPreview, String> {
    let BulkPrepareInput { client, request } = input;
    let spot = client.meta.product == exchange::account::AccountProduct::Spot;
    within(
        PREPARE_TIMEOUT,
        async move {
            let client = client_for(client, PREPARE_BULK_TIMED_OUT).await?;
            let orders = client
                .open_orders()
                .await
                .map_err(|e| describe_read_error(&e))?;
            let positions = if spot {
                Vec::new()
            } else {
                client
                    .positions()
                    .await
                    .map_err(|e| describe_read_error(&e))?
            };
            Ok(BulkPreview {
                request,
                orders: orders.len(),
                order_symbols: unique_sorted(orders.iter().map(|o| o.symbol.clone())),
                positions: positions.len(),
                position_symbols: unique_sorted(positions.iter().map(|p| p.symbol.clone())),
            })
        },
        PREPARE_BULK_TIMED_OUT,
    )
    .await
}

fn unique_sorted(items: impl Iterator<Item = String>) -> Vec<String> {
    let set: std::collections::BTreeSet<String> = items.collect();
    set.into_iter().collect()
}

/// What a bulk send found, from the fresh read after the requests.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BulkNumbers {
    pub sent: usize,
    pub failed: usize,
    pub remaining: usize,
    pub remaining_symbols: Vec<String>,
    pub reread_failed: bool,
    pub unknown_algo_types: usize,
    pub journal_failed: bool,
    /// Sends whose result was unknown and which the lookups settled as found.
    pub found: usize,
    pub not_placed: usize,
    /// Symbols whose send is still unknown after the lookups (they stay locked).
    pub unresolved: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BulkOutcome {
    Done(BulkNumbers),
    /// R10.
    Mismatch,
    /// Nothing was sent: the reason.
    NotDone(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkSummary {
    pub request: BulkRequest,
    pub outcome: BulkOutcome,
}

pub struct BulkSendInput {
    pub client: ClientInput,
    pub session: Option<SessionEntry>,
    pub risk: SharedRisk,
    pub token: ConfirmToken,
    pub request: BulkRequest,
}

fn numbers<T>(report: &BulkReport<T>, symbol_of: impl Fn(&T) -> String) -> BulkNumbers {
    BulkNumbers {
        sent: report.sent,
        failed: report.failed,
        remaining: report.remaining.len(),
        remaining_symbols: unique_sorted(report.remaining.iter().map(symbol_of)),
        reread_failed: report.reread_failed,
        unknown_algo_types: report.unknown_algo_types.len(),
        journal_failed: report.journal_failed,
        ..BulkNumbers::default()
    }
}

pub fn describe_bulk_error(e: &BulkError) -> BulkOutcome {
    match e {
        BulkError::ConfirmationMismatch => BulkOutcome::Mismatch,
        BulkError::Read(e) => BulkOutcome::NotDone(describe_read_error(e)),
        BulkError::Trade(e) => BulkOutcome::NotDone(format!("The request cannot be made: {e}.")),
    }
}

/// The step before the first bulk request ran too long. Nothing was sent.
pub const BULK_SETUP_TIMED_OUT: &str = "Preparing the request took too long and was stopped. \
     Nothing was sent. Try again.";

/// Runs the steps before the first request under a limit; a stop is `NotDone` (nothing sent).
async fn bulk_setup_within<T>(
    limit: std::time::Duration,
    work: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, BulkOutcome> {
    within(limit, work, BULK_SETUP_TIMED_OUT)
        .await
        .map_err(BulkOutcome::NotDone)
}

/// Keychain (with identity) → client → rules → gate → `cancel_all` / `flatten` → lookups of the
/// unknown results. Never resends. Not under an overall time limit: dropping it after requests went out
/// would lose the report. Only the steps before the first request run under a limit.
pub async fn send_bulk(input: BulkSendInput) -> BulkSummary {
    let BulkSendInput {
        client: ClientInput {
            meta,
            signed,
            proxy,
        },
        session,
        risk,
        token,
        request,
    } = input;
    let done = |outcome: BulkOutcome| BulkSummary { request, outcome };
    // Before the first request: the Keychain read (it can wait for a prompt nobody answers), the
    // clock and the rules. Nothing is sent yet, so this part may be stopped at any time.
    let setup = bulk_setup_within(PREPARE_TIMEOUT, async {
        let loaded = {
            let meta = meta.clone();
            tokio::task::spawn_blocking(move || load_credentials_with_identity(&meta)).await
        };
        let (creds, identity) = match loaded {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => return Err(describe_store_error(&e)),
            Err(_) => return Err("Keychain access did not complete.".to_string()),
        };
        let proxy_ok = check_proxy_snapshot(signed.proxy_state(), &proxy()).is_ok();
        let client = TradeClient::new(
            signed,
            meta.venue,
            meta.env,
            meta.product,
            account_key(meta.id),
            creds,
            proxy,
        );
        let _ = client.sync_clock().await;
        let Some(market) = trade_market(market_of(meta.venue, meta.product)) else {
            return Err("This account's market cannot be traded.".to_string());
        };
        let table = client
            .fetch_rules(market.rules)
            .await
            .map_err(|e| describe_read_error(&e))?;
        Ok((client, table, identity, proxy_ok))
    })
    .await;
    let (client, table, identity, proxy_ok) = match setup {
        Ok(ready) => ready,
        Err(outcome) => return done(outcome),
    };
    let keychain: Result<_, StoreError> = Ok(identity);
    let gate = SessionGate {
        session: session.as_ref(),
        keychain: &keychain,
        saved: &meta,
        proxy_ok,
    };
    let journal = UnknownStore::in_data_dir();
    let (mut n, pending) = match request.action {
        BulkAction::CancelAll => {
            match client
                .cancel_all(&gate, &risk, &journal, token, &table)
                .await
            {
                Ok(mut report) => {
                    let pending = std::mem::take(&mut report.pending);
                    (numbers(&report, |o| o.symbol.clone()), pending)
                }
                Err(e) => return done(describe_bulk_error(&e)),
            }
        }
        BulkAction::Flatten => {
            match client
                .flatten(&gate, &risk, &journal, token, &table, MarginMode::Cross)
                .await
            {
                Ok(mut report) => {
                    let pending = std::mem::take(&mut report.pending);
                    (numbers(&report, |p| p.symbol.clone()), pending)
                }
                Err(e) => return done(describe_bulk_error(&e)),
            }
        }
    };
    // The unknown sends are looked up (reads only); the ones that stay unknown are reported so
    // the GUI keeps their symbols locked.
    let looked = join_all(pending.into_iter().map(|p: PendingUnknown| {
        let client = &client;
        let journal = &journal;
        async move {
            let symbol = p.symbol().to_string();
            (symbol, client.resolve_unknown(p, journal).await)
        }
    }))
    .await;
    for (symbol, resolution) in looked {
        match resolution {
            Resolution::Found => n.found += 1,
            Resolution::NotPlacedPresumed => n.not_placed += 1,
            Resolution::Unresolved(_) => n.unresolved.push(symbol),
        }
    }
    done(BulkOutcome::Done(n))
}

/// The lines under the panel after a bulk send, and how loud they are. Every field of the report
/// has a sentence (a table test checks that).
pub fn describe_bulk(action: BulkAction, outcome: &BulkOutcome) -> (Vec<String>, BannerKind) {
    let what = match action {
        BulkAction::CancelAll => "cancel all",
        BulkAction::Flatten => "close all",
    };
    let n = match outcome {
        BulkOutcome::Mismatch => return (vec![BULK_MISMATCH.to_string()], BannerKind::Error),
        BulkOutcome::NotDone(why) => {
            return (
                vec![format!("The {what} was not done: {}", clip(why))],
                BannerKind::Error,
            );
        }
        BulkOutcome::Done(n) => n,
    };
    let mut lines = Vec::new();
    let mut loud = BannerKind::Normal;
    // An error is never turned down to a warning.
    let worse = |kind: &mut BannerKind, to: BannerKind| {
        if !matches!(kind, BannerKind::Error) {
            *kind = to;
        }
    };
    let clean = !n.reread_failed
        && n.remaining == 0
        && n.failed == 0
        && n.unknown_algo_types == 0
        && n.unresolved.is_empty();
    if clean {
        lines.push(format!(
            "The {what} is done: {} request(s) sent, and a fresh read found nothing left.",
            n.sent
        ));
    } else {
        lines.push(format!(
            "The {what} is NOT clean: {} request(s) sent. Read the lines below.",
            n.sent
        ));
        worse(&mut loud, BannerKind::Error);
    }
    if n.failed > 0 {
        lines.push(format!(
            "{} request(s) were not accepted, were refused or could not be built.",
            n.failed
        ));
    }
    if n.reread_failed {
        lines.push(
            "The final read failed, so what is left is unknown. Refresh and check the exchange."
                .to_string(),
        );
    } else if n.remaining > 0 {
        let on = if n.remaining_symbols.is_empty() {
            String::new()
        } else {
            format!(" (on {})", n.remaining_symbols.join(", "))
        };
        lines.push(format!(
            "{} still open after the fresh read{on}.",
            n.remaining
        ));
    }
    if n.unknown_algo_types > 0 {
        lines.push(format!(
            "{} OKX conditional order type(s) could not be read before the cancel. Orders of \
             these types may still exist: check the exchange.",
            n.unknown_algo_types
        ));
    }
    if n.journal_failed {
        lines.push(
            "The order journal could not be written for at least one request: after a restart \
             an unknown result would not be remembered."
                .to_string(),
        );
        worse(&mut loud, BannerKind::Warning);
    }
    if n.found > 0 {
        lines.push(format!(
            "{} request(s) had an unknown result and the exchange knows them. This is not a \
             fill: check the orders and positions.",
            n.found
        ));
        worse(&mut loud, BannerKind::Warning);
    }
    if n.not_placed > 0 {
        lines.push(format!(
            "{} request(s) had an unknown result and were presumed not placed. Nothing was \
             resent.",
            n.not_placed
        ));
        worse(&mut loud, BannerKind::Warning);
    }
    if !n.unresolved.is_empty() {
        lines.push(format!(
            "The result of {} request(s) is still unknown ({}). New entries on those symbols \
             stay blocked until you release them by hand or the app restarts.",
            n.unresolved.len(),
            n.unresolved.join(", ")
        ));
    }
    (lines, loud)
}

// ── Unknown results ─────────────────────────────────────────────────────

/// The release challenge waits here for the form. The challenge is not `Clone`: the slot hands
/// it out once (the same device as `order_flow::IntentSlot`).
#[derive(Clone)]
pub struct ChallengeSlot(Arc<Mutex<Option<ReleaseChallenge>>>);

impl ChallengeSlot {
    fn new(challenge: ReleaseChallenge) -> Self {
        Self(Arc::new(Mutex::new(Some(challenge))))
    }

    /// The challenge, once; afterwards `None`.
    pub fn take(&self) -> Option<ReleaseChallenge> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

impl fmt::Debug for ChallengeSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChallengeSlot(<redacted>)")
    }
}

pub fn describe_restore(e: RestoreError) -> &'static str {
    match e {
        RestoreError::WrongAccount => "The record belongs to another account.",
        RestoreError::UnknownKind => "This build does not know how to look this record up.",
        RestoreError::RulesMismatch => "The trading rules do not fit the record.",
        RestoreError::BadClientId => "The record has an invalid id.",
    }
}

/// Step one of a manual release: the challenge naming the order. Needs a client (the Keychain),
/// so it runs here; typing the phrase is the user's.
pub async fn begin_release(
    input: ClientInput,
    record: UnknownRecord,
) -> Result<ChallengeSlot, String> {
    let client = client_for(input, BEGIN_RELEASE_TIMED_OUT).await?;
    client
        .begin_manual_release_of_record(&record)
        .map(ChallengeSlot::new)
        .map_err(|e| describe_restore(e).to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseResult {
    /// The lock is gone. `removed`: the saved record was removed too.
    Released {
        removed: bool,
    },
    /// This account has no lock with that id any more.
    NotLocked,
    WrongAccount,
    NotDone(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseSummary {
    pub record: UnknownRecord,
    pub result: ReleaseResult,
    /// The file as it is now (`None`: it could not be read).
    pub remaining: Option<Vec<UnknownRecord>>,
    /// The audit line was written.
    pub logged: bool,
}

/// Step two: the user typed the phrase and `token` came out of it. The lock is released, the
/// saved record removed, the audit line written. Nothing is resent and no order is rebuilt.
pub async fn release(
    input: ClientInput,
    token: ManualReleaseToken,
    record: UnknownRecord,
) -> ReleaseSummary {
    let meta = input.meta.clone();
    let end = |result: ReleaseResult, remaining: Option<Vec<UnknownRecord>>, logged: bool| {
        ReleaseSummary {
            record: record.clone(),
            result,
            remaining,
            logged,
        }
    };
    let client = match client_for(input, BEGIN_RELEASE_TIMED_OUT).await {
        Ok(c) => c,
        Err(e) => return end(ReleaseResult::NotDone(e), None, true),
    };
    let released = match client.release_unknown_manually(token) {
        Ok(r) => r,
        Err(ReleaseError::NotLocked) => return end(ReleaseResult::NotLocked, None, true),
        Err(ReleaseError::WrongAccount) => return end(ReleaseResult::WrongAccount, None, true),
    };
    let store = UnknownStore::in_data_dir();
    let removed = store
        .remove_async(released.account, released.client_id.clone())
        .await
        .is_ok();
    let symbol = released.symbol.clone();
    let logged = tokio::task::spawn_blocking(move || {
        RiskLog::in_data_dir()
            .append(&RiskLogEntry::unknown_event(
                &meta,
                now_ms(),
                &symbol,
                LogAction::UnknownReleased,
            ))
            .is_ok()
    })
    .await
    .unwrap_or(false);
    let remaining = store.load_async().await.ok();
    end(ReleaseResult::Released { removed }, remaining, logged)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscardSummary {
    pub record: UnknownRecord,
    /// `Err`: the file was not changed (the reason is a fixed phrase).
    pub result: Result<DiscardOutcome, String>,
    pub remaining: Option<Vec<UnknownRecord>>,
}

/// Drop a saved record whose client id is invalid. Only through the logged function, so the
/// audit line is written; `logged == false` is said to the user.
pub async fn discard(
    token: DiscardToken,
    meta: AccountMeta,
    record: UnknownRecord,
) -> DiscardSummary {
    let store = UnknownStore::in_data_dir();
    let result = store
        .discard_invalid_logged_async(token, meta, now_ms(), RiskLog::in_data_dir())
        .await
        .map_err(|e| e.reason().to_string());
    let remaining = store.load_async().await.ok();
    DiscardSummary {
        record,
        result,
        remaining,
    }
}

/// The text after a discard.
pub fn describe_discard(result: &Result<DiscardOutcome, String>) -> (String, BannerKind) {
    match result {
        Err(why) => (
            format!("The record was not discarded: {why}."),
            BannerKind::Error,
        ),
        Ok(o) if !o.discarded => (
            "The record was already gone.".to_string(),
            BannerKind::Warning,
        ),
        Ok(o) if !o.logged => (
            "The record was discarded, but the audit log entry could not be written. New \
             entries of the account stay blocked until the app restarts."
                .to_string(),
            BannerKind::Warning,
        ),
        Ok(_) => (
            "The record was discarded. New entries of the account stay blocked until the app \
             restarts."
                .to_string(),
            BannerKind::Warning,
        ),
    }
}

/// The text after a manual release.
pub fn describe_release(r: &ReleaseSummary) -> (String, BannerKind) {
    match &r.result {
        ReleaseResult::Released { removed } => {
            let mut t = format!(
                "{} was released by hand. Nothing was resent and no order was rebuilt: a new \
                 entry is a new order.",
                r.record.symbol
            );
            if !removed {
                t.push_str(
                    " The saved record could not be removed and will come back after a restart.",
                );
            }
            if !r.logged {
                t.push_str(" The audit log entry could not be written.");
            }
            (t, BannerKind::Warning)
        }
        ReleaseResult::NotLocked => (
            "This send is no longer locked (it may have been resolved meanwhile).".to_string(),
            BannerKind::Warning,
        ),
        ReleaseResult::WrongAccount => (
            "The confirmation was made for another account. Nothing was released.".to_string(),
            BannerKind::Error,
        ),
        ReleaseResult::NotDone(why) => (
            format!("Nothing was released: {}", clip(why)),
            BannerKind::Error,
        ),
    }
}

/// One saved record looked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupSummary {
    pub account: Uuid,
    pub ends: Vec<(UnknownRecord, LookupEnd)>,
    pub remaining: Option<Vec<UnknownRecord>>,
}

/// After the account's test of this session: saved records get `pending_from_record` →
/// `resolve_unknown`. Reads only. A record whose rules or client cannot be set up stays
/// unknown (and locked).
pub async fn look_up(input: ClientInput, records: Vec<UnknownRecord>) -> LookupSummary {
    let account = input.meta.id;
    let product = trade_market(market_of(input.meta.venue, input.meta.product)).map(|m| m.rules);
    let unknown = |records: Vec<UnknownRecord>| LookupSummary {
        account,
        ends: records
            .into_iter()
            .map(|r| (r, LookupEnd::StillUnknown))
            .collect(),
        remaining: None,
    };
    let Ok(client) = client_for(input, BEGIN_RELEASE_TIMED_OUT).await else {
        return unknown(records);
    };
    let Some(product) = product else {
        return unknown(records);
    };
    let Ok(table) = client.fetch_rules(product).await else {
        return unknown(records);
    };
    let journal = UnknownStore::in_data_dir();
    let ends = join_all(records.into_iter().map(|record| {
        let (client, table, journal) = (&client, &table, &journal);
        async move {
            let end = match client
                .fresh(table, &record.symbol)
                .ok()
                .and_then(|f| client.pending_from_record(&record, f.rules()).ok())
            {
                Some(pending) => match client.resolve_unknown(pending, journal).await {
                    Resolution::Found => LookupEnd::Found,
                    Resolution::NotPlacedPresumed => LookupEnd::NotPlaced,
                    Resolution::Unresolved(_) => LookupEnd::StillUnknown,
                },
                None => LookupEnd::StillUnknown,
            };
            (record, end)
        }
    }))
    .await;
    LookupSummary {
        account,
        ends,
        remaining: UnknownStore::in_data_dir().load_async().await.ok(),
    }
}

pub async fn load_records() -> Result<Vec<UnknownRecord>, String> {
    UnknownStore::in_data_dir()
        .load_async()
        .await
        .map_err(|e| e.reason().to_string())
}

/// The text after a lookup of a saved record (only the symbol, never the id).
pub fn describe_lookup(symbol: &str, end: LookupEnd) -> (String, BannerKind) {
    match end {
        LookupEnd::Found => (
            format!(
                "{symbol}: the exchange knows the earlier order. This is not a fill: check the \
                 orders and the position."
            ),
            BannerKind::Warning,
        ),
        LookupEnd::NotPlaced => (
            format!(
                "{symbol}: the exchange never showed the earlier order, so it is presumed not \
                 placed. Nothing was resent."
            ),
            BannerKind::Warning,
        ),
        LookupEnd::StillUnknown => (
            format!(
                "{symbol}: the earlier order is still unknown after the lookups. The symbol \
                 stays locked."
            ),
            BannerKind::Error,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn preview(action: BulkAction, orders: usize, positions: usize) -> BulkPreview {
        BulkPreview {
            request: BulkRequest {
                account: Uuid::nil(),
                action,
            },
            orders,
            order_symbols: vec!["BTCUSDT".into()],
            positions,
            position_symbols: vec!["ETHUSDT".into()],
        }
    }

    /// The confirmation content: the value is "unknown" and never shown as zero; nothing to
    /// do says so.
    #[test]
    fn the_bulk_confirmation_says_unknown_never_zero_and_names_what_is_untouched() {
        let flat = preview(BulkAction::Flatten, 2, 3).lines().join("\n");
        assert!(flat.contains("3 position(s)") && flat.contains("unknown"));
        assert!(!flat.contains("value of the positions: 0"));
        assert!(flat.contains("Open orders (2) are not cancelled"));
        let cancel = preview(BulkAction::CancelAll, 2, 3).lines().join("\n");
        assert!(cancel.contains("2 order(s)") && cancel.contains("unknown"));
        assert!(cancel.contains("Positions (3) are not touched"));
        let none = preview(BulkAction::CancelAll, 0, 3);
        assert!(none.nothing_to_do());
        assert!(none.lines().join("\n").contains("nothing to do"));
        assert!(!preview(BulkAction::Flatten, 0, 1).nothing_to_do());
    }

    fn text_of(n: BulkNumbers) -> String {
        describe_bulk(BulkAction::CancelAll, &BulkOutcome::Done(n))
            .0
            .join(" ")
    }

    /// Every field of the report has its own sentence.
    #[test]
    fn every_bulk_report_field_has_a_text() {
        let clean = BulkNumbers {
            sent: 2,
            ..BulkNumbers::default()
        };
        assert!(text_of(clean.clone()).contains("is done"));
        let cases: Vec<(BulkNumbers, &str)> = vec![
            (
                BulkNumbers {
                    failed: 1,
                    ..clean.clone()
                },
                "not accepted",
            ),
            (
                BulkNumbers {
                    reread_failed: true,
                    ..clean.clone()
                },
                "final read failed",
            ),
            (
                BulkNumbers {
                    remaining: 2,
                    remaining_symbols: vec!["BTCUSDT".into()],
                    ..clean.clone()
                },
                "still open after the fresh read (on BTCUSDT)",
            ),
            (
                BulkNumbers {
                    unknown_algo_types: 1,
                    ..clean.clone()
                },
                "OKX conditional",
            ),
            (
                BulkNumbers {
                    journal_failed: true,
                    ..clean.clone()
                },
                "journal could not be written",
            ),
            (
                BulkNumbers {
                    found: 1,
                    ..clean.clone()
                },
                "the exchange knows them",
            ),
            (
                BulkNumbers {
                    not_placed: 1,
                    ..clean.clone()
                },
                "presumed not placed",
            ),
            (
                BulkNumbers {
                    unresolved: vec!["ETHUSDT".into()],
                    ..clean.clone()
                },
                "still unknown (ETHUSDT)",
            ),
        ];
        for (n, want) in cases {
            let t = text_of(n);
            assert!(t.contains(want), "{want}: {t}");
        }
        // A problem is loud; a clean run is not.
        let (_, calm) = describe_bulk(BulkAction::Flatten, &BulkOutcome::Done(clean));
        assert!(matches!(calm, BannerKind::Normal));
        let (_, loud) = describe_bulk(
            BulkAction::Flatten,
            &BulkOutcome::Done(BulkNumbers {
                remaining: 1,
                ..BulkNumbers::default()
            }),
        );
        assert!(matches!(loud, BannerKind::Error));
    }

    /// R10: a mismatched confirmation has its own text, not "unsupported".
    #[test]
    fn a_mismatched_bulk_confirmation_has_its_own_text() {
        assert_eq!(
            describe_bulk_error(&BulkError::ConfirmationMismatch),
            BulkOutcome::Mismatch
        );
        let (lines, kind) = describe_bulk(BulkAction::Flatten, &BulkOutcome::Mismatch);
        let text = lines.join(" ");
        assert!(text.contains("not made for this account and action"));
        assert!(!text.to_lowercase().contains("unsupported"));
        assert!(matches!(kind, BannerKind::Error));
    }

    /// N6: the Keychain read and the rules before a bulk send are stopped by the limit, and the
    /// answer says nothing was sent.
    #[test]
    fn a_bulk_setup_that_never_ends_is_stopped_before_anything_is_sent() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let got: Result<(), BulkOutcome> = rt.block_on(bulk_setup_within(
            Duration::ZERO,
            std::future::pending::<Result<(), String>>(),
        ));
        assert_eq!(
            got,
            Err(BulkOutcome::NotDone(BULK_SETUP_TIMED_OUT.to_string()))
        );
        assert!(BULK_SETUP_TIMED_OUT.contains("Nothing was sent"));
        // A step that ends in time keeps its own result, the error text included.
        let ok: Result<u8, BulkOutcome> =
            rt.block_on(bulk_setup_within(Duration::from_secs(60), async { Ok(7) }));
        assert_eq!(ok, Ok(7));
        let err: Result<u8, BulkOutcome> = rt
            .block_on(bulk_setup_within(Duration::from_secs(60), async {
                Err("why".to_string())
            }));
        assert_eq!(err, Err(BulkOutcome::NotDone("why".to_string())));
    }

    #[test]
    fn a_preparation_that_never_ends_is_stopped_with_its_reason() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let got: Result<(), String> = rt.block_on(within(
            Duration::ZERO,
            std::future::pending::<Result<(), String>>(),
            PREPARE_BULK_TIMED_OUT,
        ));
        assert_eq!(got, Err(PREPARE_BULK_TIMED_OUT.to_string()));
        assert!(PREPARE_BULK_TIMED_OUT.contains("Nothing was"));
    }

    #[test]
    fn events_print_no_client_id_and_the_challenge_slot_is_redacted() {
        let record = UnknownRecord {
            account: exchange::account::AccountKey::from_bytes([1; 16]),
            symbol: "BTCUSDT".into(),
            client_id: "eSECRETid123".into(),
            kind: "x".into(),
            recorded_at_ms: 0,
        };
        let e = Event::ReleaseBegun(Box::new(record.clone()), Err("x".into()));
        assert!(!format!("{e:?}").contains("SECRETid123"));
        let e = Event::RecordsLoaded(Ok(vec![record]));
        assert!(!format!("{e:?}").contains("SECRETid123"));
        let slot_debug = format!(
            "{:?}",
            Event::Released(Box::new(ReleaseSummary {
                record: UnknownRecord {
                    account: exchange::account::AccountKey::from_bytes([1; 16]),
                    symbol: "BTCUSDT".into(),
                    client_id: "eSECRETid123".into(),
                    kind: "x".into(),
                    recorded_at_ms: 0,
                },
                result: ReleaseResult::NotLocked,
                remaining: None,
                logged: true,
            }))
        );
        assert!(!slot_debug.contains("SECRETid123"));
    }

    /// The text after a discard: the block stays and the log failure is said.
    #[test]
    fn a_discard_says_the_account_stays_blocked_and_whether_the_log_was_written() {
        let (ok, _) = describe_discard(&Ok(DiscardOutcome {
            discarded: true,
            logged: true,
        }));
        assert!(ok.contains("stay blocked until the app restarts"));
        let (unlogged, kind) = describe_discard(&Ok(DiscardOutcome {
            discarded: true,
            logged: false,
        }));
        assert!(unlogged.contains("audit log entry could not be written"));
        assert!(matches!(kind, BannerKind::Warning));
        let (err, kind) = describe_discard(&Err("the file could not be read".into()));
        assert!(err.contains("not discarded") && matches!(kind, BannerKind::Error));
    }
}
