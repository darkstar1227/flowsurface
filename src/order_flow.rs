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
//! `.submit_replace(` and `.replace_grant(` (the stop / target replace, f3a2) are called in this
//! file only, from [`send_replace`], with a token made by the confirmation as well.

use crate::protect_attach::{
    self, AttachKind, AttachNote, AttachPlan, Holdings, Resolved, check_against_price, plan_lines,
    preview_has_attachment,
};
use crate::replace_flow::{CancelEnd, NewLeg, ReplaceResult, ReplaceSummary};
use crate::trading::{PairState, account_key};
use data::config::exchange_accounts::{
    AccountMeta, StoreError, load_credentials, load_credentials_with_identity,
};
use data::config::protect::ProtectDefaults;
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
    ConfirmToken, DEFAULT_DEVIATION_BPS, Decimal, Dir, ManualContext, ManualSpec, OpenKind,
    OpenOrder, OrderIntent, OrderPreview, Outcome, ReadError, ReplaceError, ReplaceGrant,
    ReplaceKind, Resolution, RiskRefusal, SharedRisk, SubmitError, Submitted, TradeClient,
    TradeProduct, build_intent, deviation_exceeds,
};
use exchange::unit::Price;
use iced::window;
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

/// One saved account as the pane account choice sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountChoice {
    pub id: Uuid,
    pub venue: AccountVenue,
    pub product: AccountProduct,
    pub env: AccountEnv,
    /// The account passed its connection test in this session.
    pub tested: bool,
}

/// Why a pane has no account to order with. Nothing is guessed: each one is said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountRefusal {
    /// The pane's market cannot be traded here (other venue, or an inverse market).
    NotTradable,
    /// No saved account trades the pane's market.
    NoAccount,
    /// Several accounts trade it and the ticket's one is not among them: the user picks.
    PickInTicket,
    NotTested,
    /// A Live account while Live sending is closed.
    LiveClosed,
}

impl AccountRefusal {
    pub fn message(self) -> &'static str {
        match self {
            Self::NotTradable => "Orders cannot be placed on this market.",
            Self::NoAccount => "No saved account trades this market.",
            Self::PickInTicket => {
                "Several accounts trade this market. Pick one in the order ticket first."
            }
            Self::NotTested => "The account has not been tested yet. Test it first.",
            Self::LiveClosed => "Live order sending is closed in this build.",
        }
    }
}

/// The account a pane orders with: the ticket's account when it trades the pane's market;
/// otherwise the only account that does; never a guess among several. The pick must have been
/// tested, and a Live one needs Live sending open. Nothing here is stored in a pane.
pub fn resolve_pane_account(
    pane_market: Exchange,
    ticket_account: Option<Uuid>,
    accounts: &[AccountChoice],
    live_open: bool,
) -> Result<AccountChoice, AccountRefusal> {
    let market = trade_market(pane_market).ok_or(AccountRefusal::NotTradable)?;
    let fits = |a: &&AccountChoice| a.venue == market.venue && a.product == market.product;
    let from_ticket = ticket_account
        .and_then(|id| accounts.iter().find(|a| a.id == id))
        .filter(|a| fits(a));
    let pick = match from_ticket {
        Some(a) => *a,
        None => {
            let mut matching = accounts.iter().filter(fits);
            match (matching.next(), matching.next()) {
                (None, _) => return Err(AccountRefusal::NoAccount),
                (Some(a), None) => *a,
                (Some(_), Some(_)) => return Err(AccountRefusal::PickInTicket),
            }
        }
    };
    if !pick.tested {
        return Err(AccountRefusal::NotTested);
    }
    if pick.env == AccountEnv::Live && !live_open {
        return Err(AccountRefusal::LiveClosed);
    }
    Ok(pick)
}

/// A click on a pane's price ladder, before an account and a quantity are known. The price is
/// the row's price as an exact decimal; the exchange builder rounds it to the symbol's tick
/// (a buy down, a sell up) when the preview is made, so the confirmation shows the rounded one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneOrder {
    pub market: Exchange,
    pub symbol: String,
    /// A bid row buys (long), an ask row sells (short).
    pub dir: Dir,
    pub price: Decimal,
}

impl PaneOrder {
    /// `None` when the row has no usable price.
    pub fn from_row(ticker: exchange::Ticker, bid_side: bool, price: Price) -> Option<Self> {
        Some(Self {
            market: ticker.exchange,
            symbol: ticker.to_full_symbol_and_type().0,
            dir: if bid_side { Dir::Long } else { Dir::Short },
            price: price_to_decimal(price)?,
        })
    }
}

/// The limit order a ladder click asks for: the account by [`resolve_pane_account`], the
/// quantity the ticket holds now (nothing is stored in a pane). A refusal is said in words and
/// opens nothing. It only builds a request: the review and the confirmation send nothing until
/// the user confirms.
pub fn pane_request(
    order: &PaneOrder,
    ticket_account: Option<Uuid>,
    accounts: &[AccountChoice],
    live_open: bool,
    ticket_qty: Result<Decimal, String>,
) -> Result<(PrepareRequest, AccountChoice), String> {
    let account = resolve_pane_account(order.market, ticket_account, accounts, live_open)
        .map_err(|e| e.message().to_string())?;
    let qty = ticket_qty?;
    Ok((
        PrepareRequest {
            account: account.id,
            symbol: order.symbol.clone(),
            spec: ManualSpec::Limit {
                dir: order.dir,
                qty,
                price: order.price,
            },
            protect: None,
        },
        account,
    ))
}

/// The window a confirmation opens in: the one that asked while it is still open, else the main
/// window (a popout that was closed while its preview was being made).
pub fn live_origin(
    origin: window::Id,
    main: window::Id,
    is_open: impl Fn(window::Id) -> bool,
) -> window::Id {
    if origin == main || is_open(origin) {
        origin
    } else {
        main
    }
}

/// A closed window takes the confirmation that was open in it: nobody could press it.
pub fn confirm_lost_with(
    order: Option<window::Id>,
    bulk: Option<window::Id>,
    closed: window::Id,
) -> bool {
    order == Some(closed) || bulk == Some(closed)
}

/// Drops the in-flight request of one (account, symbol) only: other orders still need theirs
/// when their results come back.
pub fn forget_request<V>(
    requests: &mut std::collections::HashMap<(Uuid, String), V>,
    account: Uuid,
    symbol: &str,
) {
    requests.remove(&(account, symbol.to_string()));
}

/// `confirm_lost_with` for several windows closed together.
pub fn confirm_lost_with_any(
    order: Option<window::Id>,
    bulk: Option<window::Id>,
    closed: &[window::Id],
) -> bool {
    closed.iter().any(|&w| confirm_lost_with(order, bulk, w))
}

/// A limit order that would fill at once against the quote (a buy at or above it, a sell at or
/// below it). `false` when there is no quote to tell.
pub fn takes_liquidity(dir: Dir, price: Option<&Decimal>, quote: Option<&Decimal>) -> bool {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let (Some(price), Some(quote)) = (price, quote) else {
        return false;
    };
    matches!(
        (dir, price.checked_cmp(quote)),
        (Dir::Long, Ok(Greater | Equal)) | (Dir::Short, Ok(Less | Equal))
    )
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

/// Where a review was asked for. A click in the order book can land twice by accident (a double
/// click), so its confirmation also waits a short while before it takes a click.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OrderSource {
    #[default]
    Ticket,
    /// A command click on the ladder.
    Dom,
    /// A stop or target line dragged on the chart (f3a2): cancel the old order, place a new one.
    Replace,
}

/// What the user asked for, kept so a preview can be made again (rules expired).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareRequest {
    pub account: Uuid,
    pub symbol: String,
    pub spec: ManualSpec,
    /// The ticket's override of the account's protection defaults for this entry (`None` =
    /// the account's defaults). It lives in the request so that a preview made again after
    /// the rules expired keeps it. Checked by the same `ProtectDefaults::spec`; it cannot
    /// switch attaching on (`protect_attach::resolve`).
    pub protect: Option<ProtectDefaults>,
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
    /// What the confirmation says about protection; `None` when the request is not an entry.
    pub attach: Option<AttachNote>,
    /// The limit price would fill at once against the market (the confirmation says so).
    pub takes_liquidity: bool,
}

/// The direction, quantity and limit price of an entry request.
///
/// A spot `Short` is a sale of what is held, never an entry (defence in depth: the builder
/// refuses it today).
fn entry_of(spec: &ManualSpec, product: AccountProduct) -> Option<(Dir, Decimal, Option<Decimal>)> {
    let entry = match spec {
        ManualSpec::Limit { dir, qty, price } => (*dir, *qty, Some(*price)),
        ManualSpec::Market { dir, qty } => (*dir, *qty, None),
        _ => return None,
    };
    (!(product == AccountProduct::Spot && entry.0 == Dir::Short)).then_some(entry)
}

/// A preview, with the intent it came from (only in the slot).
#[derive(Debug, Clone)]
pub struct Prepared {
    pub request: PrepareRequest,
    pub preview: OrderPreview,
    pub slot: IntentSlot,
    pub notes: Notes,
    /// The window the review was started in: the confirmation opens there.
    pub origin: window::Id,
    pub source: OrderSource,
    /// Only for a replace: the old order and the cancel that goes with the new order.
    pub replace: Option<ReplaceInfo>,
}

/// What the confirmation of a replace shows besides the new order, read from the old row the
/// drag started from.
#[derive(Debug, Clone)]
pub struct ReplaceInfo {
    /// The old order's row as it was when the review was made.
    pub old: OpenOrder,
    /// The cancel the grant will send, as the grant builds it (`ReplaceGrant::cancel_preview_for`).
    pub cancel: OrderPreview,
    pub kind: ReplaceKind,
    /// Which leg goes out first, as `submit_replace` will decide it for this new order (f3b).
    /// The confirmation words its warning from this, never from a guess.
    pub order: exchange::trade::ReplaceOrder,
    /// The direction of the position the order protects.
    pub dir: Dir,
    pub old_trigger: Decimal,
    /// The new trigger as the builder rounded it.
    pub new_trigger: Decimal,
    /// The new trigger would fire at once against the ticker price (`None`: no price to compare).
    pub immediate: Option<bool>,
}

pub struct PrepareInput {
    pub meta: AccountMeta,
    /// The position mode the exchange reported at the test (not the saved one).
    pub mode: Option<PositionMode>,
    /// What the account holds on the request's symbol, and whether that can be trusted.
    pub held: Holdings,
    /// Automatic protection is still at work for an earlier entry on the symbol (M3).
    pub auto_active: bool,
    pub request: PrepareRequest,
    /// Carried to `Prepared` unchanged.
    pub origin: window::Id,
    /// Carried to `Prepared` unchanged.
    pub source: OrderSource,
    pub ref_price: Option<Decimal>,
    /// The old order of a replace (the latest row); `None` for everything else.
    pub old: Option<OpenOrder>,
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
        held,
        auto_active,
        request,
        origin,
        source,
        ref_price,
        old,
        signed,
        proxy,
    } = input;
    let market = trade_market(market_of(meta.venue, meta.product))
        .ok_or_else(|| "This account's market cannot be traded.".to_string())?;
    // Which protection applies to this entry: refused here, before anything is read or
    // confirmed, when the settings or the override are not valid for the account.
    let entry = entry_of(&request.spec, meta.product);
    let resolved = match entry {
        Some((dir, _, _)) => {
            let resolved = protect_attach::resolve(&meta, mode, request.protect.as_ref())?;
            // Protection goes with new entries only: an order against a held position, or one
            // placed while the positions cannot be told, is refused when it would be attached.
            if matches!(resolved, Resolved::Attach(_)) {
                protect_attach::check_new_entry(meta.product, mode, dir, &held)?;
                // Binance futures protection is sent by the app after the fill; one entry at a
                // time per symbol (OKX carries its protection on the order).
                if auto_active && meta.venue != AccountVenue::Okx {
                    return Err(protect_attach::AUTO_STILL_ACTIVE.to_string());
                }
            }
            Some(resolved)
        }
        None => None,
    };
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
    // The protective prices at the price the entry would fill at; refused when they cannot be
    // applied (a stop distance not below the price, a share too small for the step).
    let okx = meta.venue == AccountVenue::Okx;
    let mut attach = match (&resolved, entry) {
        (Some(Resolved::Attach(spec)), Some((dir, qty, limit))) => {
            let base = limit.or(ref_price).ok_or_else(|| {
                "No market price is available to place the protection from.".to_string()
            })?;
            let planned = check_against_price(fresh.rules(), dir, &base, &qty, spec)?;
            Some((
                spec.clone(),
                AttachPlan {
                    kind: if okx {
                        AttachKind::OkxOnEntry
                    } else {
                        AttachKind::AfterFill
                    },
                    dir,
                    limit: limit.is_some(),
                    spec: spec.clone(),
                    lines: plan_lines(
                        spec,
                        &planned,
                        &base,
                        protect_attach::quote_of(&request.symbol),
                    ),
                },
            ))
        }
        _ => None,
    };
    let intent = build_intent(
        account,
        &fresh,
        &request.spec,
        ManualContext {
            mode,
            ref_price,
            okx_margin: MarginMode::Cross,
            // OKX carries it on the entry; a Binance futures entry is sent unchanged.
            protect: attach.as_ref().filter(|_| okx).map(|(spec, _)| spec),
        },
    )
    .map_err(|e| format!("The order cannot be built: {e}."))?;
    let preview = intent.preview();
    // What is listed as attached must be in what will be sent.
    if okx && attach.is_some() && !preview_has_attachment(&preview.fields) {
        return Err("The protection was not part of the order. Nothing was sent.".to_string());
    }
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
    let attach_note = resolved.map(|r| match r {
        Resolved::Nothing => AttachNote::Nothing,
        Resolved::SwitchOff { live } => AttachNote::SwitchOff { live },
        Resolved::Attach(_) => match attach.take() {
            Some((_, plan)) => AttachNote::Attach(plan),
            None => AttachNote::Nothing,
        },
    });
    let takes = entry
        .map(|(dir, _, _)| takes_liquidity(dir, limit_price.as_ref(), ref_price.as_ref()))
        .unwrap_or(false);
    // A replace: the cancel the grant will send, and what the new trigger became after rounding.
    let replace = match old {
        Some(old) => Some(replace_info(
            old,
            account,
            &fresh,
            &request.spec,
            &preview,
            exchange::trade::ReplaceOrder::of(&intent),
            ref_price.as_ref(),
        )?),
        None => None,
    };
    let notes = Notes {
        takes_liquidity: takes,
        deviation: deviation_of(limit_price.as_ref(), ref_price.as_ref()),
        reference: ref_price.as_ref().map(Decimal::to_plain_string),
        base_amount,
        attach: attach_note,
    };
    Ok(Prepared {
        request,
        preview,
        slot: IntentSlot::new(intent),
        notes,
        origin,
        source,
        replace,
    })
}

/// The new trigger as the builder rounded it, from the preview.
fn preview_trigger(preview: &OrderPreview) -> Option<Decimal> {
    ["triggerPrice", "slTriggerPx", "tpTriggerPx"]
        .iter()
        .find_map(|name| preview.field(name))
        .and_then(|v| Decimal::parse(v).ok())
        .filter(Decimal::is_positive)
}

/// The cancel preview, kind and triggers of a replace. Refused (nothing is shown) when the old
/// row cannot be cancelled by the grant, when the rounded trigger is the old one (nothing would
/// change), or when the new trigger cannot be read back from the preview.
fn replace_info(
    old: OpenOrder,
    account: exchange::account::AccountKey,
    fresh: &exchange::trade::FreshRules<'_>,
    spec: &ManualSpec,
    preview: &OrderPreview,
    order: exchange::trade::ReplaceOrder,
    mark: Option<&Decimal>,
) -> Result<ReplaceInfo, String> {
    let (kind, dir) = match spec {
        ManualSpec::Stop { dir, .. } => (ReplaceKind::Stop, *dir),
        ManualSpec::TakeProfit { dir, .. } => (ReplaceKind::Target, *dir),
        _ => return Err("This request is not a stop or a take-profit.".to_string()),
    };
    let cancel = ReplaceGrant::cancel_preview_for(account, &old, fresh)
        .map_err(|e| format!("The old order cannot be replaced: {e}."))?;
    let old_trigger = old
        .trigger_price
        .ok_or_else(|| "The old order has no readable trigger price.".to_string())?;
    let new_trigger = preview_trigger(preview)
        .ok_or_else(|| "The new trigger price could not be read from the preview.".to_string())?;
    // The tick rounding can bring a small drag back onto the old price.
    if new_trigger.checked_cmp(&old_trigger) == Ok(std::cmp::Ordering::Equal) {
        return Err("The price rounds to the old trigger price. Nothing was changed.".to_string());
    }
    let immediate = crate::replace_flow::would_trigger_now(kind, dir, &new_trigger, mark);
    Ok(ReplaceInfo {
        old,
        cancel,
        kind,
        order,
        dir,
        old_trigger,
        new_trigger,
        immediate,
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

// ── Send a replace (f3a2) ───────────────────────────────────────────────

pub struct ReplaceSendInput {
    pub meta: AccountMeta,
    /// The session entry as it is at the moment of the confirmation.
    pub session: Option<SessionEntry>,
    pub symbol: String,
    pub signed: SignedHttp,
    pub proxy: Arc<dyn Fn() -> ProxyState + Send + Sync>,
    pub risk: SharedRisk,
    pub token: ConfirmToken,
    /// The new order, from the confirmation's slot.
    pub intent: OrderIntent,
    /// The old order **as read at the click**, not the row the review was made from: the grant
    /// compares it with what the user confirmed.
    pub old: OpenOrder,
    pub kind: ReplaceKind,
    pub dir: Dir,
    pub old_trigger: Decimal,
    pub new_trigger: Decimal,
}

/// The replace on a task of its own. The confirmation window, the review and the ticket own
/// nothing of it: `tokio::spawn` detaches the send, so closing the window (or dropping the
/// iced task that waits for the result) cannot cancel it between the cancel and the new order.
/// If the spawned task itself ends without an answer, nothing is known about either leg and the
/// result is [`ReplaceResult::Lost`].
pub async fn run_replace(input: ReplaceSendInput) -> ReplaceSummary {
    let lost = ReplaceSummary {
        account: input.meta.id,
        symbol: input.symbol.clone(),
        old_id: input.old.id.clone(),
        kind: input.kind,
        dir: input.dir,
        old_trigger: input.old_trigger,
        new_trigger: input.new_trigger,
        result: ReplaceResult::Lost,
        journal_failed: false,
        replacement: None,
    };
    tokio::spawn(send_replace(input)).await.unwrap_or(lost)
}

/// What became of a new order that was sent and not accepted. An unknown one is only looked up,
/// never sent again.
async fn new_leg(client: &TradeClient, journal: &UnknownStore, sub: Submitted) -> NewLeg {
    match sub.outcome {
        Outcome::Rejected { code, msg, .. } => NewLeg::Rejected {
            code,
            msg: clip(&msg),
        },
        Outcome::Partial { .. } => NewLeg::Rejected {
            code: "partial".to_string(),
            msg: String::new(),
        },
        Outcome::NotSent(_) => NewLeg::NeverSent,
        // The new order's lookup only reads; the order is never sent again.
        Outcome::Unknown { .. } => match sub.pending {
            Some(pending) => match client.resolve_unknown(pending, journal).await {
                Resolution::Found => NewLeg::Found,
                Resolution::NotPlacedPresumed => NewLeg::NotPlacedPresumed,
                Resolution::Unresolved(_) => NewLeg::Unresolved,
            },
            None => NewLeg::Unresolved,
        },
        // Cannot happen (an accepted new order is `Ok`): never read as a success.
        Outcome::Accepted { .. } => NewLeg::Unresolved,
    }
}

fn cancel_end(outcome: &Outcome) -> CancelEnd {
    match outcome {
        Outcome::Rejected { code, msg, .. } => CancelEnd::Rejected {
            code: code.clone(),
            msg: clip(msg),
        },
        Outcome::Unknown { .. } => CancelEnd::Unknown,
        Outcome::NotSent(_) => CancelEnd::NotSent,
        // Accepted without naming the old order, or only a part: not a confirmed cancel.
        Outcome::Accepted { .. } | Outcome::Partial { .. } => CancelEnd::Unconfirmed,
    }
}

/// Keychain → client → clock → rules → grant → `submit_replace` (cancel first, the new order
/// only once the cancel is confirmed). Never resends either leg; an unknown new order is only
/// looked up.
async fn send_replace(input: ReplaceSendInput) -> ReplaceSummary {
    let ReplaceSendInput {
        meta,
        session,
        symbol,
        signed,
        proxy,
        risk,
        token,
        intent,
        old,
        kind,
        dir,
        old_trigger,
        new_trigger,
    } = input;
    let old_id = old.id.clone();
    let summary = |result: ReplaceResult, journal_failed: bool| ReplaceSummary {
        replacement: None,
        account: meta.id,
        symbol: symbol.clone(),
        old_id: old_id.clone(),
        kind,
        dir,
        old_trigger,
        new_trigger,
        result,
        journal_failed,
    };
    let refused = |why: String| summary(ReplaceResult::Refused(why), false);
    let Some(market) = trade_market(market_of(meta.venue, meta.product)) else {
        return refused("This account's market cannot be traded.".to_string());
    };
    let loaded = {
        let meta = meta.clone();
        tokio::task::spawn_blocking(move || load_credentials_with_identity(&meta)).await
    };
    let (creds, identity) = match loaded {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => return refused(describe_store_error(&e)),
        Err(_) => return refused("Keychain access did not complete.".to_string()),
    };
    let keychain: Result<_, StoreError> = Ok(identity);
    let proxy_ok = check_proxy_snapshot(signed.proxy_state(), &proxy()).is_ok();
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
    let _ = client.sync_clock().await;
    // The grant reads the old row against fresh rules; they are read again, nothing is sent yet.
    let table = match client.fetch_rules(market.rules).await {
        Ok(t) => t,
        Err(e) => return refused(describe_read_error(&e)),
    };
    let fresh = match client.fresh(&table, &symbol) {
        Ok(f) => f,
        Err(e) => return refused(format!("Trading rules: {e}.")),
    };
    let grant = match client.replace_grant(token, account, &old, &fresh) {
        Ok(g) => g,
        Err(e) => return refused(format!("{e}.")),
    };
    let gate = SessionGate {
        session: session.as_ref(),
        keychain: &keychain,
        saved: &meta,
        proxy_ok,
    };
    let journal = UnknownStore::in_data_dir();
    match client
        .submit_replace(&gate, &risk, &journal, grant, intent)
        .await
    {
        Ok(done) => {
            let journal_failed = done.cancel.journal_failed || done.new.journal_failed;
            // Only a completed replace carries the proof the automatic protection follows.
            ReplaceSummary {
                replacement: done.replacement,
                ..summary(ReplaceResult::Done, journal_failed)
            }
        }
        Err(ReplaceError::Refused(r)) => refused(format!("{r}.")),
        Err(ReplaceError::NotSent(e)) => {
            let journal_failed = matches!(e, SubmitError::JournalUnavailable);
            summary(
                ReplaceResult::Refused(describe_submit_error(&e)),
                journal_failed,
            )
        }
        Err(ReplaceError::CancelNotConfirmed(sub)) => summary(
            ReplaceResult::CancelNotConfirmed(cancel_end(&sub.outcome)),
            sub.journal_failed,
        ),
        Err(ReplaceError::NewNotPlaced { cancel, new }) => {
            let mut journal_failed = cancel.journal_failed;
            let leg = match new {
                None => NewLeg::NeverSent,
                Some(sub) => {
                    journal_failed |= sub.journal_failed;
                    new_leg(&client, &journal, *sub).await
                }
            };
            summary(ReplaceResult::NewNotPlaced(leg), journal_failed)
        }
        // Place first only (f3b): the old order was not touched.
        Err(ReplaceError::NewNotAccepted(sub)) => {
            let journal_failed = sub.journal_failed;
            let leg = new_leg(&client, &journal, *sub).await;
            summary(ReplaceResult::PlacedFirstFailed(leg), journal_failed)
        }
        // Place first only (f3b): the new order stands, the old one may too.
        Err(ReplaceError::OldNotCancelled { new, cancel }) => {
            let mut journal_failed = new.journal_failed;
            let end = match cancel {
                None => CancelEnd::NotSent,
                Some(sub) => {
                    journal_failed |= sub.journal_failed;
                    cancel_end(&sub.outcome)
                }
            };
            summary(ReplaceResult::OldNotCancelled(end), journal_failed)
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

    fn choice(n: u128, venue: AccountVenue, product: AccountProduct) -> AccountChoice {
        AccountChoice {
            id: Uuid::from_u128(n),
            venue,
            product,
            env: AccountEnv::Testnet,
            tested: true,
        }
    }

    fn order(dir: Dir) -> PaneOrder {
        PaneOrder {
            market: Exchange::BinanceLinear,
            symbol: "BTCUSDT".to_string(),
            dir,
            price: d("100.1"),
        }
    }

    /// A ladder row price reaches the request exactly, and the side of the row is the direction.
    #[test]
    fn a_row_becomes_an_exact_limit_request() {
        let ticker = exchange::Ticker::new("BTCUSDT", Exchange::BinanceLinear);
        let o = PaneOrder::from_row(ticker, true, Price::from_f32(100.25)).unwrap();
        assert_eq!(o.symbol, "BTCUSDT");
        assert_eq!(o.market, Exchange::BinanceLinear);
        assert_eq!(o.dir, Dir::Long);
        assert_eq!(o.price, d("100.25"));
        let o = PaneOrder::from_row(ticker, false, Price::from_f32(0.5)).unwrap();
        assert_eq!(o.dir, Dir::Short);
        assert_eq!(o.price, d("0.5"));
        // A row with no positive price asks for nothing.
        assert!(PaneOrder::from_row(ticker, true, Price::from_f32(0.0)).is_none());
    }

    /// The pane price is rounded by the exchange builder (`round_limit_price`), not here and not
    /// by the display grouping: a buy goes down and a sell goes up, for ticks of 0.25, 0.5, 0.1.
    #[test]
    fn the_limit_price_is_rounded_to_the_tick_a_buy_down_and_a_sell_up() {
        let cases = [
            // (tick, price, buy, sell)
            ("0.25", "100.30", "100.25", "100.50"),
            ("0.5", "100.30", "100.00", "100.50"),
            ("0.1", "100.35", "100.3", "100.4"),
            ("0.25", "100.25", "100.25", "100.25"),
        ];
        for (tick, price, buy, sell) in cases {
            let (tick, price) = (d(tick), d(price));
            assert_eq!(price.floor_to_step(&tick).unwrap(), d(buy), "buy {price:?}");
            assert_eq!(
                price.ceil_to_step(&tick).unwrap(),
                d(sell),
                "sell {price:?}"
            );
        }
    }

    /// No quantity in the ticket, or no account: refused with a reason, nothing is built.
    #[test]
    fn a_pane_order_without_quantity_or_account_is_refused() {
        let a = choice(1, AccountVenue::Binance, AccountProduct::UsdM);
        let o = order(Dir::Long);
        let qty = || Ok(d("0.5"));
        let (req, who) = pane_request(&o, None, &[a], true, qty()).unwrap();
        assert_eq!(who.id, a.id);
        assert_eq!(req.account, a.id);
        assert_eq!(req.symbol, "BTCUSDT");
        assert_eq!(req.protect, None);
        assert_eq!(
            req.spec,
            ManualSpec::Limit {
                dir: Dir::Long,
                qty: d("0.5"),
                price: d("100.1")
            }
        );
        // The sell side of the book is a sale.
        let (req, _) = pane_request(&order(Dir::Short), None, &[a], true, qty()).unwrap();
        assert!(matches!(
            req.spec,
            ManualSpec::Limit {
                dir: Dir::Short,
                ..
            }
        ));
        let no_qty = Err("Enter a quantity in the order ticket first.".to_string());
        assert_eq!(
            pane_request(&o, None, &[a], true, no_qty).unwrap_err(),
            "Enter a quantity in the order ticket first."
        );
        assert_eq!(
            pane_request(&o, None, &[], true, qty()).unwrap_err(),
            AccountRefusal::NoAccount.message()
        );
        let b = choice(2, AccountVenue::Binance, AccountProduct::UsdM);
        assert_eq!(
            pane_request(&o, None, &[a, b], true, qty()).unwrap_err(),
            AccountRefusal::PickInTicket.message()
        );
        let untested = AccountChoice { tested: false, ..a };
        assert_eq!(
            pane_request(&o, None, &[untested], true, qty()).unwrap_err(),
            AccountRefusal::NotTested.message()
        );
    }

    #[test]
    fn a_limit_that_fills_at_once_takes_liquidity() {
        let q = d("100");
        let t = |dir, p: &str| takes_liquidity(dir, Some(&d(p)), Some(&q));
        assert!(t(Dir::Long, "100"));
        assert!(t(Dir::Long, "101"));
        assert!(!t(Dir::Long, "99.9"));
        assert!(t(Dir::Short, "100"));
        assert!(t(Dir::Short, "99"));
        assert!(!t(Dir::Short, "100.1"));
        // No quote: it cannot be told, and is not claimed.
        assert!(!takes_liquidity(Dir::Long, Some(&q), None));
        assert!(!takes_liquidity(Dir::Long, None, Some(&q)));
    }

    /// A confirmation goes with its window; one for another window stays.
    #[test]
    fn closing_a_window_takes_its_confirmation_and_reroutes_a_resend() {
        let (main, pop, other) = (
            window::Id::unique(),
            window::Id::unique(),
            window::Id::unique(),
        );
        assert!(confirm_lost_with(Some(pop), None, pop));
        assert!(confirm_lost_with(None, Some(pop), pop));
        assert!(!confirm_lost_with(Some(other), Some(main), pop));
        assert!(!confirm_lost_with(None, None, pop));
        // A resend goes back to the window it came from while that is open, else the main one.
        assert_eq!(live_origin(pop, main, |w| w == pop), pop);
        assert_eq!(live_origin(pop, main, |_| false), main);
        assert_eq!(live_origin(main, main, |_| false), main);
    }

    /// Discarding a confirmation leaves the other orders' requests alone (S4).
    #[test]
    fn forgetting_a_request_keeps_the_other_in_flight_ones() {
        let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let mut requests = std::collections::HashMap::new();
        requests.insert((a, "BTCUSDT".to_string()), 1);
        requests.insert((a, "ETHUSDT".to_string()), 2);
        requests.insert((b, "BTCUSDT".to_string()), 3);
        forget_request(&mut requests, a, "BTCUSDT");
        assert_eq!(requests.len(), 2);
        assert!(requests.contains_key(&(a, "ETHUSDT".to_string())));
        assert!(requests.contains_key(&(b, "BTCUSDT".to_string())));
    }

    /// A popout closed by a layout switch or a merge (no close request) is still forgotten:
    /// the `Closed` event reaches `forget_window`, so a resend no longer opens in it.
    #[test]
    fn every_way_of_closing_a_window_ends_in_forget_window() {
        let main = include_str!("main.rs");
        let compact: String = main.split_whitespace().collect();
        let arm = compact
            .split("window::Event::Closed(window_id)=>")
            .nth(1)
            .expect("the Closed event is handled");
        // The comment in the arm is part of the compacted text: look at the arm's start only.
        assert!(arm[..arm.find("Task::none()").unwrap()].contains("self.forget_window(window_id)"));
        let win: String = include_str!("window.rs").split_whitespace().collect();
        assert!(win.contains(
            "iced::Event::Window(iced::window::Event::Closed)=>Some(Event::Closed(window))"
        ));
        // A layout switch drops the confirmation of a popout it is about to close.
        assert!(compact.contains("confirm_lost_with_any("));

        let (main_id, pop) = (window::Id::unique(), window::Id::unique());
        let mut factors = crate::window::WindowScaleFactors::new(main_id);
        factors.set(pop, crate::window::WindowScaleFactor::new(2.0));
        assert_eq!(live_origin(pop, main_id, |w| factors.contains(w)), pop);
        factors.remove(pop);
        assert_eq!(live_origin(pop, main_id, |w| factors.contains(w)), main_id);
        assert!(confirm_lost_with_any(Some(pop), None, &[main_id, pop]));
        assert!(!confirm_lost_with_any(Some(pop), None, &[main_id]));
    }

    /// The pane account is the ticket's when it fits, else the only fitting one, never a guess.
    #[test]
    fn the_pane_account_is_picked_by_rule_and_never_guessed() {
        use AccountProduct::{Spot, UsdM};
        use AccountRefusal as R;
        use AccountVenue::{Binance, Okx};
        let a = choice(1, Binance, UsdM);
        let b = choice(2, Binance, UsdM);
        let o = choice(3, Okx, UsdM);
        let live = AccountChoice {
            env: AccountEnv::Live,
            ..choice(4, Binance, UsdM)
        };
        let untested = AccountChoice {
            tested: false,
            ..choice(5, Binance, UsdM)
        };
        let id = |c: &AccountChoice| Some(c.id);
        let fut = Exchange::BinanceLinear;
        // (name, market, ticket account, accounts, live open, expected id or refusal)
        type Case = (
            &'static str,
            Exchange,
            Option<Uuid>,
            Vec<AccountChoice>,
            bool,
            Result<Uuid, AccountRefusal>,
        );
        let cases: Vec<Case> = vec![
            ("no account", fut, None, vec![], true, Err(R::NoAccount)),
            ("one, no ticket pick", fut, None, vec![a], true, Ok(a.id)),
            (
                "one, ticket picked it",
                fut,
                id(&a),
                vec![a],
                true,
                Ok(a.id),
            ),
            (
                "many, no ticket pick",
                fut,
                None,
                vec![a, b],
                true,
                Err(R::PickInTicket),
            ),
            (
                "many, ticket picked",
                fut,
                id(&b),
                vec![a, b],
                true,
                Ok(b.id),
            ),
            (
                "ticket account is another venue, one fits",
                fut,
                id(&o),
                vec![o, a],
                true,
                Ok(a.id),
            ),
            (
                "ticket account is another venue, many fit",
                fut,
                id(&o),
                vec![o, a, b],
                true,
                Err(R::PickInTicket),
            ),
            (
                "venue mismatch only",
                fut,
                id(&o),
                vec![o],
                true,
                Err(R::NoAccount),
            ),
            (
                "product mismatch only",
                Exchange::BinanceSpot,
                None,
                vec![a],
                true,
                Err(R::NoAccount),
            ),
            (
                "ticket account unknown",
                fut,
                Some(Uuid::from_u128(99)),
                vec![a],
                true,
                Ok(a.id),
            ),
            (
                "not tested",
                fut,
                None,
                vec![untested],
                true,
                Err(R::NotTested),
            ),
            (
                "live closed",
                fut,
                id(&live),
                vec![live],
                false,
                Err(R::LiveClosed),
            ),
            ("live open", fut, id(&live), vec![live], true, Ok(live.id)),
            (
                "not tradable market",
                Exchange::BybitLinear,
                None,
                vec![a],
                true,
                Err(R::NotTradable),
            ),
            (
                "okx spot is tradable",
                Exchange::OkexSpot,
                None,
                vec![choice(6, Okx, Spot)],
                true,
                Ok(Uuid::from_u128(6)),
            ),
        ];
        for (name, market, ticket, accounts, live_open, want) in cases {
            let got = resolve_pane_account(market, ticket, &accounts, live_open).map(|c| c.id);
            assert_eq!(got, want, "{name}");
        }
        // Every refusal has words for the user.
        for r in [
            R::NotTradable,
            R::NoAccount,
            R::PickInTicket,
            R::NotTested,
            R::LiveClosed,
        ] {
            assert!(!r.message().is_empty());
        }
    }

    #[test]
    fn an_expired_send_never_touches_an_open_or_pending_review() {
        assert!(may_review_again(false, false));
        assert!(!may_review_again(true, false));
        assert!(!may_review_again(false, true));
        assert!(!may_review_again(true, true));
    }
    use exchange::trade::TradeError;

    #[test]
    fn a_spot_sale_is_never_an_entry_so_it_gets_no_protection_note() {
        let q = Decimal::parse("1").unwrap();
        let short = ManualSpec::Market {
            dir: Dir::Short,
            qty: q,
        };
        let long = ManualSpec::Market {
            dir: Dir::Long,
            qty: q,
        };
        assert!(entry_of(&short, AccountProduct::Spot).is_none());
        assert!(entry_of(&long, AccountProduct::Spot).is_some());
        assert!(entry_of(&short, AccountProduct::UsdM).is_some());
    }

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

    /// The only caller of `submit` in the GUI is `send`, only here (condition 71). The automatic
    /// paths are called from `auto_flow.rs` only (d3, condition 72): it is the one allowed file
    /// for everything that builds, authorises or sends an automatic request, and the manual path
    /// never uses them. Files are compared by their path under `src/`, not by their name.
    #[test]
    fn submit_is_called_from_this_file_only_and_the_automatic_paths_from_auto_flow_only() {
        const AUTO_FILES: [&str; 1] = ["auto_flow.rs"];
        const AUTO_CALLS: [&str; 7] = [
            "submit_auto(",
            "AutoIntent::new(",
            "AutoGrant::from_entry(",
            ".authorise(",
            ".cancel_intent(",
            ".with_basis(",
            ".add_fill(",
        ];
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![src_dir.clone()];
        let mut submit_files = Vec::new();
        let mut auto_files = Vec::new();
        let mut mint_files = Vec::new();
        let mut replace_files = Vec::new();
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
                let name = path
                    .strip_prefix(&src_dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let text = std::fs::read_to_string(&path).unwrap();
                // Only the code, not the tests module of the file (it quotes the words). Single
                // items that are `#[cfg(test)]` come before it and stay in the scan.
                let code = text
                    .split("#[cfg(test)]\nmod tests")
                    .next()
                    .and_then(|c| c.split("#[cfg(test)]\npub(crate) mod tests").next())
                    .unwrap_or_default();
                if code.contains(".submit(") {
                    submit_files.push(name.clone());
                }
                if AUTO_CALLS.iter().any(|call| code.contains(call)) {
                    auto_files.push(name.clone());
                }
                // The grant is made from the confirmed intent, in the confirmation handler only.
                if code.contains("auto_flow::mint(") {
                    mint_files.push(name.clone());
                }
                // The replace path (cancel the old protective order, place the new one) is
                // sent and authorised in this file only (f3a2).
                if code.contains("submit_replace(") || code.contains("replace_grant(") {
                    replace_files.push(name);
                }
                assert!(
                    !code.contains("block_on("),
                    "{} contains block_on(",
                    path.display()
                );
            }
        }
        assert_eq!(submit_files, vec!["order_flow.rs".to_string()]);
        assert_eq!(auto_files, AUTO_FILES.map(String::from).to_vec());
        assert_eq!(mint_files, vec!["main.rs".to_string()]);
        assert_eq!(replace_files, vec!["order_flow.rs".to_string()]);
    }

    /// A confirmation opens in the window the request came from, never a fixed main window,
    /// and a review made again after the rules expired keeps that window.
    #[test]
    fn the_confirmation_opens_in_the_window_of_the_request() {
        let main = include_str!("main.rs");
        let compact: String = main.split_whitespace().collect();
        assert!(compact.contains("PendingConfirm::new(origin,"));
        assert!(
            compact.contains("PendingBulk::new(origin,Kind::Bulk")
                || compact.contains("PendingBulk::new(origin,modal::bulk_confirm::Kind::Bulk")
        );
        assert!(!compact.contains("PendingConfirm::new(self.main_window.id"));
        // The send keeps the window with its request; the new review takes it from there.
        assert!(compact.contains("(request,origin,source)"));
        assert!(compact.contains("self.start_review(request,self.live_origin(origin),source)"));
    }

    /// "Retry protection" only opens the review: the order is sent by the confirmation handler
    /// after the user confirms (new client id, new token), never from the retry itself, and it
    /// makes no automatic order (so no grant).
    #[test]
    fn the_stop_and_cancel_offers_only_open_the_review_and_send_nothing_themselves() {
        let main = include_str!("main.rs");
        // Each offer ends at the next method of the impl. Together they either open the review
        // themselves or hand over to the one function that does.
        for (name, opens) in [
            ("fn start_protection_retry(", "self.open_stop_review("),
            ("fn start_add_stop(", "self.open_stop_review("),
            ("fn open_stop_review(", "self.start_review("),
            ("fn start_stray_cancel(", "self.start_review("),
            ("fn start_trade_request(", "self.start_review("),
            ("fn start_replace_request(", "self.start_review_of("),
        ] {
            let start = main.find(name).unwrap_or_else(|| panic!("{name} exists"));
            let rest = &main[start..];
            let end = rest.find("\n    /// ").expect("a doc comment follows");
            let body = &rest[..end];
            assert!(body.contains(opens), "{name} must use {opens}: {body}");
            if name == "fn open_stop_review(" {
                // The exchange's symbol is checked before it reaches `Ticker::new` (B2).
                let check = body.find("valid_symbol(").expect("the symbol is checked");
                assert!(check < body.find("self.quote_of(").expect("a quote is read"));
            }
            for banned in [
                "order_flow::send(",
                "confirm_order",
                ".submit(",
                "submit_auto(",
                "AutoIntent::new(",
                "auto_flow::mint(",
                "run_replace(",
                "confirm_replace(",
                "AutoGrant",
                "for_bulk(",
                "BulkAction::CancelAll",
            ] {
                assert!(!body.contains(banned), "{name} must not use {banned}");
            }
        }
    }
}
