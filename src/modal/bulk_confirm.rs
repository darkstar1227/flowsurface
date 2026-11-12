//! The confirmation window of a cancel-all, a close-all or the lifting of a risk lock (M5
//! c2b-3). It shows what will happen and hands out the one token on one click.
//!
//! Same rules as `order_confirm`: LIVE has the delay and the tag, testnet neither; no text input,
//! no default focus and no keyboard path (Escape only cancels, in `main`). The bulk token is made
//! here and only here, from the account and action stored when the window was opened.

use crate::{
    modal::order_confirm::{ConfirmGuard, account_label},
    order_flow::env_is_live,
    style,
};
use data::config::exchange_accounts::AccountMeta;
use exchange::account::AccountKey;
use exchange::trade::{BulkAction, ConfirmToken};
use iced::{
    Element, Length, Theme,
    widget::{button, column, container, row, text},
};
use std::time::Instant;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum Message {
    Confirm,
    Cancel,
}

/// What the window is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bulk(BulkAction),
    /// Lifting a risk lock: a local change, no request.
    AckTrip,
}

/// What one click gives. The token is for the account and action the window was opened for.
pub enum Confirmed {
    Bulk(ConfirmToken),
    AckTrip,
}

pub struct PendingBulk {
    pub window: iced::window::Id,
    /// The account the window was opened for. Never taken from the ticket again.
    pub account: Uuid,
    account_key: AccountKey,
    kind: Kind,
    label: String,
    live: bool,
    lines: Vec<String>,
    /// `false`: there is nothing to do, and Confirm cannot be pressed.
    can_confirm: bool,
    guard: ConfirmGuard,
}

impl PendingBulk {
    pub fn new(
        window: iced::window::Id,
        kind: Kind,
        meta: &AccountMeta,
        saved: &[AccountMeta],
        lines: Vec<String>,
        can_confirm: bool,
        now: Instant,
    ) -> Self {
        let live = env_is_live(meta.env);
        Self {
            window,
            account: meta.id,
            account_key: crate::trading::account_key(meta.id),
            kind,
            label: account_label(meta, saved),
            live,
            lines,
            can_confirm,
            guard: ConfirmGuard::new(live, crate::order_flow::OrderSource::Ticket, now),
        }
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn waiting(&self, now: Instant) -> bool {
        self.guard.waiting(now)
    }

    /// The token (or the acknowledgement), once. A bulk token is made here from the stored
    /// account and action, never from anything read later.
    pub fn confirm(&mut self, now: Instant) -> Option<Confirmed> {
        if !self.can_confirm || !self.guard.try_confirm(now) {
            return None;
        }
        Some(match self.kind {
            Kind::Bulk(action) => Confirmed::Bulk(ConfirmToken::for_bulk(self.account_key, action)),
            Kind::AckTrip => Confirmed::AckTrip,
        })
    }

    fn title(&self) -> &'static str {
        match self.kind {
            Kind::Bulk(BulkAction::CancelAll) => "Confirm: cancel all orders",
            Kind::Bulk(BulkAction::Flatten) => "Confirm: close all positions",
            Kind::AckTrip => "Confirm: lift the risk lock",
        }
    }

    pub fn view(&self, now: Instant) -> Element<'_, Message> {
        let mut title = row![text(self.title()).size(style::text_size::SECTION)]
            .spacing(8)
            .align_y(iced::Alignment::Center);
        if self.live {
            title = title.push(text("LIVE").size(style::text_size::SMALL).style(
                |theme: &Theme| text::Style {
                    color: Some(theme.extended_palette().danger.base.color),
                },
            ));
        } else {
            title = title.push(text("TESTNET").size(style::text_size::SMALL));
        }
        let mut body = column![].spacing(4);
        for line in &self.lines {
            body = body.push(text(line.clone()).size(style::text_size::SMALL));
        }
        let ready = self.can_confirm && self.guard.ready(now);
        let label = if self.can_confirm && self.guard.waiting(now) {
            "Confirm (wait…)"
        } else {
            "Confirm"
        };
        let confirm = button(text(label))
            .style(if self.live {
                button::danger
            } else {
                button::primary
            })
            .on_press_maybe(ready.then_some(Message::Confirm));
        container(
            column![
                title,
                text(self.label.clone()).size(style::text_size::SMALL),
                body,
                row![confirm, button(text("Cancel")).on_press(Message::Cancel)].spacing(8),
            ]
            .spacing(10),
        )
        .max_width(420)
        .padding(24)
        .width(Length::Shrink)
        .style(style::dashboard_modal)
        .into()
    }
}

/// The confirmation click of `main`: the token comes out of the open window once, and the
/// window is closed with it, so a second click (a double click, a repeated key) finds no window
/// and makes no second request. A click that is refused (a delay, nothing to do) keeps it open.
pub fn take_confirmed(slot: &mut Option<PendingBulk>, now: Instant) -> Option<Confirmed> {
    let confirmed = slot.as_mut()?.confirm(now)?;
    *slot = None;
    Some(confirmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::account::{AccountEnv, AccountProduct, AccountVenue, PositionMode};
    use std::time::Duration;

    fn meta(alias: &str, env: AccountEnv) -> AccountMeta {
        AccountMeta::new(
            alias.into(),
            AccountVenue::Binance,
            env,
            AccountProduct::UsdM,
            PositionMode::OneWay,
        )
    }

    fn pending(kind: Kind, m: &AccountMeta, can: bool, now: Instant) -> PendingBulk {
        PendingBulk::new(
            iced::window::Id::unique(),
            kind,
            m,
            std::slice::from_ref(m),
            vec!["x".into()],
            can,
            now,
        )
    }

    /// The token is for the account the window was opened for, and the action it shows.
    #[test]
    fn the_token_is_for_the_stored_account_and_action() {
        let now = Instant::now();
        let m = meta("alpha", AccountEnv::Testnet);
        let mut p = pending(Kind::Bulk(BulkAction::Flatten), &m, true, now);
        assert_eq!(p.account, m.id);
        match p.confirm(now) {
            Some(Confirmed::Bulk(token)) => {
                // The same token again is made only for the same account and action.
                let same = format!("{token:?}");
                let want = format!(
                    "{:?}",
                    ConfirmToken::for_bulk(crate::trading::account_key(m.id), BulkAction::Flatten)
                );
                assert_eq!(same, want);
            }
            _ => panic!("a bulk token was expected"),
        }
        // Single use.
        assert!(p.confirm(now).is_none());
    }

    #[test]
    fn an_empty_action_cannot_be_confirmed() {
        let now = Instant::now();
        let m = meta("alpha", AccountEnv::Testnet);
        let mut p = pending(Kind::Bulk(BulkAction::CancelAll), &m, false, now);
        assert!(p.confirm(now).is_none());
    }

    #[test]
    fn live_waits_and_acknowledging_a_lock_is_not_a_token() {
        let now = Instant::now();
        let live = meta("alpha", AccountEnv::Live);
        let mut p = pending(Kind::AckTrip, &live, true, now);
        assert!(p.waiting(now));
        assert!(p.confirm(now).is_none());
        assert!(matches!(
            p.confirm(now + crate::modal::order_confirm::LIVE_DELAY + Duration::from_millis(1)),
            Some(Confirmed::AckTrip)
        ));
    }

    /// Two Confirm clicks on one window give one token, and with it one request.
    #[test]
    fn two_confirm_clicks_give_one_token_and_close_the_window() {
        let now = Instant::now();
        let m = meta("alpha", AccountEnv::Testnet);
        let mut slot = Some(pending(Kind::Bulk(BulkAction::CancelAll), &m, true, now));
        let got = [
            take_confirmed(&mut slot, now),
            take_confirmed(&mut slot, now),
        ];
        assert_eq!(got.iter().filter(|c| c.is_some()).count(), 1);
        assert!(slot.is_none());
        // A refused click (the Live delay) keeps the window and gives nothing.
        let live = meta("beta", AccountEnv::Live);
        let mut slot = Some(pending(Kind::Bulk(BulkAction::Flatten), &live, true, now));
        assert!(take_confirmed(&mut slot, now).is_none());
        assert!(slot.is_some());
    }
}
