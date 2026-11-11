//! Manual orders (M5 c2b-2): the market → account mapping, the two background steps (prepare a
//! preview, send after the confirmation) and the texts for every way a send can end.
//!
//! The steps are thin: the rules about which builder a form maps to are in
//! `exchange::trade::manual`, the gate and the risk hook are the exchange's own. Nothing here
//! keeps a credential: the Keychain is read inside each step and the client is dropped at its
//! end. Every request goes through `SignedHttp` (never `server_http`).
//!
//! `.submit(` is called in this file only, from [`send`], and only with a token made by the
//! confirmation (`order_confirm`). Manual orders never use `submit_auto` or `AutoIntent`.

use crate::trading::{PairState, account_key};
use data::config::exchange_accounts::{
    AccountMeta, StoreError, load_credentials, load_credentials_with_identity,
};
use data::config::trade_gate::{GateRejection, SessionEntry, SessionGate};
use data::config::unknown_store::UnknownStore;
use exchange::account::{
    AccountEnv, AccountProduct, AccountVenue, PositionMode, ProxyState, check_proxy_snapshot,
};
use exchange::adapter::Exchange;
use exchange::signed_http::SignedHttp;
use exchange::trade::MarginMode;
use exchange::trade::stream::StreamState;
use exchange::trade::{
    ConfirmToken, DEFAULT_DEVIATION_BPS, Decimal, ManualContext, ManualSpec, OpenKind, OrderIntent,
    OrderPreview, Outcome, ReadError, Resolution, RiskRefusal, SharedRisk, SubmitError,
    TradeClient, TradeProduct, build_intent, deviation_exceeds,
};
use exchange::unit::Price;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use uuid::Uuid;

/// Characters of an exchange message that are shown (it is shown only, never logged).
const MSG_CHARS: usize = 120;

/// The longest a preparation (Keychain, clock, rules, builder; or the reads before a bulk
/// confirmation) may take before the form is released again (I1). Nothing is sent by it.
pub const PREPARE_TIMEOUT: Duration = Duration::from_secs(60);

/// Wait for a preparation for at most `limit`. A late answer is dropped with the future, and the
/// caller gets `timed_out` instead, so a form frozen for the preparation is released.
pub async fn within<T>(
    limit: Duration,
    work: impl Future<Output = Result<T, String>>,
    timed_out: &str,
) -> Result<T, String> {
    match tokio::time::timeout(limit, work).await {
        Ok(result) => result,
        Err(_) => Err(timed_out.to_string()),
    }
}

pub const PREPARE_TIMED_OUT: &str =
    "Preparing the order took too long and was stopped. Nothing was sent. Review it again.";

// ── Market → account ────────────────────────────────────────────────────

/// What a market of the app (a chart's `Exchange`) is on the trading side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TradeMarket {
    pub venue: AccountVenue,
    pub product: AccountProduct,
    pub rules: TradeProduct,
}

/// The tradable markets: Binance and OKX, spot and linear (USDⓈ-M / SWAP). Everything else
/// (Bybit, Hyperliquid, MEXC, the inverse markets) cannot be traded and is `None`.
pub fn trade_market(exchange: Exchange) -> Option<TradeMarket> {
    let (venue, product, rules) = match exchange {
        Exchange::BinanceLinear => (
            AccountVenue::Binance,
            AccountProduct::UsdM,
            TradeProduct::BinanceUsdM,
        ),
        Exchange::BinanceSpot => (
            AccountVenue::Binance,
            AccountProduct::Spot,
            TradeProduct::BinanceSpot,
        ),
        Exchange::OkexLinear => (
            AccountVenue::Okx,
            AccountProduct::UsdM,
            TradeProduct::OkxSwap,
        ),
        Exchange::OkexSpot => (
            AccountVenue::Okx,
            AccountProduct::Spot,
            TradeProduct::OkxSpot,
        ),
        Exchange::BinanceInverse
        | Exchange::BybitLinear
        | Exchange::BybitInverse
        | Exchange::BybitSpot
        | Exchange::HyperliquidLinear
        | Exchange::HyperliquidSpot
        | Exchange::OkexInverse
        | Exchange::MexcLinear
        | Exchange::MexcInverse
        | Exchange::MexcSpot => return None,
    };
    Some(TradeMarket {
        venue,
        product,
        rules,
    })
}

/// The market an account trades, to find its quotes.
pub fn market_of(venue: AccountVenue, product: AccountProduct) -> Exchange {
    match (venue, product) {
        (AccountVenue::Binance, AccountProduct::UsdM) => Exchange::BinanceLinear,
        (AccountVenue::Binance, AccountProduct::Spot) => Exchange::BinanceSpot,
        (AccountVenue::Okx, AccountProduct::UsdM) => Exchange::OkexLinear,
        (AccountVenue::Okx, AccountProduct::Spot) => Exchange::OkexSpot,
    }
}

/// A quote from the ticker table, as the exact decimal (no float).
pub fn price_to_decimal(price: Price) -> Option<Decimal> {
    const SCALE: i64 = 100_000_000_000;
    if price.units <= 0 {
        return None;
    }
    Decimal::parse(&format!(
        "{}.{:011}",
        price.units / SCALE,
        price.units % SCALE
    ))
    .ok()
}

// ── Prepare ─────────────────────────────────────────────────────────────

/// What the user asked for, kept so a preview can be made again (rules expired).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareRequest {
    pub account: Uuid,
    pub symbol: String,
    pub spec: ManualSpec,
}

/// The intent waits here for the confirmation. It is not `Clone`: the slot hands it out once.
#[derive(Clone)]
pub struct IntentSlot(Arc<Mutex<Option<OrderIntent>>>);

impl IntentSlot {
    fn new(intent: OrderIntent) -> Self {
        Self(Arc::new(Mutex::new(Some(intent))))
    }

    /// The intent, once; afterwards `None`.
    pub fn take(&self) -> Option<OrderIntent> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

impl fmt::Debug for IntentSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IntentSlot(<redacted>)")
    }
}

/// How far a limit price is from the market (condition 63).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deviation {
    /// Not a limit order.
    NotApplicable,
    /// No quote to compare with: it is said, not skipped.
    NoReference,
    Within,
    Exceeds,
}

pub fn deviation_of(price: Option<&Decimal>, reference: Option<&Decimal>) -> Deviation {
    match (price, reference) {
        (None, _) => Deviation::NotApplicable,
        (Some(_), None) => Deviation::NoReference,
        (Some(p), Some(r)) => match deviation_exceeds(p, r, DEFAULT_DEVIATION_BPS) {
            Ok(true) => Deviation::Exceeds,
            Ok(false) => Deviation::Within,
            Err(_) => Deviation::NoReference,
        },
    }
}

/// What the confirmation shows besides the builder's fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notes {
    pub deviation: Deviation,
    /// The quote used, as text.
    pub reference: Option<String>,
    /// OKX contracts and what they are in the base currency (condition 63).
    pub base_amount: Option<String>,
}

/// A preview, with the intent it came from (only in the slot).
#[derive(Debug, Clone)]
pub struct Prepared {
    pub request: PrepareRequest,
    pub preview: OrderPreview,
    pub slot: IntentSlot,
    pub notes: Notes,
}

pub struct PrepareInput {
    pub meta: AccountMeta,
    /// The position mode the exchange reported at the test (not the saved one).
    pub mode: Option<PositionMode>,
    pub request: PrepareRequest,
    pub ref_price: Option<Decimal>,
    pub signed: SignedHttp,
    pub proxy: Arc<dyn Fn() -> ProxyState + Send + Sync>,
}

pub(crate) fn clip(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(MSG_CHARS)
        .collect()
}

pub fn describe_read_error(e: &ReadError) -> String {
    match e {
        ReadError::Transport(e) => {
            format!("The exchange could not be read: {}", clip(&e.to_string()))
        }
        ReadError::Parse => "The exchange answered in an unexpected shape.".to_string(),
        ReadError::Trade(e) => format!("Trading rules: {e}."),
    }
}

pub(crate) fn describe_store_error(_: &StoreError) -> String {
    // The store's own text can name the Keychain item; only the fact is shown.
    "The API credentials could not be read from the Keychain.".to_string()
}

/// [`prepare_steps`] under the overall time limit (I1).
pub async fn prepare(input: PrepareInput) -> Result<Prepared, String> {
    within(PREPARE_TIMEOUT, prepare_steps(input), PREPARE_TIMED_OUT).await
}

/// Keychain → client → clock → rules → builder → intent → preview. Nothing is sent but reads.
async fn prepare_steps(input: PrepareInput) -> Result<Prepared, String> {
    let PrepareInput {
        meta,
        mode,
        request,
        ref_price,
        signed,
        proxy,
    } = input;
    let market = trade_market(market_of(meta.venue, meta.product))
        .ok_or_else(|| "This account's market cannot be traded.".to_string())?;
    let creds = {
        let meta = meta.clone();
        tokio::task::spawn_blocking(move || load_credentials(&meta))
            .await
            .map_err(|_| "Keychain access did not complete.".to_string())?
            .map_err(|e| describe_store_error(&e))?
    };
    let account = account_key(meta.id);
    let client = TradeClient::new(
        signed,
        meta.venue,
        meta.env,
        meta.product,
        account,
        creds,
        proxy,
    );
    // Stores the measured offset in the client; a failure keeps offset zero.
    let _ = client.sync_clock().await;
    let table = client
        .fetch_rules(market.rules)
        .await
        .map_err(|e| describe_read_error(&e))?;
    let fresh = client
        .fresh(&table, &request.symbol)
        .map_err(|e| format!("Trading rules: {e}."))?;
    let intent = build_intent(
        account,
        &fresh,
        &request.spec,
        ManualContext {
            mode,
            ref_price,
            okx_margin: MarginMode::Cross,
        },
    )
    .map_err(|e| format!("The order cannot be built: {e}."))?;
    let preview = intent.preview();
    let limit_price = match &request.spec {
        ManualSpec::Limit { .. } => preview
            .field("price")
            .or_else(|| preview.field("px"))
            .and_then(|p| Decimal::parse(p).ok()),
        _ => None,
    };
    let base_amount = fresh.rules().okx_contract().and_then(|c| {
        let sz = Decimal::parse(preview.field("sz")?).ok()?;
        let base = exchange::trade::checks::contracts_to_base(&sz, &c.ct_val).ok()?;
        Some(format!(
            "{} contracts = {} of the base currency",
            sz.to_plain_string(),
            base.to_plain_string()
        ))
    });
    let notes = Notes {
        deviation: deviation_of(limit_price.as_ref(), ref_price.as_ref()),
        reference: ref_price.as_ref().map(Decimal::to_plain_string),
        base_amount,
    };
    Ok(Prepared {
        request,
        preview,
        slot: IntentSlot::new(intent),
        notes,
    })
}

// ── Send ────────────────────────────────────────────────────────────────

/// The end of a send, in words the UI can show. `Clone`, and it holds no credential and no
/// full client id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendResult {
    Accepted {
        order_id: Option<String>,
    },
    /// The exchange refused it (`code` is `[-0-9A-Za-z]` ≤ 16 chars; `msg` is clipped).
    Rejected {
        code: String,
        msg: String,
    },
    Partial {
        accepted: usize,
        rejected: usize,
    },
    /// It never left this process.
    NotSent(String),
    /// The gate, the risk hook or a lock refused it; nothing was sent.
    Refused(String),
    /// The rules expired between the preview and the send: preview again, do not call it a
    /// failure.
    RulesExpired,
    /// The confirmation was not for this request.
    ConfirmationMismatch,
    /// The result was unknown and the lookup found the order.
    Found,
    /// The lookup never saw the order: presumed not placed. Nothing was resent.
    NotPlacedPresumed,
    /// The result stayed unknown after the lookups.
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendSummary {
    pub account: Uuid,
    pub symbol: String,
    pub result: SendResult,
    /// A send went out without its write-ahead record, or an entry was refused because the
    /// record could not be written: the journal is damaged.
    pub journal_failed: bool,
    /// The last four characters of the client id (never the whole id).
    pub client_tail: Option<String>,
}

impl SendSummary {
    /// What the (account, symbol) is after this: only an unresolved result keeps it blocked.
    pub fn pair_after(&self) -> Option<PairState> {
        matches!(self.result, SendResult::Unresolved).then_some(PairState::Unresolved)
    }
}

fn tail(id: &str) -> String {
    let n = id.chars().count();
    id.chars().skip(n.saturating_sub(4)).collect()
}

pub struct SendInput {
    pub meta: AccountMeta,
    /// The session entry as it is at the moment of the confirmation.
    pub session: Option<SessionEntry>,
    pub symbol: String,
    pub signed: SignedHttp,
    pub proxy: Arc<dyn Fn() -> ProxyState + Send + Sync>,
    /// The account's one tracker.
    pub risk: SharedRisk,
    pub token: ConfirmToken,
    pub intent: OrderIntent,
}

/// Keychain (with identity) → client → gate → `submit` → lookup when unknown. Never resends.
pub async fn send(input: SendInput) -> SendSummary {
    let SendInput {
        meta,
        session,
        symbol,
        signed,
        proxy,
        risk,
        token,
        intent,
    } = input;
    let summary =
        |result: SendResult, journal_failed: bool, client_tail: Option<String>| SendSummary {
            account: meta.id,
            symbol: symbol.clone(),
            result,
            journal_failed,
            client_tail,
        };
    // Read for this order only (condition 57), together with what the item says it belongs to.
    let loaded = {
        let meta = meta.clone();
        tokio::task::spawn_blocking(move || load_credentials_with_identity(&meta)).await
    };
    let (creds, identity) = match loaded {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => return summary(SendResult::NotSent(describe_store_error(&e)), false, None),
        Err(_) => {
            return summary(
                SendResult::NotSent("Keychain access did not complete.".to_string()),
                false,
                None,
            );
        }
    };
    let keychain: Result<_, StoreError> = Ok(identity);
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
    let gate = SessionGate {
        session: session.as_ref(),
        keychain: &keychain,
        saved: &meta,
        proxy_ok,
    };
    let journal = UnknownStore::in_data_dir();
    match client.submit(&gate, &risk, &journal, token, intent).await {
        Err(SubmitError::RulesExpired) => summary(SendResult::RulesExpired, false, None),
        Err(SubmitError::ConfirmationMismatch) => {
            summary(SendResult::ConfirmationMismatch, false, None)
        }
        Err(e) => {
            let journal_failed = matches!(e, SubmitError::JournalUnavailable);
            summary(
                SendResult::Refused(describe_submit_error(&e)),
                journal_failed,
                None,
            )
        }
        Ok(submitted) => {
            let journal_failed = submitted.journal_failed;
            match submitted.outcome {
                Outcome::Accepted { ids } => summary(
                    SendResult::Accepted {
                        order_id: ids.into_iter().next().map(|i| clip(&i)),
                    },
                    journal_failed,
                    None,
                ),
                Outcome::Rejected { code, msg, .. } => summary(
                    SendResult::Rejected {
                        code,
                        msg: clip(&msg),
                    },
                    journal_failed,
                    None,
                ),
                Outcome::Partial { accepted, rejected } => summary(
                    SendResult::Partial {
                        accepted: accepted.len(),
                        rejected: rejected.len(),
                    },
                    journal_failed,
                    None,
                ),
                Outcome::NotSent(e) => summary(
                    SendResult::NotSent(clip(&e.to_string())),
                    journal_failed,
                    None,
                ),
                Outcome::Unknown { client_id, .. } => {
                    let client_tail = client_id.as_deref().map(tail);
                    let result = match submitted.pending {
                        // Never a resend: the lookup only reads.
                        Some(pending) => match client.resolve_unknown(pending, &journal).await {
                            Resolution::Found => SendResult::Found,
                            Resolution::NotPlacedPresumed => SendResult::NotPlacedPresumed,
                            Resolution::Unresolved(_) => SendResult::Unresolved,
                        },
                        None => SendResult::Unresolved,
                    };
                    summary(result, journal_failed, client_tail)
                }
            }
        }
    }
}

// ── Texts ───────────────────────────────────────────────────────────────

pub fn describe_gate(r: &GateRejection) -> &'static str {
    match r {
        GateRejection::LiveTradingNotYetEnabled => "Live trading is not open yet.",
        GateRejection::ProxyNotOk => {
            "The proxy setting changed or is unreadable. Test the account again."
        }
        GateRejection::KeychainUnavailable => "The API credentials could not be read.",
        GateRejection::IdentityMismatch => {
            "The stored key does not belong to this account (venue, environment or product)."
        }
        GateRejection::NotTestedThisSession => "The account was not tested in this session.",
        GateRejection::SessionStale => "The test result is out of date. Test the account again.",
        GateRejection::AccountRejected => "The exchange rejected the account's credentials.",
        GateRejection::NotUsable => "The account is not usable for trading.",
        GateRejection::NoTradePermission => "The API key has no trading permission.",
        GateRejection::PositionModeUnknown => {
            "The position mode of the account is unknown. Test the account again."
        }
    }
}

pub fn describe_risk(r: &RiskRefusal) -> &'static str {
    match r {
        RiskRefusal::NotionalUnknown => {
            "A risk limit is set but the order value is unknown (no price)."
        }
        RiskRefusal::SingleOrderCapExceeded => "The order is above the single-order limit.",
        RiskRefusal::SymbolCapExceeded => {
            "The position after this order would be above the per-symbol limit."
        }
        RiskRefusal::PositionUnknown => {
            "The position size of this symbol is unknown or old, and a per-symbol limit is set. \
             Held positions cannot be valued in this build, so any open position makes this \
             unknown; with no position it is refreshed while the stream is connected."
        }
        RiskRefusal::EquityUnavailable => {
            "The account equity is unknown or old while an equity limit is set."
        }
        RiskRefusal::Locked => "A risk limit tripped: new entries are refused.",
        RiskRefusal::SettingsInvalid => {
            "The saved risk settings are invalid: new entries are refused."
        }
        RiskRefusal::WrongAccount => "The risk state belongs to another account.",
        RiskRefusal::SnapshotNotApplied => {
            "The account was just resynchronised and its positions are not applied yet. Try again."
        }
        RiskRefusal::StateUnavailable => "The risk state is unusable: new entries are refused.",
    }
}

pub fn describe_stream(s: StreamState) -> &'static str {
    match s {
        StreamState::Connected => "The private stream is connected.",
        StreamState::Reconnecting => {
            "The private stream is reconnecting: new entries are refused until it is synchronised."
        }
        StreamState::Resyncing => {
            "The private stream is resynchronising: new entries are refused until it is done."
        }
        StreamState::AuthFailed => {
            "The exchange rejected the credentials: all orders are paused. Test the account again."
        }
        StreamState::Unavailable => {
            "There is no private stream for this account: new entries are refused."
        }
    }
}

pub fn describe_submit_error(e: &SubmitError<GateRejection>) -> String {
    match e {
        SubmitError::Gate(g) => describe_gate(g).to_string(),
        SubmitError::Risk(r) => describe_risk(r).to_string(),
        SubmitError::RulesExpired => {
            "The trading data expired. Review the order again.".to_string()
        }
        SubmitError::Locked => {
            "An earlier send on this symbol has no known result yet: new entries stay blocked."
                .to_string()
        }
        SubmitError::JournalUnavailable => {
            "The record of this order could not be written, so nothing was sent.".to_string()
        }
        SubmitError::StreamNotReady(s) => describe_stream(*s).to_string(),
        SubmitError::Malformed(t) => format!("The order cannot be sent: {t}."),
        SubmitError::AccountMismatch => "The order was built for another account.".to_string(),
        SubmitError::ConfirmationMismatch => "The content changed. Review it again.".to_string(),
    }
}

/// The line under the ticket after a send. The exchange's `msg` is clipped and shown only.
pub fn describe_result(r: &SendResult, tail: Option<&str>) -> String {
    let id = tail.map(|t| format!(" (order …{t})")).unwrap_or_default();
    match r {
        SendResult::Accepted { order_id } => match order_id {
            Some(i) => format!("Accepted by the exchange (id {i})."),
            None => "Accepted by the exchange.".to_string(),
        },
        SendResult::Rejected { code, msg } => {
            format!("Rejected by the exchange: {code} {}", clip(msg))
        }
        SendResult::Partial { accepted, rejected } => {
            format!("{accepted} accepted, {rejected} rejected. Check the open orders.")
        }
        SendResult::NotSent(why) => format!("Not sent: {}", clip(why)),
        SendResult::Refused(why) => format!("Refused: {}", clip(why)),
        SendResult::RulesExpired => "The trading data expired. Review the order again.".to_string(),
        SendResult::ConfirmationMismatch => "The content changed. Review it again.".to_string(),
        SendResult::Found => format!(
            "The result was unknown; the exchange knows the order{id}. This is not a fill: check \
             the order and the position."
        ),
        SendResult::NotPlacedPresumed => format!(
            "The result was unknown; the exchange never showed the order{id}, so it is presumed \
             not placed. Nothing was resent. Send again by hand if you still want it."
        ),
        SendResult::Unresolved => format!(
            "The result is unknown{id}. Nothing was resent. New entries on this symbol stay \
             blocked until you release it by hand in the account section or the app restarts; \
             check the exchange."
        ),
    }
}

/// `true` for the kinds of order the ticket may cancel from the stream's list.
pub fn cancellable(kind: &OpenKind) -> bool {
    matches!(kind, OpenKind::Plain | OpenKind::Algo(_))
}

/// An expired send may start a new preview by itself only when no confirmation is open and no
/// other preview is being made: it would replace or outdate that one (M2).
pub fn may_review_again(confirm_open: bool, preparing: bool) -> bool {
    !confirm_open && !preparing
}

pub fn env_is_live(env: AccountEnv) -> bool {
    env == AccountEnv::Live
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_expired_send_never_touches_an_open_or_pending_review() {
        assert!(may_review_again(false, false));
        assert!(!may_review_again(true, false));
        assert!(!may_review_again(false, true));
        assert!(!may_review_again(true, true));
    }
    use exchange::trade::TradeError;

    const ALL_EXCHANGES: [Exchange; 14] = [
        Exchange::BinanceLinear,
        Exchange::BinanceInverse,
        Exchange::BinanceSpot,
        Exchange::BybitLinear,
        Exchange::BybitInverse,
        Exchange::BybitSpot,
        Exchange::HyperliquidLinear,
        Exchange::HyperliquidSpot,
        Exchange::OkexLinear,
        Exchange::OkexInverse,
        Exchange::OkexSpot,
        Exchange::MexcLinear,
        Exchange::MexcInverse,
        Exchange::MexcSpot,
    ];

    #[test]
    fn the_market_table_has_four_tradable_markets_and_ten_that_are_not() {
        let tradable: Vec<_> = ALL_EXCHANGES
            .iter()
            .filter_map(|e| trade_market(*e).map(|m| (*e, m)))
            .collect();
        assert_eq!(tradable.len(), 4);
        let want = [
            (
                Exchange::BinanceLinear,
                AccountVenue::Binance,
                AccountProduct::UsdM,
                TradeProduct::BinanceUsdM,
            ),
            (
                Exchange::BinanceSpot,
                AccountVenue::Binance,
                AccountProduct::Spot,
                TradeProduct::BinanceSpot,
            ),
            (
                Exchange::OkexLinear,
                AccountVenue::Okx,
                AccountProduct::UsdM,
                TradeProduct::OkxSwap,
            ),
            (
                Exchange::OkexSpot,
                AccountVenue::Okx,
                AccountProduct::Spot,
                TradeProduct::OkxSpot,
            ),
        ];
        for (e, venue, product, rules) in want {
            assert_eq!(
                trade_market(e),
                Some(TradeMarket {
                    venue,
                    product,
                    rules
                })
            );
        }
        for e in [
            Exchange::BinanceInverse,
            Exchange::BybitLinear,
            Exchange::BybitInverse,
            Exchange::BybitSpot,
            Exchange::HyperliquidLinear,
            Exchange::HyperliquidSpot,
            Exchange::OkexInverse,
            Exchange::MexcLinear,
            Exchange::MexcInverse,
            Exchange::MexcSpot,
        ] {
            assert_eq!(trade_market(e), None, "{e} is not tradable");
        }
    }

    #[test]
    fn an_account_maps_back_to_the_market_that_maps_to_it() {
        for e in ALL_EXCHANGES {
            if let Some(m) = trade_market(e) {
                assert_eq!(market_of(m.venue, m.product), e);
            }
        }
    }

    #[test]
    fn a_quote_becomes_an_exact_decimal() {
        // Built with the crate's own conversion, not with a scale written down here.
        for (value, want) in [
            (50000.5, "50000.5"),
            (0.25, "0.25"),
            (1.0, "1"),
            (123.125, "123.125"),
        ] {
            assert_eq!(
                price_to_decimal(Price::from_f64(value)).map(|d| d.to_plain_string()),
                Some(want.to_string()),
                "{value}"
            );
        }
        // The smallest unit is one step of the crate's own scale.
        let one = Price::from_f64(1.0).units;
        assert_eq!(
            price_to_decimal(Price::from_units(1)).map(|d| d.to_plain_string()),
            Some(format!(
                "0.{:0>width$}",
                1,
                width = one.to_string().len() - 1
            ))
        );
        assert_eq!(price_to_decimal(Price::from_units(0)), None);
        assert_eq!(price_to_decimal(Price::from_units(-5)), None);
    }

    fn d(s: &str) -> Decimal {
        Decimal::parse(s).unwrap()
    }

    #[test]
    fn the_deviation_is_five_percent_and_a_missing_quote_is_said() {
        let r = d("100");
        assert_eq!(deviation_of(None, Some(&r)), Deviation::NotApplicable);
        assert_eq!(deviation_of(Some(&d("100")), None), Deviation::NoReference);
        assert_eq!(deviation_of(Some(&d("105")), Some(&r)), Deviation::Within);
        assert_eq!(deviation_of(Some(&d("95")), Some(&r)), Deviation::Within);
        assert_eq!(
            deviation_of(Some(&d("105.1")), Some(&r)),
            Deviation::Exceeds
        );
        assert_eq!(deviation_of(Some(&d("94.9")), Some(&r)), Deviation::Exceeds);
    }

    const GATE: [GateRejection; 10] = [
        GateRejection::LiveTradingNotYetEnabled,
        GateRejection::ProxyNotOk,
        GateRejection::KeychainUnavailable,
        GateRejection::IdentityMismatch,
        GateRejection::NotTestedThisSession,
        GateRejection::SessionStale,
        GateRejection::AccountRejected,
        GateRejection::NotUsable,
        GateRejection::NoTradePermission,
        GateRejection::PositionModeUnknown,
    ];
    const RISK: [RiskRefusal; 10] = [
        RiskRefusal::NotionalUnknown,
        RiskRefusal::SingleOrderCapExceeded,
        RiskRefusal::SymbolCapExceeded,
        RiskRefusal::PositionUnknown,
        RiskRefusal::EquityUnavailable,
        RiskRefusal::Locked,
        RiskRefusal::SettingsInvalid,
        RiskRefusal::WrongAccount,
        RiskRefusal::SnapshotNotApplied,
        RiskRefusal::StateUnavailable,
    ];
    const STREAM: [StreamState; 5] = [
        StreamState::Connected,
        StreamState::Reconnecting,
        StreamState::Resyncing,
        StreamState::AuthFailed,
        StreamState::Unavailable,
    ];

    /// Every variant has its own plain sentence: not empty, not a Debug dump, no id.
    fn plain(text: &str) {
        assert!(text.len() > 10, "{text}");
        for bad in [
            "Gate(",
            "Risk(",
            "Refusal",
            "StreamNotReady",
            "{",
            "}",
            "0x",
        ] {
            assert!(!text.contains(bad), "{text} leaks {bad}");
        }
    }

    #[test]
    fn every_way_a_send_can_be_refused_has_a_sentence() {
        let mut seen = std::collections::HashSet::new();
        for g in GATE {
            let t = describe_submit_error(&SubmitError::Gate(g));
            plain(&t);
            assert!(seen.insert(t), "{g:?} shares a sentence");
        }
        for r in RISK {
            let t = describe_submit_error(&SubmitError::Risk(r));
            plain(&t);
            assert!(seen.insert(t), "{r:?} shares a sentence");
        }
        for s in STREAM {
            let t = describe_submit_error(&SubmitError::StreamNotReady(s));
            plain(&t);
            assert!(seen.insert(t), "{s:?} shares a sentence");
        }
        for e in [
            SubmitError::RulesExpired,
            SubmitError::Locked,
            SubmitError::JournalUnavailable,
            SubmitError::Malformed(TradeError::QtyBelowMin),
            SubmitError::AccountMismatch,
            SubmitError::ConfirmationMismatch,
        ] {
            plain(&describe_submit_error(&e));
        }
        // The two that mean "try again", not "failed", say so.
        assert!(
            describe_submit_error(&SubmitError::ConfirmationMismatch).contains("Review it again")
        );
        assert!(
            describe_submit_error(&SubmitError::RulesExpired).contains("Review the order again")
        );
    }

    #[test]
    fn position_unknown_says_why() {
        let t = describe_risk(&RiskRefusal::PositionUnknown);
        assert!(t.contains("cannot be valued") && t.contains("refreshed"));
    }

    #[test]
    fn an_unknown_result_is_never_a_failure_and_a_found_order_is_never_a_fill() {
        for r in [
            SendResult::Found,
            SendResult::NotPlacedPresumed,
            SendResult::Unresolved,
        ] {
            let t = describe_result(&r, Some("QuyF"));
            assert!(t.contains("…QuyF"));
            assert!(!t.to_lowercase().contains("failed"), "{t}");
            assert!(t.contains("unknown"), "{t}");
        }
        assert!(describe_result(&SendResult::Found, None).contains("not a fill"));
        assert!(
            describe_result(&SendResult::NotPlacedPresumed, None).contains("Nothing was resent")
        );
    }

    #[test]
    fn an_exchange_message_is_clipped_by_characters() {
        let long = "é".repeat(500);
        let t = describe_result(
            &SendResult::Rejected {
                code: "-2010".into(),
                msg: long,
            },
            None,
        );
        assert!(t.chars().count() < 200);
        let s = clip(&"x\u{0}y".repeat(100));
        assert!(!s.contains('\u{0}') && s.chars().count() <= MSG_CHARS);
    }

    #[test]
    fn a_summary_holds_four_characters_of_the_client_id_at_most() {
        assert_eq!(tail("eAbCdEfGhIjKlMnOpQuyF"), "QuyF");
        assert_eq!(tail("ab"), "ab");
        let s = SendSummary {
            account: Uuid::nil(),
            symbol: "BTCUSDT".into(),
            result: SendResult::Unresolved,
            journal_failed: false,
            client_tail: Some(tail("eAbCdEfGhIjKlMnOpQuyF")),
        };
        let shown = format!(
            "{s:?} {}",
            describe_result(&s.result, s.client_tail.as_deref())
        );
        assert!(!shown.contains("eAbCdEfGhIjKlMnOpQuyF"));
        assert_eq!(s.pair_after(), Some(PairState::Unresolved));
        let found = SendSummary {
            result: SendResult::Found,
            ..s
        };
        assert_eq!(found.pair_after(), None);
    }

    /// The gate is borrowed across the `submit` await inside a background task.
    #[test]
    fn the_session_gate_can_cross_threads() {
        fn sync<T: Sync>() {}
        sync::<SessionGate<'static>>();
    }

    /// The only caller of `submit` in the GUI is `send`, only here, and the manual path never
    /// uses the automatic ones (condition 71, 72).
    #[test]
    fn submit_is_called_from_this_file_only_and_never_the_automatic_paths() {
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![src_dir];
        let mut submit_files = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                // Only the code, not the tests of the file (they quote the words).
                let code = text.split("#[cfg(test)]").next().unwrap_or_default();
                if code.contains(".submit(") {
                    submit_files.push(path.file_name().unwrap().to_string_lossy().to_string());
                }
                for banned in ["submit_auto(", "AutoIntent::new(", "block_on("] {
                    assert!(
                        !code.contains(banned),
                        "{} contains {banned}",
                        path.display()
                    );
                }
            }
        }
        assert_eq!(submit_files, vec!["order_flow.rs".to_string()]);
    }
}
