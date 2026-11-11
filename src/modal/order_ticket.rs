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
use data::config::protect::{ProtectDefaults, ProtectOffset, TakeProfitLeg, TrailingDefault};
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

/// What protection this entry gets: the account's saved defaults, none, or values typed for
/// this order only. Whatever is chosen is checked by `ProtectDefaults::spec` when the order is
/// prepared, and none of it can switch attaching on (see `protect_attach::resolve`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProtectChoice {
    #[default]
    Defaults,
    Off,
    Custom,
}

impl ProtectChoice {
    const ALL: [ProtectChoice; 3] = [
        ProtectChoice::Defaults,
        ProtectChoice::Off,
        ProtectChoice::Custom,
    ];
}

impl fmt::Display for ProtectChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ProtectChoice::Defaults => "Account defaults",
            ProtectChoice::Off => "No protection",
            ProtectChoice::Custom => "Custom for this order",
        })
    }
}

/// A stop or target as a percent of the fill price or as a price distance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OffsetChoice {
    #[default]
    Percent,
    Distance,
}

impl OffsetChoice {
    const ALL: [OffsetChoice; 2] = [OffsetChoice::Percent, OffsetChoice::Distance];

    fn offset(self, value: &str) -> ProtectOffset {
        match self {
            OffsetChoice::Percent => ProtectOffset::Percent(value.trim().to_string()),
            OffsetChoice::Distance => ProtectOffset::Distance(value.trim().to_string()),
        }
    }
}

impl fmt::Display for OffsetChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            OffsetChoice::Percent => "% of fill price",
            OffsetChoice::Distance => "Price distance",
        })
    }
}

/// The typed override as defaults to check, or `None` for the account's own. The numbers stay
/// text here: `ProtectDefaults::spec` is the one place they are parsed and judged. A custom
/// choice with every field empty is an error, not "no protection".
pub fn build_override(
    choice: ProtectChoice,
    stop: (OffsetChoice, &str),
    target: (OffsetChoice, &str),
    trailing: &str,
) -> Result<Option<ProtectDefaults>, String> {
    match choice {
        ProtectChoice::Defaults => Ok(None),
        ProtectChoice::Off => Ok(Some(ProtectDefaults::default())),
        ProtectChoice::Custom => {
            let given = |s: &str| !s.trim().is_empty();
            let custom = ProtectDefaults {
                stop_loss: given(stop.1).then(|| stop.0.offset(stop.1)),
                // One level takes the whole quantity.
                take_profits: if given(target.1) {
                    vec![TakeProfitLeg {
                        offset: target.0.offset(target.1),
                        share_pct: 100,
                    }]
                } else {
                    Vec::new()
                },
                trailing: given(trailing).then(|| TrailingDefault {
                    callback_pct: trailing.trim().to_string(),
                    activation: None,
                }),
                ..ProtectDefaults::default()
            };
            if custom.is_empty() {
                return Err(
                    "Enter a stop-loss, take-profit or trailing value, or pick another \
                     protection choice."
                        .to_string(),
                );
            }
            Ok(Some(custom))
        }
    }
}

/// One line on the saved defaults of an account, for the ticket.
pub fn defaults_line(p: &ProtectDefaults, env: AccountEnv) -> String {
    let off = |o: &ProtectOffset| match o {
        ProtectOffset::Percent(v) => format!("{v}%"),
        ProtectOffset::Distance(v) => format!("{v} away"),
    };
    let mut parts = Vec::new();
    if let Some(o) = &p.stop_loss {
        parts.push(format!("stop {}", off(o)));
    }
    for (i, t) in p.take_profits.iter().enumerate() {
        parts.push(format!(
            "take-profit {} {}% ({})",
            i + 1,
            t.share_pct,
            off(&t.offset)
        ));
    }
    if let Some(t) = &p.trailing {
        parts.push(format!("trailing {}%", t.callback_pct));
    }
    let what = if parts.is_empty() {
        "none set".to_string()
    } else {
        parts.join(", ")
    };
    // The same rule the confirmation uses: a Live account needs its second switch too.
    let switch = if p.attaches_automatically(env) {
        "auto-attach on"
    } else if p.auto_attach {
        "auto-attach off: this Live account needs its own switch"
    } else {
        "auto-attach off"
    };
    format!("Account defaults: {what} ({switch})")
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
    Protect(ProtectChoice),
    StopKind(OffsetChoice),
    StopValue(String),
    TargetKind(OffsetChoice),
    TargetValue(String),
    TrailValue(String),
    Review,
    /// A cancel of one open order from the list.
    CancelOrder(String, String, OpenKind),
    /// The preview came back (the number tells which review it belongs to).
    Prepared(u64, Result<Box<Prepared>, String>),
    Close,
}

pub enum Action {
    /// Prepare a preview for this request (`main` checks the gate again).
    /// Boxed: the request carries the protection override.
    Review(Box<PrepareRequest>),
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
    protect: ProtectChoice,
    stop_kind: OffsetChoice,
    stop_value: String,
    target_kind: OffsetChoice,
    target_value: String,
    trail_value: String,
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
        return Some("Preparing the order… Press Back to cancel.");
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

    /// The account picked in the form.
    pub fn account(&self) -> Option<Uuid> {
        self.account
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
        // Only an entry carries protection.
        let protect = if kind.is_entry() {
            build_override(
                self.protect,
                (self.stop_kind, &self.stop_value),
                (self.target_kind, &self.target_value),
                &self.trail_value,
            )?
        } else {
            None
        };
        Ok(PrepareRequest {
            account,
            symbol: self.symbol.trim().to_string(),
            spec,
            protect,
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
                // Values typed for the previous account never carry over to the new one.
                self.protect = ProtectChoice::default();
                self.stop_value.clear();
                self.target_value.clear();
                self.trail_value.clear();
            }
            Message::Kind(k) => self.kind = Some(k),
            Message::Symbol(s) => self.symbol = s.to_uppercase(),
            Message::Dir(d) => self.dir = Some(d),
            Message::Qty(s) => self.qty = s,
            Message::Price(s) => self.price = s,
            Message::Trigger(s) => self.trigger = s,
            Message::Callback(s) => self.callback = s,
            Message::Whole(b) => self.whole = b,
            Message::Protect(c) => self.protect = c,
            Message::StopKind(k) => self.stop_kind = k,
            Message::StopValue(s) => self.stop_value = s,
            Message::TargetKind(k) => self.target_kind = k,
            Message::TargetValue(s) => self.target_value = s,
            Message::TrailValue(s) => self.trail_value = s,
            Message::Review => match self.request() {
                // `main` starts the review (it owns the gate and the clients).
                Ok(req) => return Some(Action::Review(Box::new(req))),
                Err(e) => self.status = Some((e, BannerKind::Error)),
            },
            Message::CancelOrder(symbol, id, kind) => {
                let Some(account) = self.account else {
                    self.status = Some(("Pick an account.".to_string(), BannerKind::Error));
                    return None;
                };
                return Some(Action::Review(Box::new(PrepareRequest {
                    account,
                    symbol,
                    spec: ManualSpec::Cancel { id, kind },
                    protect: None,
                })));
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

        if self.kind.is_some_and(Kind::is_entry) {
            content = content.push(text("Protection").size(style::text_size::SMALL));
            if let Some(m) = meta {
                content = content
                    .push(text(defaults_line(&m.protect, m.env)).size(style::text_size::SMALL));
            }
            content = content.push(pick(
                pick_list(ProtectChoice::ALL, Some(self.protect), Message::Protect).into(),
                self.protect.to_string(),
            ));
            if self.protect == ProtectChoice::Custom {
                content = content
                    .push(
                        row![
                            pick(
                                pick_list(
                                    OffsetChoice::ALL,
                                    Some(self.stop_kind),
                                    Message::StopKind
                                )
                                .into(),
                                self.stop_kind.to_string()
                            ),
                            field("Stop-loss", &self.stop_value, Message::StopValue),
                        ]
                        .spacing(8),
                    )
                    .push(
                        row![
                            pick(
                                pick_list(
                                    OffsetChoice::ALL,
                                    Some(self.target_kind),
                                    Message::TargetKind
                                )
                                .into(),
                                self.target_kind.to_string()
                            ),
                            field("Take-profit", &self.target_value, Message::TargetValue),
                        ]
                        .spacing(8),
                    )
                    .push(field(
                        "Trailing callback % (Binance one-way)",
                        &self.trail_value,
                        Message::TrailValue,
                    ));
            }
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
    fn form(kind: Kind) -> OrderTicket {
        OrderTicket {
            account: Some(Uuid::nil()),
            kind: Some(kind),
            dir: Some(DirChoice::Long),
            symbol: "BTCUSDT".into(),
            qty: "1".into(),
            price: "100".into(),
            ..OrderTicket::default()
        }
    }

    #[test]
    fn the_override_choices_map_to_what_is_checked_later() {
        use OffsetChoice::{Distance, Percent};
        let none = (Percent, "");
        // The account's own defaults: no override at all.
        assert_eq!(
            build_override(ProtectChoice::Defaults, none, none, ""),
            Ok(None)
        );
        // "No protection" is an empty override, which differs from "no override".
        assert_eq!(
            build_override(ProtectChoice::Off, (Percent, "5"), none, "1"),
            Ok(Some(ProtectDefaults::default()))
        );
        let custom = build_override(
            ProtectChoice::Custom,
            (Percent, " 2 "),
            (Distance, "300"),
            "1.5",
        )
        .unwrap()
        .unwrap();
        assert_eq!(custom.stop_loss, Some(ProtectOffset::Percent("2".into())));
        assert_eq!(custom.take_profits.len(), 1);
        assert_eq!(custom.take_profits[0].share_pct, 100);
        assert_eq!(
            custom.take_profits[0].offset,
            ProtectOffset::Distance("300".into())
        );
        assert_eq!(custom.trailing.as_ref().unwrap().callback_pct, "1.5");
        // Only a trailing value is something too.
        assert!(
            build_override(ProtectChoice::Custom, none, none, "1")
                .unwrap()
                .is_some()
        );
        // Custom with nothing typed is a mistake, not "no protection".
        assert!(build_override(ProtectChoice::Custom, none, none, "  ").is_err());
        // A typed value is not judged here: the one check is `ProtectDefaults::spec`.
        assert!(
            build_override(ProtectChoice::Custom, (Percent, "abc"), none, "")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn only_an_entry_carries_the_override_and_it_does_not_follow_an_account_change() {
        let mut t = form(Kind::Limit);
        t.update(Message::Protect(ProtectChoice::Custom));
        t.update(Message::StopValue("2".into()));
        let r = t.request().unwrap();
        assert_eq!(
            r.protect.as_ref().and_then(|p| p.stop_loss.clone()),
            Some(ProtectOffset::Percent("2".into()))
        );
        // A protective order of its own never carries one.
        let mut stop = form(Kind::Stop);
        stop.trigger = "90".into();
        stop.protect = ProtectChoice::Custom;
        stop.stop_value = "2".into();
        assert_eq!(stop.request().unwrap().protect, None);
        // A custom choice with nothing typed stops the review with a message.
        t.update(Message::StopValue(String::new()));
        assert!(t.request().is_err());
        // Another account: the typed values are gone.
        t.update(Message::StopValue("2".into()));
        t.update(Message::Account(AccountChoice {
            id: Uuid::from_u128(5),
            label: "other".into(),
        }));
        assert_eq!(t.protect, ProtectChoice::Defaults);
        assert!(t.stop_value.is_empty());
        assert_eq!(t.request().unwrap().protect, None);
    }

    #[test]
    fn the_defaults_are_described_on_one_line() {
        let p = ProtectDefaults {
            stop_loss: Some(ProtectOffset::Percent("2".into())),
            take_profits: vec![TakeProfitLeg {
                offset: ProtectOffset::Distance("300".into()),
                share_pct: 100,
            }],
            ..ProtectDefaults::default()
        };
        let line = defaults_line(&p, AccountEnv::Testnet);
        assert!(
            line.contains("stop 2%") && line.contains("300 away"),
            "{line}"
        );
        assert!(line.contains("auto-attach off"), "{line}");
        assert!(
            defaults_line(&ProtectDefaults::default(), AccountEnv::Testnet).contains("none set")
        );
        // The first switch on: a testnet account attaches, a Live one still needs its own.
        let on = ProtectDefaults {
            auto_attach: true,
            ..p
        };
        assert!(defaults_line(&on, AccountEnv::Testnet).ends_with("(auto-attach on)"));
        let live = defaults_line(&on, AccountEnv::Live);
        assert!(
            live.contains("needs its own switch") && !live.contains("auto-attach on"),
            "{live}"
        );
        let both = ProtectDefaults {
            auto_attach_live: true,
            ..on
        };
        assert!(defaults_line(&both, AccountEnv::Live).ends_with("(auto-attach on)"));
    }

    /// Condition 75(4): nothing puts an indicator value, a chart value or a study result into
    /// the order form. The form changes only through its own messages, and no other file
    /// feeds it a value.
    #[test]
    fn the_form_takes_no_value_from_an_indicator_or_the_chart() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let code_of = |rel: &str| {
            let text = std::fs::read_to_string(src.join(rel)).unwrap();
            text.split("#[cfg(test)]")
                .next()
                .unwrap_or_default()
                .to_string()
        };
        for file in [
            "modal/order_ticket.rs",
            "modal/order_confirm.rs",
            "order_flow.rs",
            "protect_attach.rs",
        ] {
            let code = code_of(file).to_lowercase();
            for banned in [
                "indicator",
                "studies",
                "study",
                "kline",
                "heatmap",
                "footprint",
            ] {
                assert!(!code.contains(banned), "{file} mentions {banned}");
            }
        }
        // `main` talks to the ticket with `Prepared` only: no value is pushed into a field.
        for line in code_of("main.rs")
            .lines()
            .filter(|l| l.contains("order_ticket::Message::"))
        {
            assert!(line.contains("Message::Prepared("), "{line}");
        }
        let mut stack = vec![src.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs")
                    || path.ends_with("modal/order_ticket.rs")
                {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                let code = text.split("#[cfg(test)]").next().unwrap_or_default();
                for field in [
                    "Qty(",
                    "Price(",
                    "Trigger(",
                    "Callback(",
                    "StopValue(",
                    "TargetValue(",
                ] {
                    assert!(
                        !code.contains(&format!("order_ticket::Message::{field}")),
                        "{} feeds the order form ({field})",
                        path.display()
                    );
                }
            }
        }
    }
}
