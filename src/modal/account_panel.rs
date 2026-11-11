//! The account section under the order ticket (M5 c2b-3): the orders and positions the private
//! stream last reported (read-only, marked when they may be stale), cancel all and close all,
//! the risk lock, and the saved unknown results with their manual release and discard.
//!
//! The typed confirmation phrases live here and nowhere else. No text field is ever filled in
//! by the program and nothing here submits on a key press: only a button does.

use crate::{
    account_flow::ChallengeSlot,
    auto_flow::FailureView,
    modal::order_ticket::{Gate, blocked_reason},
    style,
    trading::{BareRow, RecordStatus, StrayOrder, TradingState, account_key, display_name},
    widget::{BannerKind, banner},
};
use data::config::exchange_accounts::AccountMeta;
use exchange::account::{AccountProduct, AccountVenue};
use exchange::trade::Dir;
use exchange::trade::{
    BulkAction, DiscardChallenge, DiscardToken, ManualReleaseToken, ReleaseChallenge, UnknownRecord,
};
use iced::{
    Element,
    widget::{button, column, row, text, text_input},
};
use std::fmt;

/// What a click in the panel asks `main` to do. Not a `Message`: it can carry a token.
pub enum Action {
    Bulk(BulkAction),
    /// Offer a stop again for the entry with this client id (its position's protection failed).
    /// The key names the entry by value; `main` looks it up, never trusts a stale position.
    Retry(String),
    /// The user confirmed "I've handled it" for the entry with this client id.
    Handled(String),
    /// Offer a whole-position stop for the position of this symbol and direction (a bare
    /// position found in the lists). Opens the confirmation; nothing is sent by the click.
    AddStop(String, Dir),
    /// Offer to cancel the order with this exchange id (an order with no position, or one left
    /// over). Opens the ordinary confirmation of a cancel.
    CancelStray(String),
    AckTrip,
    BeginRelease(Box<UnknownRecord>),
    Release(ManualReleaseToken, Box<UnknownRecord>),
    Discard(DiscardToken, Box<UnknownRecord>),
}

#[derive(Clone)]
pub enum Message {
    CancelAll,
    CloseAll,
    AckTrip,
    /// Offer a stop again for the entry of this key (client id of the entry, by value).
    Retry(String),
    /// Ask to remove the entry of this key from the protection list; confirmed on a second step.
    Handled(String),
    ConfirmHandled,
    CancelHandled,
    AddStop(String, Dir),
    CancelStray(String),
    /// The client id of the record, looked up by value in the records the panel holds (a list
    /// that was replaced between the view and the click never points at another record).
    Release(String),
    Discard(String),
    Typed(String),
    Submit,
    CloseForm,
}

/// The typed text and record ids stay out of any log.
impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Message::CancelAll => "CancelAll",
            Message::CloseAll => "CloseAll",
            Message::AckTrip => "AckTrip",
            Message::Retry(_) => "Retry(..)",
            Message::Handled(_) => "Handled(..)",
            Message::AddStop(..) => "AddStop(..)",
            Message::CancelStray(_) => "CancelStray(..)",
            Message::ConfirmHandled => "ConfirmHandled",
            Message::CancelHandled => "CancelHandled",
            Message::Release(_) => "Release(..)",
            Message::Discard(_) => "Discard(..)",
            Message::Typed(_) => "Typed(<redacted>)",
            Message::Submit => "Submit",
            Message::CloseForm => "CloseForm",
        })
    }
}

enum Challenge {
    /// The challenge is being made (it needs a client).
    Release(Option<ReleaseChallenge>),
    Discard(DiscardChallenge),
}

/// One release or discard being typed.
struct Form {
    record: UnknownRecord,
    challenge: Option<Challenge>,
    typed: String,
    /// The token went out: waiting for the answer.
    sent: bool,
}

#[derive(Default)]
pub struct AccountPanel {
    records: Vec<UnknownRecord>,
    form: Option<Form>,
    status: Option<(String, BannerKind)>,
    /// Bumped for each bulk preparation, so an older answer is told apart.
    bulk_nonce: u64,
    bulk_preparing: bool,
    /// The entry whose "I've handled it" waits for its second click (key by value).
    handling: Option<String>,
}

/// Why cancel-all / close-all cannot start now, or `None`. `gate` has no order kind, so a
/// stopped stream does not block (a cancel reduces risk); the rest is the ticket's gate.
pub fn bulk_blocked_reason(
    gate: &Gate,
    deleting: bool,
    running: bool,
    confirm_open: bool,
) -> Option<&'static str> {
    if deleting {
        return Some("The account is being deleted.");
    }
    if running {
        return Some("A cancel all or close all of this account is running.");
    }
    if confirm_open {
        return Some("Another confirmation is open. Finish or cancel it first.");
    }
    blocked_reason(gate)
}

/// Close all makes no sense for a spot account (nothing to flatten), on any venue.
pub fn flatten_offered(venue: AccountVenue, product: AccountProduct) -> bool {
    let _ = venue;
    product != AccountProduct::Spot
}

impl AccountPanel {
    pub fn set_records(&mut self, records: Vec<UnknownRecord>) {
        self.records = records;
        // A form for a record that is gone is dropped (the form names it by value).
        if let Some(form) = &self.form
            && !form.sent
            && !self.records.iter().any(|r| same(r, &form.record))
        {
            self.form = None;
        }
    }

    /// The record of the ticket's account with this client id.
    fn record_of(&self, meta: &AccountMeta, client_id: &str) -> Option<&UnknownRecord> {
        self.records
            .iter()
            .find(|r| r.account == account_key(meta.id) && r.client_id == client_id)
    }

    pub fn set_status(&mut self, text: String, kind: BannerKind) {
        self.status = Some((text, kind));
    }

    pub fn start_bulk(&mut self) -> u64 {
        self.bulk_nonce += 1;
        self.bulk_preparing = true;
        self.status = None;
        self.bulk_nonce
    }

    pub fn is_bulk_current(&self, nonce: u64) -> bool {
        self.bulk_preparing && self.bulk_nonce == nonce
    }

    pub fn is_bulk_preparing(&self) -> bool {
        self.bulk_preparing
    }

    pub fn finish_bulk_prepare(&mut self) {
        self.bulk_preparing = false;
    }

    /// Leaving the panel: nothing half typed stays behind.
    pub fn reset_form(&mut self) {
        self.form = None;
        self.handling = None;
    }

    /// The challenge of a release arrived (or failed). The input starts empty.
    pub fn release_begun(&mut self, record: &UnknownRecord, result: Result<ChallengeSlot, String>) {
        let Some(form) = self.form.as_mut().filter(|f| same(&f.record, record)) else {
            return;
        };
        match result {
            Ok(slot) => match slot.take() {
                Some(c) => form.challenge = Some(Challenge::Release(Some(c))),
                None => self.fail_form("The release could not be started."),
            },
            Err(why) => self.fail_form(&why),
        }
    }

    fn fail_form(&mut self, why: &str) {
        self.form = None;
        self.status = Some((why.to_string(), BannerKind::Error));
    }

    /// The release or discard ended: the form goes, the answer is shown by `main`.
    pub fn form_done(&mut self) {
        self.form = None;
    }

    /// `deleting`: the ticket's account is being deleted (nothing is started for it).
    pub fn update(
        &mut self,
        message: Message,
        trading: &TradingState,
        account: Option<&AccountMeta>,
        deleting: bool,
    ) -> Option<Action> {
        match message {
            Message::CancelAll => return Some(Action::Bulk(BulkAction::CancelAll)),
            Message::CloseAll => return Some(Action::Bulk(BulkAction::Flatten)),
            Message::AckTrip => return Some(Action::AckTrip),
            Message::Retry(key) => {
                self.handling = None;
                if deleting {
                    self.status = Some((DELETING.to_string(), BannerKind::Error));
                    return None;
                }
                self.status = None;
                return Some(Action::Retry(key));
            }
            Message::AddStop(symbol, dir) => {
                self.handling = None;
                if deleting {
                    self.status = Some((DELETING.to_string(), BannerKind::Error));
                    return None;
                }
                self.status = None;
                return Some(Action::AddStop(symbol, dir));
            }
            Message::CancelStray(id) => {
                self.handling = None;
                if deleting {
                    self.status = Some((DELETING.to_string(), BannerKind::Error));
                    return None;
                }
                self.status = None;
                return Some(Action::CancelStray(id));
            }
            Message::Handled(key) => {
                // One more click: the entry is not removed by a single press.
                self.status = None;
                self.handling = Some(key);
            }
            Message::CancelHandled => self.handling = None,
            Message::ConfirmHandled => {
                let key = self.handling.take()?;
                if deleting {
                    self.status = Some((DELETING.to_string(), BannerKind::Error));
                    return None;
                }
                return Some(Action::Handled(key));
            }
            Message::Release(client_id) => {
                let meta = account?;
                let record = self.record_of(meta, &client_id)?.clone();
                if deleting {
                    self.status = Some((DELETING.to_string(), BannerKind::Error));
                    return None;
                }
                if !trading.record_status(meta.id, &record).may_release() {
                    return None;
                }
                self.status = None;
                self.form = Some(Form {
                    record: record.clone(),
                    challenge: Some(Challenge::Release(None)),
                    typed: String::new(),
                    sent: false,
                });
                return Some(Action::BeginRelease(Box::new(record)));
            }
            Message::Discard(client_id) => {
                let meta = account?;
                let record = self.record_of(meta, &client_id)?.clone();
                if deleting {
                    self.status = Some((DELETING.to_string(), BannerKind::Error));
                    return None;
                }
                if !trading.record_status(meta.id, &record).may_discard() {
                    return None;
                }
                // The challenge of a discard needs no client: it comes out of the record.
                match record.begin_discard() {
                    Ok(c) => {
                        self.status = None;
                        self.form = Some(Form {
                            record,
                            challenge: Some(Challenge::Discard(c)),
                            typed: String::new(),
                            sent: false,
                        });
                    }
                    Err(_) => {
                        self.status = Some((
                            "This record has a valid id and cannot be discarded.".to_string(),
                            BannerKind::Error,
                        ));
                    }
                }
            }
            Message::Typed(s) => {
                if let Some(f) = self.form.as_mut().filter(|f| !f.sent) {
                    f.typed = s;
                }
            }
            Message::Submit => return self.check_phrase(trading, account, deleting),
            Message::CloseForm => {
                if self.form.as_ref().is_some_and(|f| !f.sent) {
                    self.form = None;
                }
            }
        }
        None
    }

    /// The phrase is checked by the exchange layer's challenge. A wrong phrase gives the
    /// challenge back and keeps the block.
    ///
    /// The record's state is judged again here: the form may have been opened before a lookup,
    /// a bulk run or a deletion began.
    fn check_phrase(
        &mut self,
        trading: &TradingState,
        account: Option<&AccountMeta>,
        deleting: bool,
    ) -> Option<Action> {
        let form = self.form.as_ref().filter(|f| !f.sent)?;
        let still_allowed = account.is_some_and(|m| {
            form.record.account == account_key(m.id) && {
                let status = trading.record_status(m.id, &form.record);
                match form.challenge {
                    Some(Challenge::Discard(_)) => status.may_discard(),
                    _ => status.may_release(),
                }
            }
        });
        if deleting || !still_allowed {
            self.form = None;
            let why = if deleting {
                DELETING
            } else {
                "The record changed while the form was open. Nothing was changed; open it again."
            };
            self.status = Some((why.to_string(), BannerKind::Error));
            return None;
        }
        let form = self.form.as_mut().filter(|f| !f.sent)?;
        let typed = std::mem::take(&mut form.typed);
        match form.challenge.take()? {
            Challenge::Release(Some(c)) => match c.confirm(&typed) {
                Ok(token) => {
                    form.sent = true;
                    Some(Action::Release(token, Box::new(form.record.clone())))
                }
                Err(c) => {
                    form.challenge = Some(Challenge::Release(Some(*c)));
                    self.status = Some((MISMATCH.to_string(), BannerKind::Error));
                    None
                }
            },
            Challenge::Release(None) => {
                form.challenge = Some(Challenge::Release(None));
                form.typed = typed;
                None
            }
            Challenge::Discard(c) => match c.confirm(&typed) {
                Ok(token) => {
                    form.sent = true;
                    Some(Action::Discard(token, Box::new(form.record.clone())))
                }
                Err(c) => {
                    form.challenge = Some(Challenge::Discard(*c));
                    self.status = Some((MISMATCH.to_string(), BannerKind::Error));
                    None
                }
            },
        }
    }

    /// The text field of the form, if one is open: always the user's own typing.
    #[cfg(test)]
    fn typed(&self) -> Option<&str> {
        self.form.as_ref().map(|f| f.typed.as_str())
    }

    #[cfg(test)]
    fn phrase(&self) -> Option<String> {
        match self.form.as_ref()?.challenge.as_ref()? {
            Challenge::Release(c) => c.as_ref().map(|c| c.phrase().to_string()),
            Challenge::Discard(c) => Some(c.phrase().to_string()),
        }
    }

    pub fn view<'a>(
        &'a self,
        meta: &AccountMeta,
        saved: &[AccountMeta],
        trading: &TradingState,
        bulk_blocked: Option<&'static str>,
    ) -> Element<'a, Message> {
        let small = style::text_size::SMALL;
        let mut col = column![
            text(format!("Account: {}", display_name(saved, meta))).size(style::text_size::SECTION)
        ]
        .spacing(6);

        // Orders and positions: what the stream last said.
        if trading.lists_stale(meta.id) {
            col = col.push(banner(
                "These lists may be out of date: the private stream is not connected or not \
                 synchronised.",
                BannerKind::Warning,
            ));
        }
        col = col.push(
            text(format!(
                "Open orders ({})",
                trading.open_orders(meta.id).len()
            ))
            .size(small),
        );
        for o in trading.open_orders(meta.id) {
            col = col.push(text(format!("  {}", o.symbol)).size(small));
        }
        col =
            col.push(text(format!("Positions ({})", trading.positions(meta.id).len())).size(small));
        // Protection per position: "cannot tell" while the lists may be out of date, never
        // "protected" (a status line only; the banners and actions come in d5).
        let protection = trading.protection(meta.id);
        for p in trading.positions(meta.id) {
            col = col.push(text(format!("  {} {:?} {}", p.symbol, p.side, p.qty)).size(small));
            if let Some(status) = protection
                .iter()
                .find(|s| s.symbol == p.symbol && s.dir == p.dir)
            {
                col = col.push(text(format!("    {}", status.protection.label())).size(small));
            }
            // The protective orders that stand for it (only when the lists are fresh).
            for line in trading.protective_lines(meta.id, &p.symbol, p.dir) {
                col = col.push(text(format!("    {line}")).size(small));
            }
        }

        // An entry with protection listed but no position yet (a resting limit order).
        let attached = trading.attached_on_exchange(meta.id);
        for symbol in trading.pending_without_position(meta.id) {
            // OKX: the exchange itself holds the attached orders for the resting entry.
            let line = if attached.contains(&symbol) {
                "Protection is attached on the exchange; it acts only once the entry is fully \
                 filled"
                    .to_string()
            } else {
                exchange::trade::Protection::EntryPending
                    .label()
                    .to_string()
            };
            col = col.push(text(format!("  {symbol}: {line}")).size(small));
        }

        // Cancel all / close all.
        let press = |m: Message| bulk_blocked.is_none().then_some(m);

        // Condition 65: what the app could not do by itself after a fill is shown until the
        // user has handled it (never retried by the app). Each entry offers a stop again (a
        // new confirmation), closing all positions (the same confirmation as "Close all"), and
        // removing the warning after a second click.
        let alias = display_name(saved, meta);
        let idle = bulk_blocked.is_none();
        // Positions with no stop-loss, found from the lists (d5): shown until they have one or
        // are gone. A position the failure banner below already speaks for is not repeated.
        for row in trading.bare_rows(meta.id) {
            col = col.push(bare_view(
                row,
                &alias,
                idle,
                flatten_offered(meta.venue, meta.product),
            ));
        }
        // Orders with no position to protect, and orders that look left over.
        for stray in trading.stray_orders(meta.id).unwrap_or_default() {
            col = col.push(stray_view(&stray, idle));
        }
        for f in trading.auto_failures(meta.id) {
            col = col.push(failure_view(
                f,
                &alias,
                self.handling.as_deref(),
                bulk_blocked.is_none(),
                flatten_offered(meta.venue, meta.product),
            ));
        }
        // Notes that are not failures of an entry.
        for note in trading.auto_info_notes(meta.id) {
            col = col.push(banner(note, BannerKind::Warning));
        }

        let mut bulk =
            row![button(text("Cancel all").size(small)).on_press_maybe(press(Message::CancelAll))]
                .spacing(8);
        if flatten_offered(meta.venue, meta.product) {
            bulk = bulk.push(
                button(text("Close all").size(small)).on_press_maybe(press(Message::CloseAll)),
            );
        }
        col = col.push(bulk);
        if let Some(why) = bulk_blocked {
            col = col.push(text(why).size(small));
        }

        // Risk lock and the automatic close setting (status only).
        if let Some(reason) = trading.trip_reason(meta.id) {
            col = col.push(banner(
                format!("Risk lock on: {reason}. New entries are blocked."),
                BannerKind::Error,
            ));
            col =
                col.push(button(text("Lift the risk lock").size(small)).on_press(Message::AckTrip));
        }
        if let Some(auto) = trading.auto_close_setting(meta.id) {
            col = col.push(
                text(format!(
                    "Auto close on a tripped limit: {} (status only; this app does not run an \
                     automatic close).",
                    if auto { "on" } else { "off" }
                ))
                .size(small),
            );
        }

        // Unknown results.
        let mine: Vec<&UnknownRecord> = self
            .records
            .iter()
            .filter(|r| r.account == account_key(meta.id))
            .collect();
        if !mine.is_empty() {
            col = col.push(text("Orders with an unknown result").size(small));
        }
        for r in mine {
            let status = trading.record_status(meta.id, r);
            col = col.push(text(format!("{}: {}", r.symbol, status.text())).size(small));
            let busy = self.form.is_some();
            col = col.push(record_buttons(&r.client_id, status, busy));
        }
        if let Some(form) = &self.form {
            col = col.push(form_view(form));
        }
        if let Some((s, kind)) = &self.status {
            col = col.push(banner(s.as_str(), *kind));
        }
        col.into()
    }
}

/// One entry of the protection list: the red banner of its failures with its buttons, or a plain
/// line for an entry that only still counts as protection at work. `idle`: nothing else is
/// running, so a confirmation can be opened.
fn failure_view<'a>(
    f: FailureView,
    alias: &str,
    handling: Option<&str>,
    idle: bool,
    flatten: bool,
) -> Element<'a, Message> {
    let small = style::text_size::SMALL;
    let mut col = column![].spacing(4);
    if f.reasons.is_empty() {
        col = col.push(
            text(format!(
                "{}: automatic protection still counts as at work and blocks a new entry on \
                 this symbol. If you know it is over, remove it.",
                f.symbol
            ))
            .size(small),
        );
    } else {
        let mut lines = format!("Protection failed on {alias}: {}", f.symbol);
        for r in &f.reasons {
            lines.push_str("\n- ");
            lines.push_str(r.text());
        }
        col = col.push(banner(lines, BannerKind::Error));
    }
    if handling == Some(f.key.as_str()) {
        col = col.push(text(handled_warning(&f)).size(small));
        col = col.push(
            row![
                button(text("Yes, I've handled it").size(small)).on_press(Message::ConfirmHandled),
                button(text("Back").size(small)).on_press(Message::CancelHandled),
            ]
            .spacing(8),
        );
        return col.into();
    }
    let mut buttons = row![].spacing(8);
    if f.may_retry {
        buttons = buttons.push(
            button(text("Retry protection").size(small))
                .on_press_maybe(idle.then(|| Message::Retry(f.key.clone()))),
        );
    }
    if flatten && !f.reasons.is_empty() {
        buttons = buttons.push(
            button(text("Flatten (close all positions)").size(small))
                .on_press_maybe(idle.then_some(Message::CloseAll)),
        );
    }
    buttons = buttons.push(
        button(text("I've handled it").size(small)).on_press(Message::Handled(f.key.clone())),
    );
    col.push(buttons).into()
}

/// The banner of one position that has no whole stop-loss, with its two buttons. "Add a stop-loss"
/// only opens the confirmation window; "Flatten" is the same confirmation as "Close all".
fn bare_view<'a>(row: &BareRow, alias: &str, idle: bool, flatten: bool) -> Element<'a, Message> {
    let small = style::text_size::SMALL;
    let kind = if row.lower_severity() {
        BannerKind::Warning
    } else {
        BannerKind::Error
    };
    let mut buttons = row![
        button(text("Add a stop-loss").size(small))
            .on_press_maybe(idle.then(|| Message::AddStop(row.symbol.clone(), row.dir)))
    ]
    .spacing(8);
    if flatten {
        buttons = buttons.push(
            button(text("Flatten (close all positions)").size(small))
                .on_press_maybe(idle.then_some(Message::CloseAll)),
        );
    }
    column![banner(row.text(alias), kind), buttons]
        .spacing(4)
        .into()
}

/// An order with no position (or one that looks left over) with its cancel button. The button
/// opens the confirmation window of a cancel; nothing is cancelled by the click.
fn stray_view<'a>(stray: &StrayOrder, idle: bool) -> Element<'a, Message> {
    let small = style::text_size::SMALL;
    let mut col = column![banner(stray.text.clone(), BannerKind::Warning)].spacing(4);
    if stray.guards_position {
        // Only told: the order stands for a position that is still held.
    } else if stray.can_cancel {
        col = col.push(
            button(text("Cancel").size(small))
                .on_press_maybe(idle.then(|| Message::CancelStray(stray.id.clone()))),
        );
    } else {
        col = col.push(
            text("This order cannot be cancelled from here. Cancel it on the exchange.")
                .size(small),
        );
    }
    col.into()
}

/// The text of the second step of "I've handled it": what stays behind once the entry is removed
/// from the book, said for this entry (T0: nothing may be left out of sight).
pub fn handled_warning(f: &FailureView) -> String {
    let mut text = String::from(
        "Remove this warning? Do it only after you checked the position and the open orders on \
         the exchange. The app stops watching this entry and a new entry on the symbol is \
         allowed. An order with an unknown result is not cleared by this.",
    );
    if f.entry_open {
        text.push_str("\nThe entry order is still open: a later fill will NOT get a stop-loss.");
    }
    if f.entry_unresolved {
        text.push_str("\nThe entry order may still be open: its result was never found out.");
    }
    if f.standing > 0 {
        text.push_str(&format!(
            "\n{} protective order{} of this entry may still stand on the exchange: cancel \
             {} first.",
            f.standing,
            if f.standing == 1 { "" } else { "s" },
            if f.standing == 1 { "it" } else { "them" }
        ));
    }
    text
}

const DELETING: &str = "The account is being deleted.";

const MISMATCH: &str = "The phrase does not match. Nothing was changed; the block stays.";

fn same(a: &UnknownRecord, b: &UnknownRecord) -> bool {
    a.account == b.account && a.client_id == b.client_id
}

fn record_buttons<'a>(client_id: &str, status: RecordStatus, busy: bool) -> Element<'a, Message> {
    let small = style::text_size::SMALL;
    let mut r = row![].spacing(8);
    if status.may_release() {
        r = r.push(
            button(text("Release by hand").size(small))
                .on_press_maybe((!busy).then(|| Message::Release(client_id.to_string()))),
        );
    }
    if status.may_discard() {
        r = r.push(
            button(text("Discard record").size(small))
                .on_press_maybe((!busy).then(|| Message::Discard(client_id.to_string()))),
        );
    }
    r.into()
}

fn form_view(form: &Form) -> Element<'_, Message> {
    let small = style::text_size::SMALL;
    let (intro, phrase) = match &form.challenge {
        Some(Challenge::Release(Some(c))) => (
            "Release by hand: the program will not resend or rebuild this order. Check the \
             exchange yourself first. Type the phrase below to continue.",
            Some(c.phrase().to_string()),
        ),
        Some(Challenge::Release(None)) | None => {
            // The challenge needs a client; Back drops the answer when it arrives.
            return column![
                text("Preparing the release… Press Back to cancel.").size(small),
                button(text("Back").size(small)).on_press(Message::CloseForm),
            ]
            .spacing(4)
            .into();
        }
        Some(Challenge::Discard(c)) => (
            "Discard the record: it is removed from the file, the account stays blocked for \
             new entries until the app restarts, and the discard is written to the audit log. \
             Type the phrase below to continue.",
            Some(c.phrase().to_string()),
        ),
    };
    let mut col = column![text(intro).size(small)].spacing(4);
    if let Some(p) = phrase {
        col = col.push(text(p).size(small));
    }
    if form.sent {
        return col.push(text("Working…").size(small)).into();
    }
    // Always empty at the start: the user types the phrase.
    col = col.push(text_input("type the phrase", &form.typed).on_input(Message::Typed));
    col.push(
        row![
            button(text("Confirm").size(small)).on_press(Message::Submit),
            button(text("Back").size(small)).on_press(Message::CloseForm),
        ]
        .spacing(8),
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::TradingState;
    use exchange::account::{AccountEnv, PositionMode, ProxyState};

    fn meta() -> AccountMeta {
        AccountMeta::new(
            "a".into(),
            AccountVenue::Binance,
            AccountEnv::Testnet,
            AccountProduct::UsdM,
            PositionMode::OneWay,
        )
    }

    fn failure(entry_open: bool, standing: usize) -> FailureView {
        FailureView {
            key: "k".into(),
            account: uuid::Uuid::nil(),
            symbol: "BTCUSDT".into(),
            reasons: Vec::new(),
            dir: Dir::Long,
            may_retry: false,
            active: true,
            entry_open,
            entry_unresolved: false,
            standing,
        }
    }

    #[test]
    fn the_handled_warning_says_what_stays_behind() {
        let plain = handled_warning(&failure(false, 0));
        assert!(plain.starts_with("Remove this warning?"));
        assert!(!plain.contains("still open") && !plain.contains("protective order"));
        let open = handled_warning(&failure(true, 0));
        assert!(
            open.contains("The entry order is still open: a later fill will NOT get a stop-loss.")
        );
        let one = handled_warning(&failure(false, 1));
        assert!(one.contains(
            "1 protective order of this entry may still stand on the exchange: cancel it first."
        ));
        let many = handled_warning(&failure(true, 3));
        assert!(many.contains(
            "3 protective orders of this entry may still stand on the exchange: cancel them first."
        ));
        assert!(many.contains("still open"));
    }

    #[test]
    fn a_stop_or_cancel_click_only_asks_main_to_open_a_review() {
        let m = meta();
        let trading = TradingState::new(ProxyState::Direct);
        let mut p = AccountPanel::default();
        match p.update(
            Message::AddStop("BTCUSDT".into(), Dir::Long),
            &trading,
            Some(&m),
            false,
        ) {
            Some(Action::AddStop(s, d)) => assert_eq!((s.as_str(), d), ("BTCUSDT", Dir::Long)),
            _ => panic!("an AddStop action was expected"),
        }
        assert!(matches!(
            p.update(Message::CancelStray("7".into()), &trading, Some(&m), false),
            Some(Action::CancelStray(id)) if id == "7"
        ));
        // Nothing is asked of an account that is being deleted.
        assert!(
            p.update(
                Message::AddStop("BTCUSDT".into(), Dir::Long),
                &trading,
                Some(&m),
                true
            )
            .is_none()
        );
    }

    const INVALID_ID: &str = "not-a-valid-client-id";

    fn invalid_record(m: &AccountMeta) -> UnknownRecord {
        UnknownRecord {
            account: account_key(m.id),
            symbol: "BTCUSDT".into(),
            client_id: INVALID_ID.into(),
            kind: "x".into(),
            recorded_at_ms: 0,
        }
    }

    #[test]
    fn the_discard_form_starts_empty_and_a_wrong_phrase_keeps_the_block() {
        let m = meta();
        let trading = TradingState::new(ProxyState::Direct);
        let mut p = AccountPanel::default();
        p.set_records(vec![invalid_record(&m)]);
        assert!(
            p.update(
                Message::Discard(INVALID_ID.to_string()),
                &trading,
                Some(&m),
                false
            )
            .is_none()
        );
        // Never prefilled: the field is empty and the phrase is only shown as text.
        assert_eq!(p.typed(), Some(""));
        assert!(p.phrase().is_some_and(|s| s.starts_with("discard BTCUSDT")));
        p.update(Message::Typed("discard".into()), &trading, Some(&m), false);
        assert!(
            p.update(Message::Submit, &trading, Some(&m), false)
                .is_none()
        );
        // The challenge came back: the form is still there and the text is gone.
        assert!(p.phrase().is_some());
        assert_eq!(p.typed(), Some(""));
        assert!(
            p.status
                .as_ref()
                .is_some_and(|(s, _)| s.contains("does not match"))
        );
    }

    #[test]
    fn the_exact_phrase_gives_a_token_once() {
        let m = meta();
        let trading = TradingState::new(ProxyState::Direct);
        let mut p = AccountPanel::default();
        p.set_records(vec![invalid_record(&m)]);
        p.update(
            Message::Discard(INVALID_ID.to_string()),
            &trading,
            Some(&m),
            false,
        );
        let phrase = p.phrase().unwrap();
        p.update(Message::Typed(phrase), &trading, Some(&m), false);
        assert!(matches!(
            p.update(Message::Submit, &trading, Some(&m), false),
            Some(Action::Discard(..))
        ));
        // Sent: a second click makes no second token.
        assert!(
            p.update(Message::Submit, &trading, Some(&m), false)
                .is_none()
        );
    }

    /// A saved record names the order and nothing else: no size, price, side or rules. This
    /// destructuring stops compiling if a field is added, so a release can never rebuild an
    /// order from it.
    #[test]
    fn a_record_carries_no_order_parameters() {
        let m = meta();
        let UnknownRecord {
            account: _,
            symbol: _,
            client_id: _,
            kind: _,
            recorded_at_ms: _,
        } = invalid_record(&m);
    }

    #[test]
    fn closing_the_form_while_the_challenge_is_prepared_drops_the_late_answer() {
        let m = meta();
        let trading = TradingState::new(ProxyState::Direct);
        let mut p = AccountPanel::default();
        let mut r = invalid_record(&m);
        r.client_id = valid_id();
        p.set_records(vec![r.clone()]);
        p.form = Some(Form {
            record: r.clone(),
            challenge: Some(Challenge::Release(None)),
            typed: String::new(),
            sent: false,
        });
        p.update(Message::CloseForm, &trading, Some(&m), false);
        assert!(p.form.is_none());
        p.release_begun(&r, Err("late".into()));
        assert!(p.form.is_none() && p.status.is_none());
    }

    fn booted_with(r: &UnknownRecord) -> TradingState {
        use crate::trading::plan_boot;
        let mut t = TradingState::new(ProxyState::Direct);
        t.on_booted(plan_boot(Ok(vec![r.clone()]), || {}, |_| Ok(())));
        t
    }

    fn release_form_for(p: &mut AccountPanel, r: &UnknownRecord) {
        p.form = Some(Form {
            record: r.clone(),
            challenge: Some(Challenge::Release(None)),
            typed: String::new(),
            sent: false,
        });
    }

    /// N1: a click names the record by its client id, so a list that changed since the view
    /// never makes the click open another record.
    #[test]
    fn a_click_finds_its_record_by_client_id_after_the_list_changed() {
        let m = meta();
        let trading = TradingState::new(ProxyState::Direct);
        let mut a = invalid_record(&m);
        a.client_id = "bad id A".into();
        let mut b = invalid_record(&m);
        b.client_id = "bad id B".into();
        let mut p = AccountPanel::default();
        p.set_records(vec![a.clone(), b.clone()]);
        // The list is replaced (order swapped) before the click on B arrives.
        p.set_records(vec![b.clone(), a.clone()]);
        p.update(
            Message::Discard(b.client_id.clone()),
            &trading,
            Some(&m),
            false,
        );
        assert!(p.form.as_ref().is_some_and(|f| same(&f.record, &b)));
        p.reset_form();
        // A record that is gone, or one of another account, opens nothing.
        p.set_records(vec![a.clone()]);
        assert!(
            p.update(
                Message::Discard(b.client_id.clone()),
                &trading,
                Some(&m),
                false
            )
            .is_none()
        );
        assert!(p.form.is_none());
        let other = AccountMeta::new(
            "b".into(),
            AccountVenue::Binance,
            exchange::account::AccountEnv::Testnet,
            AccountProduct::UsdM,
            PositionMode::OneWay,
        );
        p.update(
            Message::Discard(a.client_id.clone()),
            &trading,
            Some(&other),
            false,
        );
        assert!(p.form.is_none());
    }

    /// N4: a deleting account opens no form, and a form that is open when the deletion begins
    /// gives no token.
    #[test]
    fn a_deleting_account_opens_no_form_and_an_open_form_gives_no_token() {
        let m = meta();
        let trading = TradingState::new(ProxyState::Direct);
        let mut p = AccountPanel::default();
        p.set_records(vec![invalid_record(&m)]);
        let open = Message::Discard(INVALID_ID.to_string());
        assert!(p.update(open.clone(), &trading, Some(&m), true).is_none());
        assert!(p.form.is_none());
        assert!(
            p.status
                .as_ref()
                .is_some_and(|(s, _)| s.contains("deleted"))
        );
        // The form opens while the account is fine; the deletion begins before the click.
        p.update(open, &trading, Some(&m), false);
        let phrase = p.phrase().unwrap();
        p.update(Message::Typed(phrase), &trading, Some(&m), false);
        assert!(
            p.update(Message::Submit, &trading, Some(&m), true)
                .is_none()
        );
        assert!(p.form.is_none());
    }

    /// N5: the state is judged again on Submit.
    #[test]
    fn submit_judges_the_record_again() {
        use crate::trading::LookupEnd;
        let m = meta();
        let mut r = invalid_record(&m);
        r.client_id = valid_id();
        let mut t = booted_with(&r);
        let mut p = AccountPanel::default();
        p.set_records(vec![r.clone()]);
        // Not yet tested: no release.
        release_form_for(&mut p, &r);
        assert!(p.update(Message::Submit, &t, Some(&m), false).is_none());
        assert!(p.form.is_none() && p.status.is_some());
        // StillUnknown: the form stays. A bulk run then drops it.
        t.begin_lookup(m.id);
        t.lookup_finished(&r, LookupEnd::StillUnknown);
        release_form_for(&mut p, &r);
        p.update(Message::Typed("x".into()), &t, Some(&m), false);
        p.update(Message::Submit, &t, Some(&m), false);
        assert!(p.form.is_some(), "the form of a releasable record stays");
        t.begin_bulk(m.id);
        p.update(Message::Submit, &t, Some(&m), false);
        assert!(p.form.is_none());
        // And no release form opens during a bulk run.
        assert!(
            p.update(Message::Release(r.client_id.clone()), &t, Some(&m), false)
                .is_none()
        );
        assert!(p.form.is_none());
    }

    #[test]
    fn a_valid_record_cannot_be_discarded() {
        let m = meta();
        let trading = TradingState::new(ProxyState::Direct);
        let mut p = AccountPanel::default();
        let mut r = invalid_record(&m);
        r.client_id = valid_id();
        p.set_records(vec![r]);
        assert!(
            p.update(
                Message::Discard(INVALID_ID.to_string()),
                &trading,
                Some(&m),
                false
            )
            .is_none()
        );
        assert!(p.form.is_none());
    }

    fn valid_id() -> String {
        "eAbc123".to_string()
    }

    #[test]
    fn the_messages_print_no_typed_text() {
        let s = format!("{:?}", Message::Typed("discard BTCUSDT ab12c".into()));
        assert!(!s.contains("ab12c") && !s.contains("BTCUSDT"));
    }

    #[test]
    fn close_all_is_hidden_for_spot_on_every_venue() {
        for v in [AccountVenue::Binance, AccountVenue::Okx] {
            assert!(!flatten_offered(v, AccountProduct::Spot));
        }
        assert!(flatten_offered(AccountVenue::Binance, AccountProduct::UsdM));
    }

    #[test]
    fn a_deleting_account_or_an_open_confirmation_blocks_bulk() {
        let g = Gate::default();
        assert!(bulk_blocked_reason(&g, true, false, false).is_some_and(|s| s.contains("deleted")));
        assert!(bulk_blocked_reason(&g, false, true, false).is_some_and(|s| s.contains("running")));
        assert!(
            bulk_blocked_reason(&g, false, false, true).is_some_and(|s| s.contains("confirmation"))
        );
    }
}
