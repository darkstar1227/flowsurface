//! The manual order ticket, a sidebar modal (M5 c2b-2). It collects the form, says why a review
//! is not possible, and asks `main` to prepare a preview; the confirmation window
//! (`order_confirm`) and the send step (`order_flow`) do the rest.
//!
//! Nothing here sends anything, reads the Keychain or holds a credential. Numbers are typed as
//! text, parsed to exact decimals, and rounded only by the venue builders.

use crate::{
    order_flow::{PrepareRequest, Prepared, SendSummary, cancellable, market_of, trade_market},
    style,
    trading::{PairState, TradingState},
    widget::{BannerKind, banner},
};
use data::config::exchange_accounts::AccountMeta;
use data::config::trade_gate::TradeSession;
use exchange::account::{AccountEnv, AccountProduct, AccountVenue};
use exchange::trade::stream::StreamState;
use exchange::trade::{Decimal, Dir, ManualSpec, OpenKind, Size};
use iced::{
    Element,
    widget::{button, checkbox, column, container, pick_list, row, text, text_input},
};
use std::fmt;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Limit,
    Market,
    Stop,
    TakeProfit,
    Trailing,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Limit,
        Kind::Market,
        Kind::Stop,
        Kind::TakeProfit,
        Kind::Trailing,
    ];

    /// Opens or adds exposure (needs a synchronised stream).
    pub fn is_entry(self) -> bool {
        matches!(self, Kind::Limit | Kind::Market)
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::Limit => "Limit",
            Kind::Market => "Market",
            Kind::Stop => "Stop loss",
            Kind::TakeProfit => "Take profit",
            Kind::Trailing => "Trailing stop",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirChoice {
    Long,
    Short,
}

impl DirChoice {
    const ALL: [DirChoice; 2] = [DirChoice::Long, DirChoice::Short];

    fn dir(self) -> Dir {
        match self {
            DirChoice::Long => Dir::Long,
            DirChoice::Short => Dir::Short,
        }
    }
}

impl fmt::Display for DirChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DirChoice::Long => "Long",
            DirChoice::Short => "Short",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountChoice {
    pub id: Uuid,
    label: String,
}

impl AccountChoice {
    fn new(m: &AccountMeta) -> Self {
        let env = match m.env {
            AccountEnv::Live => "LIVE",
            AccountEnv::Testnet => "TESTNET",
        };
        let market = match m.venue {
            AccountVenue::Binance => format!("{} {}", m.venue, m.product),
            AccountVenue::Okx => m.venue.to_string(),
        };
        Self {
            id: m.id,
            label: format!("{} ({market}, {env})", m.alias),
        }
    }
}

impl fmt::Display for AccountChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Account(AccountChoice),
    Kind(Kind),
    Symbol(String),
    Dir(DirChoice),
    Qty(String),
    Price(String),
    Trigger(String),
    Callback(String),
    Whole(bool),
    Review,
    /// A cancel of one open order from the list.
    CancelOrder(String, String, OpenKind),
    /// The preview came back (the number tells which review it belongs to).
    Prepared(u64, Result<Box<Prepared>, String>),
    Close,
}

pub enum Action {
    /// Prepare a preview for this request (`main` checks the gate again).
    Review(PrepareRequest),
    Exit,
}

#[derive(Default)]
pub struct OrderTicket {
    account: Option<Uuid>,
    kind: Option<Kind>,
    symbol: String,
    dir: Option<DirChoice>,
    qty: String,
    price: String,
    trigger: String,
    callback: String,
    whole: bool,
    nonce: u64,
    preparing: bool,
    status: Option<(String, BannerKind)>,
}

/// Everything that decides whether a review may start. Pure, so it can be tabled.
#[derive(Debug, Clone, Default)]
pub struct Gate {
    pub boot_done: bool,
    pub account: Option<(AccountVenue, AccountProduct, AccountEnv)>,
    pub tested: bool,
    pub needs_retest: bool,
    pub live_open: bool,
    pub signed_ok: bool,
    pub record_locked: bool,
    pub pair: Option<PairState>,
    pub stream: Option<StreamState>,
    pub kind: Option<Kind>,
    pub preparing: bool,
}

/// Why a review cannot start, or `None`. The first reason that applies is shown.
pub fn blocked_reason(g: &Gate) -> Option<&'static str> {
    if !g.boot_done {
        return Some("Starting up: saved order records are still being loaded.");
    }
    let Some((venue, product, env)) = g.account else {
        return Some("Pick an account.");
    };
    if g.preparing {
        return Some("Preparing the order…");
    }
    if venue == AccountVenue::Binance && product == AccountProduct::Spot {
        return Some("Binance spot orders are not available yet (no private stream).");
    }
    if env == AccountEnv::Live && !g.live_open {
        return Some("Live trading is not open yet.");
    }
    if !g.tested {
        return Some("Test the account in Exchange accounts first.");
    }
    if g.needs_retest {
        return Some("The credentials were refused. Test the account again.");
    }
    if !g.signed_ok {
        return Some("No usable connection (the proxy setting changed or is unreadable).");
    }
    let entry = g.kind.is_some_and(Kind::is_entry);
    // An unknown result locks entries only; a stop or a cancel may still reduce the risk.
    if entry && g.record_locked {
        return Some("An earlier order on this symbol has an unknown result: it stays locked.");
    }
    match g.pair {
        Some(PairState::InFlight) => return Some("An order on this symbol is being sent."),
        Some(PairState::Unresolved) if entry => {
            return Some("The result of an earlier order on this symbol is unknown.");
        }
        _ => {}
    }
    if g.kind == Some(Kind::Trailing) && venue == AccountVenue::Okx {
        return Some("Trailing stops are off on OKX.");
    }
    if g.stream == Some(StreamState::AuthFailed) {
        return Some("The exchange refused the credentials: orders are paused.");
    }
    if entry && g.stream != Some(StreamState::Connected) {
        return Some("New entries need a connected, synchronised private stream.");
    }
    None
}

/// The gate for the account and the form, from the app state.
pub fn gate_for(
    meta: Option<&AccountMeta>,
    kind: Option<Kind>,
    symbol: &str,
    trading: &TradingState,
    session: &TradeSession,
    signed_ok: bool,
    preparing: bool,
) -> Gate {
    let Some(m) = meta else {
        return Gate {
            boot_done: trading.orders_allowed(),
            ..Gate::default()
        };
    };
    Gate {
        boot_done: trading.orders_allowed(),
        account: Some((m.venue, m.product, m.env)),
        tested: session.get(m.id).is_some(),
        needs_retest: trading.needs_retest(m.id),
        live_open: data::config::trade_gate::live_sending_enabled(),
        signed_ok,
        record_locked: trading.is_record_locked(m.id, symbol),
        pair: trading.pair_state(m.id, symbol),
        stream: trading.stream_state(m.id),
        kind,
        preparing,
    }
}

/// The kind of order a request is (`None` for a cancel).
pub fn kind_of(spec: &ManualSpec) -> Option<Kind> {
    match spec {
        ManualSpec::Limit { .. } => Some(Kind::Limit),
        ManualSpec::Market { .. } => Some(Kind::Market),
        ManualSpec::Stop { .. } => Some(Kind::Stop),
        ManualSpec::TakeProfit { .. } => Some(Kind::TakeProfit),
        ManualSpec::Trailing { .. } => Some(Kind::Trailing),
        ManualSpec::Cancel { .. } => None,
    }
}

/// A symbol as the exchanges name them. Checked before it reaches `Ticker::new`, which asserts
/// on length and on `|`, so typed text can never panic the app.
pub fn valid_symbol(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 28
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn parse(label: &str, s: &str) -> Result<Decimal, String> {
    Decimal::parse(s.trim()).map_err(|_| format!("{label} is not a number."))
}

/// The form as a request. Nothing is rounded here.
pub fn build_spec(
    kind: Kind,
    dir: Dir,
    whole: bool,
    qty: &str,
    price: &str,
    trigger: &str,
    callback: &str,
) -> Result<ManualSpec, String> {
    let size = |whole: bool| -> Result<Size, String> {
        if whole {
            Ok(Size::ClosePosition)
        } else {
            Ok(Size::Qty(parse("Quantity", qty)?))
        }
    };
    Ok(match kind {
        Kind::Limit => ManualSpec::Limit {
            dir,
            qty: parse("Quantity", qty)?,
            price: parse("Price", price)?,
        },
        Kind::Market => ManualSpec::Market {
            dir,
            qty: parse("Quantity", qty)?,
        },
        Kind::Stop => ManualSpec::Stop {
            dir,
            size: size(whole)?,
            trigger: parse("Trigger price", trigger)?,
        },
        Kind::TakeProfit => ManualSpec::TakeProfit {
            dir,
            size: size(whole)?,
            trigger: parse("Trigger price", trigger)?,
        },
        Kind::Trailing => ManualSpec::Trailing {
            dir,
            qty: parse("Quantity", qty)?,
            callback_percent: parse("Callback", callback)?,
        },
    })
}

impl OrderTicket {
    pub fn set_status(&mut self, text: String, kind: BannerKind) {
        self.status = Some((text, kind));
    }

    pub fn is_preparing(&self) -> bool {
        self.preparing
    }

    /// The preview request has ended (a result, a refusal or a discard).
    pub fn finish_prepare(&mut self) {
        self.preparing = false;
    }

    /// A review started: the number tells the answer apart from older ones.
    pub fn start_prepare(&mut self) -> u64 {
        self.nonce += 1;
        self.preparing = true;
        self.status = None;
        self.nonce
    }

    /// A send ended: show it under the form.
    pub fn show_result(&mut self, summary: &SendSummary, line: String) {
        use crate::order_flow::SendResult as R;
        let kind = match summary.result {
            R::Accepted { .. } | R::Found => BannerKind::Normal,
            R::RulesExpired | R::ConfirmationMismatch | R::NotPlacedPresumed => BannerKind::Warning,
            _ => BannerKind::Error,
        };
        self.status = Some((line, kind));
    }

    /// Whether a `Prepared` answer is the current one.
    pub fn is_current(&self, nonce: u64) -> bool {
        self.preparing && self.nonce == nonce
    }

    /// The request of the form, for the selected account.
    pub fn request(&self) -> Result<PrepareRequest, String> {
        let account = self.account.ok_or("Pick an account.")?;
        let kind = self.kind.ok_or("Pick an order type.")?;
        let dir = self.dir.ok_or("Pick a direction.")?.dir();
        if self.symbol.trim().is_empty() {
            return Err("Enter a symbol.".to_string());
        }
        if !valid_symbol(self.symbol.trim()) {
            return Err("The symbol may hold up to 28 letters, digits, '-' or '_'.".to_string());
        }
        let spec = build_spec(
            kind,
            dir,
            self.whole,
            &self.qty,
            &self.price,
            &self.trigger,
            &self.callback,
        )?;
        Ok(PrepareRequest {
            account,
            symbol: self.symbol.trim().to_string(),
            spec,
        })
    }

    pub fn update(&mut self, message: Message) -> Option<Action> {
        // While a preview is being made the form is frozen: a change now would leave the
        // confirmation that opens next showing something other than the form (M1).
        if self.preparing && !matches!(message, Message::Prepared(..) | Message::Close) {
            return None;
        }
        match message {
            Message::Account(c) => {
                self.account = Some(c.id);
                self.status = None;
            }
            Message::Kind(k) => self.kind = Some(k),
            Message::Symbol(s) => self.symbol = s.to_uppercase(),
            Message::Dir(d) => self.dir = Some(d),
            Message::Qty(s) => self.qty = s,
            Message::Price(s) => self.price = s,
            Message::Trigger(s) => self.trigger = s,
            Message::Callback(s) => self.callback = s,
            Message::Whole(b) => self.whole = b,
            Message::Review => match self.request() {
                // `main` starts the review (it owns the gate and the clients).
                Ok(req) => return Some(Action::Review(req)),
                Err(e) => self.status = Some((e, BannerKind::Error)),
            },
            Message::CancelOrder(symbol, id, kind) => {
                let Some(account) = self.account else {
                    self.status = Some(("Pick an account.".to_string(), BannerKind::Error));
                    return None;
                };
                return Some(Action::Review(PrepareRequest {
                    account,
                    symbol,
                    spec: ManualSpec::Cancel { id, kind },
                }));
            }
            // Handled in `main`, which knows the confirmation window.
            Message::Prepared(..) => {}
            Message::Close => return Some(Action::Exit),
        }
        None
    }

    /// `symbol` of an open order list entry matches what is typed (case-blind).
    fn lists(&self, order_symbol: &str) -> bool {
        self.symbol.trim().is_empty() || self.symbol.trim().eq_ignore_ascii_case(order_symbol)
    }

    pub fn view<'a>(
        &'a self,
        accounts: &[AccountMeta],
        trading: &'a TradingState,
        session: &'a TradeSession,
        signed_ok: bool,
    ) -> Element<'a, Message> {
        let meta = self
            .account
            .and_then(|id| accounts.iter().find(|m| m.id == id));
        let choices: Vec<AccountChoice> = accounts
            .iter()
            .filter(|m| trade_market(market_of(m.venue, m.product)).is_some())
            .map(AccountChoice::new)
            .collect();
        let selected = meta.map(AccountChoice::new);
        let blocked = blocked_reason(&gate_for(
            meta,
            self.kind,
            self.symbol.trim(),
            trading,
            session,
            signed_ok,
            self.preparing,
        ));

        let header = row![
            button(style::icon_text(style::Icon::Return, 11)).on_press(Message::Close),
            text("Order ticket").size(style::text_size::SECTION),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let preparing = self.preparing;
        // Pick lists have no disabled state: while preparing they are shown as plain text.
        let pick = |list: Element<'a, Message>, shown: String| -> Element<'a, Message> {
            if preparing {
                text(shown).size(style::text_size::SMALL).into()
            } else {
                list
            }
        };
        // A text input without `on_input` is disabled.
        let field = |hint: &'static str, value: &'a str, f: fn(String) -> Message| {
            let input = text_input(hint, value);
            if preparing { input } else { input.on_input(f) }
        };
        let mut content = column![
            header,
            pick(
                pick_list(choices, selected.clone(), Message::Account)
                    .placeholder("Account")
                    .into(),
                selected.map(|c| c.to_string()).unwrap_or_default()
            ),
            pick(
                pick_list(Kind::ALL, self.kind, Message::Kind)
                    .placeholder("Order type")
                    .into(),
                self.kind.map(|k| k.to_string()).unwrap_or_default()
            ),
            field(
                "Symbol (as the exchange names it)",
                &self.symbol,
                Message::Symbol
            ),
            pick(
                pick_list(DirChoice::ALL, self.dir, Message::Dir)
                    .placeholder(if self.kind.is_some_and(Kind::is_entry) {
                        "Direction"
                    } else {
                        "Direction of the position to protect"
                    })
                    .into(),
                self.dir.map(|d| d.to_string()).unwrap_or_default()
            ),
        ]
        .spacing(8);

        let protective = matches!(self.kind, Some(Kind::Stop | Kind::TakeProfit));
        if protective {
            let whole = checkbox(self.whole).label("Close the whole position");
            content = content.push(if preparing {
                whole
            } else {
                whole.on_toggle(Message::Whole)
            });
        }
        if !(protective && self.whole) {
            content = content.push(field(
                "Quantity (contracts on OKX)",
                &self.qty,
                Message::Qty,
            ));
        }
        match self.kind {
            Some(Kind::Limit) => {
                content = content.push(field("Price", &self.price, Message::Price));
            }
            Some(Kind::Stop | Kind::TakeProfit) => {
                content = content.push(field("Trigger price", &self.trigger, Message::Trigger));
            }
            Some(Kind::Trailing) => {
                content = content.push(field("Callback %", &self.callback, Message::Callback));
            }
            _ => {}
        }

        content = content.push(
            button(text("Review")).on_press_maybe(blocked.is_none().then_some(Message::Review)),
        );
        if let Some(why) = blocked {
            content = content.push(text(why).size(style::text_size::SMALL));
        }
        if let Some((s, kind)) = &self.status {
            content = content.push(banner(s.as_str(), *kind));
        }

        // Cancel of one open order, from the stream's last snapshot.
        if let (Some(m), Some(_)) = (meta, self.account) {
            let orders: Vec<_> = trading
                .open_orders(m.id)
                .iter()
                .filter(|o| cancellable(&o.kind) && self.lists(&o.symbol))
                .collect();
            if !orders.is_empty() {
                let cancel_blocked = blocked_reason(&gate_for(
                    meta,
                    None,
                    self.symbol.trim(),
                    trading,
                    session,
                    signed_ok,
                    self.preparing,
                ))
                .is_some();
                let mut list =
                    column![text("Open orders").size(style::text_size::SMALL)].spacing(4);
                for o in orders {
                    let tail: String = {
                        let n = o.id.chars().count();
                        o.id.chars().skip(n.saturating_sub(6)).collect()
                    };
                    list = list.push(
                        row![
                            text(format!("{} …{tail}", o.symbol)).size(style::text_size::SMALL),
                            button(text("Cancel").size(style::text_size::SMALL)).on_press_maybe(
                                (!cancel_blocked).then(|| Message::CancelOrder(
                                    o.symbol.clone(),
                                    o.id.clone(),
                                    o.kind.clone(),
                                ))
                            ),
                        ]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                    );
                }
                content = content.push(list);
            }
        }

        container(content)
            .max_width(360)
            .padding(24)
            .style(style::dashboard_modal)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_gate() -> Gate {
        Gate {
            boot_done: true,
            account: Some((
                AccountVenue::Binance,
                AccountProduct::UsdM,
                AccountEnv::Testnet,
            )),
            tested: true,
            needs_retest: false,
            live_open: false,
            signed_ok: true,
            record_locked: false,
            pair: None,
            stream: Some(StreamState::Connected),
            kind: Some(Kind::Limit),
            preparing: false,
        }
    }

    #[test]
    fn a_good_gate_allows_a_review() {
        assert_eq!(blocked_reason(&ok_gate()), None);
    }

    #[test]
    fn each_reason_blocks_and_the_first_one_wins() {
        let mut cases: Vec<(Gate, &str)> = Vec::new();
        let mut g = ok_gate();
        g.boot_done = false;
        cases.push((g, "Starting up"));
        let mut g = ok_gate();
        g.account = None;
        cases.push((g, "Pick an account"));
        let mut g = ok_gate();
        g.preparing = true;
        cases.push((g, "Preparing"));
        let mut g = ok_gate();
        g.account = Some((
            AccountVenue::Binance,
            AccountProduct::Spot,
            AccountEnv::Testnet,
        ));
        cases.push((g, "spot"));
        let mut g = ok_gate();
        g.account = Some((AccountVenue::Okx, AccountProduct::UsdM, AccountEnv::Live));
        cases.push((g, "Live trading is not open"));
        let mut g = ok_gate();
        g.tested = false;
        cases.push((g, "Test the account"));
        let mut g = ok_gate();
        g.needs_retest = true;
        cases.push((g, "refused"));
        let mut g = ok_gate();
        g.signed_ok = false;
        cases.push((g, "connection"));
        let mut g = ok_gate();
        g.record_locked = true;
        cases.push((g, "unknown result"));
        let mut g = ok_gate();
        g.pair = Some(PairState::InFlight);
        cases.push((g, "being sent"));
        let mut g = ok_gate();
        g.pair = Some(PairState::Unresolved);
        cases.push((g, "unknown"));
        let mut g = ok_gate();
        g.account = Some((AccountVenue::Okx, AccountProduct::UsdM, AccountEnv::Testnet));
        g.kind = Some(Kind::Trailing);
        cases.push((g, "Trailing stops are off on OKX"));
        let mut g = ok_gate();
        g.stream = Some(StreamState::AuthFailed);
        g.kind = Some(Kind::Stop);
        cases.push((g, "paused"));
        for state in [
            None,
            Some(StreamState::Reconnecting),
            Some(StreamState::Resyncing),
            Some(StreamState::Unavailable),
        ] {
            let mut g = ok_gate();
            g.stream = state;
            cases.push((g, "private stream"));
        }
        for (g, want) in cases {
            let why = blocked_reason(&g).unwrap_or_else(|| panic!("{g:?} was not blocked"));
            assert!(why.contains(want), "{why} should contain {want}");
        }
    }

    #[test]
    fn a_protective_order_or_a_cancel_does_not_need_a_connected_stream() {
        for state in [
            None,
            Some(StreamState::Reconnecting),
            Some(StreamState::Resyncing),
            Some(StreamState::Unavailable),
        ] {
            let mut g = ok_gate();
            g.stream = state;
            g.kind = Some(Kind::Stop);
            assert_eq!(blocked_reason(&g), None);
            g.kind = None;
            assert_eq!(blocked_reason(&g), None);
        }
    }

    #[test]
    fn a_boot_that_is_not_done_blocks_before_anything_else() {
        let g = Gate::default();
        assert!(blocked_reason(&g).unwrap().contains("Starting up"));
    }

    #[test]
    fn the_form_becomes_a_request_and_bad_numbers_are_named() {
        let spec = build_spec(Kind::Limit, Dir::Long, false, "0.5", "100", "", "").unwrap();
        assert!(matches!(spec, ManualSpec::Limit { .. }));
        let spec = build_spec(Kind::Stop, Dir::Long, true, "", "", "90", "").unwrap();
        assert!(matches!(
            spec,
            ManualSpec::Stop {
                size: Size::ClosePosition,
                ..
            }
        ));
        assert!(
            build_spec(Kind::Market, Dir::Long, false, "abc", "", "", "")
                .unwrap_err()
                .contains("Quantity")
        );
        assert!(
            build_spec(Kind::Limit, Dir::Long, false, "1", "", "", "")
                .unwrap_err()
                .contains("Price")
        );
        assert!(
            build_spec(Kind::Trailing, Dir::Short, false, "1", "", "", "x")
                .unwrap_err()
                .contains("Callback")
        );
    }

    #[test]
    fn a_typed_symbol_that_could_panic_the_ticker_is_refused() {
        assert!(valid_symbol("BTCUSDT"));
        assert!(valid_symbol("BTC-USDT-SWAP"));
        assert!(valid_symbol(&"A".repeat(28)));
        assert!(!valid_symbol(&"A".repeat(29)));
        assert!(!valid_symbol(&"A".repeat(40)));
        assert!(!valid_symbol(""));
        assert!(!valid_symbol("ＢＴＣ"));
        assert!(!valid_symbol("BTC|USDT"));
        assert!(!valid_symbol("BTC USDT"));
        let mut t = OrderTicket {
            account: Some(Uuid::nil()),
            kind: Some(Kind::Market),
            dir: Some(DirChoice::Long),
            qty: "1".into(),
            ..OrderTicket::default()
        };
        for bad in ["A".repeat(40), "ＢＴＣ".to_string(), "A|B".to_string()] {
            t.symbol = bad;
            assert!(t.request().is_err());
        }
        t.symbol = "BTCUSDT".into();
        assert!(t.request().is_ok());
    }

    #[test]
    fn a_stale_preview_answer_is_dropped() {
        let mut t = OrderTicket::default();
        let first = t.start_prepare();
        let second = t.start_prepare();
        assert!(!t.is_current(first));
        assert!(t.is_current(second));
        t.finish_prepare();
        assert!(!t.is_current(second));
    }

    #[test]
    fn the_symbol_is_upper_cased_and_a_review_needs_the_whole_form() {
        let mut t = OrderTicket::default();
        t.update(Message::Symbol("btcusdt".into()));
        assert_eq!(t.symbol, "BTCUSDT");
        assert!(t.update(Message::Review).is_none());
        assert!(t.status.is_some());
    }

    #[test]
    fn the_form_is_frozen_while_a_preview_is_being_made() {
        let mut t = OrderTicket {
            account: Some(Uuid::nil()),
            kind: Some(Kind::Market),
            dir: Some(DirChoice::Long),
            symbol: "BTCUSDT".into(),
            qty: "1".into(),
            ..OrderTicket::default()
        };
        let nonce = t.start_prepare();
        let other = AccountChoice {
            id: Uuid::from_u128(7),
            label: "other".into(),
        };
        assert!(t.update(Message::Account(other)).is_none());
        assert_eq!(t.account, Some(Uuid::nil()));
        t.update(Message::Kind(Kind::Limit));
        t.update(Message::Symbol("ethusdt".into()));
        t.update(Message::Dir(DirChoice::Short));
        t.update(Message::Qty("9".into()));
        t.update(Message::Price("1".into()));
        t.update(Message::Trigger("1".into()));
        t.update(Message::Callback("1".into()));
        t.update(Message::Whole(true));
        assert_eq!(t.kind, Some(Kind::Market));
        assert_eq!(t.symbol, "BTCUSDT");
        assert_eq!(t.qty, "1");
        assert!(!t.whole);
        // No second review or cancel can start, and the review in progress stays current.
        assert!(t.update(Message::Review).is_none());
        assert!(
            t.update(Message::CancelOrder(
                "X".into(),
                "1".into(),
                OpenKind::Plain
            ))
            .is_none()
        );
        assert!(t.is_current(nonce));
        // Once it ended the form works again.
        t.finish_prepare();
        t.update(Message::Qty("2".into()));
        assert_eq!(t.qty, "2");
    }
}
