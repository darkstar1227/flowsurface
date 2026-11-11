//! The confirmation window of a manual order (M5 c2b-2): shows exactly what will be sent and
//! hands the intent to the send step once, on one click.
//!
//! LIVE orders have a short delay before Confirm works; testnet has none and no LIVE tag. There
//! is no text input, no default focus and no keyboard path here (Escape only cancels, in
//! `main`). The token is made from the stored preview, never from a fresh one.

use crate::{
    order_flow::{Deviation, IntentSlot, Notes, PrepareRequest, Prepared},
    style,
};
use data::config::exchange_accounts::AccountMeta;
use exchange::trade::{ConfirmToken, Exposure, OrderIntent, OrderPreview};
use iced::{
    Element, Length, Theme,
    widget::{button, column, container, row, text},
};
use std::time::{Duration, Instant};

/// How long a LIVE confirmation ignores clicks after it opens.
pub const LIVE_DELAY: Duration = Duration::from_millis(700);

#[derive(Debug, Clone)]
pub enum Message {
    Confirm,
    Cancel,
    /// Only redraws, so the button turns on after the delay.
    DelayElapsed,
}

/// The click rules, without the intent: single use and the LIVE delay. The clock is passed in.
#[derive(Debug, Clone)]
pub struct ConfirmGuard {
    live: bool,
    opened_at: Instant,
    used: bool,
}

impl ConfirmGuard {
    pub fn new(live: bool, now: Instant) -> Self {
        Self {
            live,
            opened_at: now,
            used: false,
        }
    }

    pub fn ready(&self, now: Instant) -> bool {
        !self.used && (!self.live || now.saturating_duration_since(self.opened_at) >= LIVE_DELAY)
    }

    /// `true` once: a second click, or a click inside the LIVE delay, is nothing.
    pub fn try_confirm(&mut self, now: Instant) -> bool {
        if !self.ready(now) {
            return false;
        }
        self.used = true;
        true
    }

    pub fn waiting(&self, now: Instant) -> bool {
        self.live && !self.used && !self.ready(now)
    }
}

/// The stored account a prepared request is for: by the request's own account, never by what the
/// ticket shows now (M1).
pub fn account_of<'a>(
    accounts: &'a [AccountMeta],
    request: &PrepareRequest,
) -> Option<&'a AccountMeta> {
    accounts.iter().find(|m| m.id == request.account)
}

/// Alias, venue, product and environment, as the confirmation names the account.
pub fn account_label(m: &AccountMeta) -> String {
    let env = if crate::order_flow::env_is_live(m.env) {
        "LIVE"
    } else {
        "TESTNET"
    };
    format!("{} · {} {} · {env}", m.alias, m.venue, m.product)
}

/// What the order does to the exposure, in words. `None` for a cancel or a read.
pub fn exposure_line(e: &Exposure) -> Option<String> {
    match e {
        Exposure::Adds { notional: Some(n) } => {
            Some(format!("Estimated value: {n} (quote currency)"))
        }
        Exposure::Adds { notional: None } => Some("Estimated value: unknown".to_string()),
        Exposure::Reduces => Some("Reduces / closes only".to_string()),
        Exposure::Neutral => None,
    }
}

pub struct PendingConfirm {
    pub window: iced::window::Id,
    pub request: PrepareRequest,
    account_label: String,
    live: bool,
    preview: OrderPreview,
    slot: IntentSlot,
    notes: Notes,
    guard: ConfirmGuard,
}

impl PendingConfirm {
    /// `meta` is the account of `prepared.request` (see `account_of`).
    pub fn new(
        window: iced::window::Id,
        meta: &AccountMeta,
        prepared: Prepared,
        now: Instant,
    ) -> Self {
        let live = crate::order_flow::env_is_live(meta.env);
        Self {
            window,
            request: prepared.request,
            account_label: account_label(meta),
            live,
            preview: prepared.preview,
            slot: prepared.slot,
            notes: prepared.notes,
            guard: ConfirmGuard::new(live, now),
        }
    }

    pub fn waiting(&self, now: Instant) -> bool {
        self.guard.waiting(now)
    }

    /// The token and the intent, once. The token is for the preview that was shown.
    pub fn confirm(&mut self, now: Instant) -> Option<(ConfirmToken, OrderIntent)> {
        if !self.guard.try_confirm(now) {
            return None;
        }
        let intent = self.slot.take()?;
        let token = ConfirmToken::for_preview(&self.preview);
        Some((token, intent))
    }

    pub fn view(&self, now: Instant) -> Element<'_, Message> {
        let mut title = row![text("Confirm order").size(style::text_size::SECTION)]
            .spacing(8)
            .align_y(iced::Alignment::Center);
        if self.live {
            title = title.push(live_tag());
        } else {
            title = title.push(text("TESTNET").size(style::text_size::SMALL));
        }

        let mut fields = column![].spacing(4);
        for f in &self.preview.fields {
            fields = fields.push(
                row![
                    text(f.name.clone())
                        .size(style::text_size::SMALL)
                        .width(Length::Fixed(110.0)),
                    text(f.value.clone()).size(style::text_size::SMALL),
                ]
                .spacing(8),
            );
        }

        let mut content = column![
            title,
            text(self.account_label.clone()).size(style::text_size::SMALL),
            text(format!(
                "{:?} {} {}",
                self.preview.method, self.preview.path, self.preview.symbol
            ))
            .size(style::text_size::SMALL),
            fields,
        ]
        .spacing(10);

        if let Some(line) = exposure_line(&self.preview.exposure) {
            content = content.push(text(line).size(style::text_size::SMALL));
        }
        if let Some(base) = &self.notes.base_amount {
            content = content.push(text(base.clone()).size(style::text_size::SMALL));
        }
        match self.notes.deviation {
            Deviation::Exceeds => {
                content = content.push(warn(format!(
                    "The limit price is more than 5% away from the market{}.",
                    self.notes
                        .reference
                        .as_deref()
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default()
                )));
            }
            Deviation::NoReference => {
                content = content.push(
                    text("No market price was available to compare the limit price with.")
                        .size(style::text_size::SMALL),
                );
            }
            Deviation::Within | Deviation::NotApplicable => {}
        }

        let confirm_label = if self.guard.waiting(now) {
            "Confirm (wait…)"
        } else {
            "Confirm"
        };
        let confirm = button(text(confirm_label))
            .style(if self.live {
                button::danger
            } else {
                button::primary
            })
            .on_press_maybe(self.guard.ready(now).then_some(Message::Confirm));
        content = content
            .push(row![confirm, button(text("Cancel")).on_press(Message::Cancel)].spacing(8));

        container(content)
            .max_width(420)
            .padding(24)
            .style(style::dashboard_modal)
            .into()
    }
}

fn live_tag<'a>() -> Element<'a, Message> {
    text("LIVE")
        .size(style::text_size::SMALL)
        .style(|theme: &Theme| text::Style {
            color: Some(theme.extended_palette().danger.base.color),
        })
        .into()
}

fn warn<'a>(s: String) -> Element<'a, Message> {
    text(s)
        .size(style::text_size::SMALL)
        .style(|theme: &Theme| text::Style {
            color: Some(theme.extended_palette().danger.base.color),
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn live_waits_out_the_delay_and_testnet_does_not() {
        let start = t0();
        let mut live = ConfirmGuard::new(true, start);
        assert!(live.waiting(start));
        assert!(!live.try_confirm(start));
        assert!(!live.try_confirm(start + LIVE_DELAY - Duration::from_millis(1)));
        assert!(live.try_confirm(start + LIVE_DELAY));

        let mut test = ConfirmGuard::new(false, start);
        assert!(!test.waiting(start));
        assert!(test.try_confirm(start));
    }

    #[test]
    fn a_confirmation_works_once() {
        let start = t0();
        let mut g = ConfirmGuard::new(false, start);
        assert!(g.try_confirm(start));
        assert!(!g.try_confirm(start));
        assert!(!g.ready(start + Duration::from_secs(10)));
    }

    /// No keyboard path and no text input in the confirmation or the ticket (condition 69).
    #[test]
    fn the_confirmation_and_the_ticket_have_no_keyboard_path() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/modal");
        for name in ["order_confirm.rs", "order_ticket.rs"] {
            let text = std::fs::read_to_string(dir.join(name)).unwrap();
            let code = text.split("#[cfg(test)]").next().unwrap_or_default();
            for banned in ["on_submit", "keyboard::", "text_input("] {
                if name == "order_ticket.rs" && banned == "text_input(" {
                    continue; // the ticket has the number fields, but never submits by key
                }
                assert!(!code.contains(banned), "{name} contains {banned}");
            }
        }
    }

    /// The token is made from the stored preview, here only (condition 70).
    #[test]
    fn the_token_comes_from_the_stored_preview_in_this_file_only() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![src];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().is_none_or(|x| x != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&p).unwrap();
                let code = text.split("#[cfg(test)]").next().unwrap_or_default();
                for line in code.lines().filter(|l| l.contains("for_preview(")) {
                    assert!(
                        p.ends_with("order_confirm.rs") && line.contains("&self.preview)"),
                        "{}: {line}",
                        p.display()
                    );
                }
            }
        }
    }

    fn meta(alias: &str, env: exchange::account::AccountEnv) -> AccountMeta {
        AccountMeta::new(
            alias.into(),
            exchange::account::AccountVenue::Binance,
            env,
            exchange::account::AccountProduct::UsdM,
            exchange::account::PositionMode::OneWay,
        )
    }

    /// The confirmation names the account of the request, not the one the ticket shows now (M1).
    #[test]
    fn the_label_comes_from_the_requests_account() {
        use exchange::account::AccountEnv;
        use exchange::trade::ManualSpec;
        let a = meta("alpha", AccountEnv::Testnet);
        let b = meta("bravo", AccountEnv::Live);
        let accounts = vec![b.clone(), a.clone()];
        let request = PrepareRequest {
            account: a.id,
            symbol: "BTCUSDT".into(),
            spec: ManualSpec::Cancel {
                id: "1".into(),
                kind: exchange::trade::OpenKind::Plain,
            },
        };
        let found = account_of(&accounts, &request).unwrap();
        assert_eq!(found.id, a.id);
        let label = account_label(found);
        assert!(
            label.contains("alpha") && label.contains("TESTNET"),
            "{label}"
        );
        assert!(
            !label.contains("bravo") && !label.contains("LIVE"),
            "{label}"
        );
        assert!(account_label(&b).contains("LIVE"));
        // An account that is gone gives no confirmation at all.
        let gone = PrepareRequest {
            account: uuid::Uuid::from_u128(9),
            ..request
        };
        assert!(account_of(&accounts, &gone).is_none());
    }

    #[test]
    fn the_exposure_is_said_in_words() {
        use exchange::trade::Decimal;
        let n = Decimal::parse("12.5").unwrap();
        assert!(
            exposure_line(&Exposure::Adds { notional: Some(n) })
                .unwrap()
                .contains("12.5")
        );
        assert!(
            exposure_line(&Exposure::Adds { notional: None })
                .unwrap()
                .contains("unknown")
        );
        assert!(
            exposure_line(&Exposure::Reduces)
                .unwrap()
                .contains("Reduces")
        );
        assert!(exposure_line(&Exposure::Neutral).is_none());
    }
}
