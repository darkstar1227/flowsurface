//! Settings modal for exchange API accounts (Binance, OKX).
//!
//! Secrets never enter `Message` in plain form (they are wrapped in [`Redacted`]), are never
//! pre-filled from the Keychain, and are wiped from the form on save, failure and close.

use crate::{
    style,
    trading::{self, TradingState},
    widget::{BannerKind, banner},
};
use data::config::exchange_accounts::{
    AccountMeta, StoreError, check_key_not_bound, delete_credentials, load_credentials,
    save_credentials,
};
use data::config::risk::{AccountRiskSettings, RiskSettingsError};
use exchange::account::{
    AccountEnv, AccountProduct, AccountVenue, ApiCredentials, CheckReport, PositionMode, Verdict,
    check_account, is_usable,
};
use exchange::signed_http::SignedHttp;
use exchange::trade::stream::StreamCloser;
use std::time::Duration;

use iced::{
    Element, Task, Theme,
    widget::{button, column, container, pick_list, row, text, text_input},
};
use uuid::Uuid;
use zeroize::Zeroize;

pub enum Action {
    /// The account list changed and must be persisted.
    Persist,
    Exit,
    /// A connection test finished. `Some` records its verdict in the send gate's session;
    /// `None` (the test could not run) voids any earlier verdict.
    Tested(Box<AccountMeta>, Option<Box<CheckReport>>),
    /// A delete was confirmed and is running: the session verdict is void from now on, so no
    /// order can be sent for the account while its credentials are being removed (L4).
    Deleting(Uuid),
    /// An account was deleted: its session verdict is void and the list must be persisted.
    Removed(Uuid),
    /// Every account's test could not start (no usable signed client): all verdicts are void.
    Voided(Vec<AccountMeta>),
    /// The account's stream was closed for a delete that then failed at the Keychain: the
    /// stream and the session verdict are gone, the account stays listed and must be tested
    /// again. The caller keeps the account's risk state (N1): it is handled like a failed test.
    DeleteFailed(Box<AccountMeta>),
    /// New risk settings were saved for the account (already validated by `limits()`): the
    /// tracker takes them and the list must be persisted.
    RiskSaved(Uuid, Box<AccountRiskSettings>),
}

/// Looks up the closer of an account's running private stream.
pub type CloserFor<'a> = &'a dyn Fn(Uuid) -> Option<StreamCloser>;

/// Text typed into a secret field. Debug is masked: with the `debug` feature iced prints
/// every `Message` with `{:?}`.
#[derive(Clone, Default)]
pub struct Redacted(String);

impl Drop for Redacted {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Whether switching the sidebar to `new_menu` must wipe the typed credentials.
/// Only an actual menu change away from this modal does; background ticks never do.
pub fn should_clear_on_menu_change(new_menu: Option<&data::sidebar::Menu>) -> bool {
    !matches!(new_menu, Some(data::sidebar::Menu::ExchangeAccounts))
}

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Redacted(***)")
    }
}

type TestOutcome = Result<Box<CheckReport>, String>;
// Boxed: `AccountMeta` grew with the protect defaults and would make this variant the largest.
type SaveOutcome = Result<Box<AccountMeta>, String>;

#[derive(Debug, Clone)]
pub enum Message {
    GoBack,
    AliasChanged(String),
    VenueSelected(AccountVenue),
    EnvSelected(AccountEnv),
    ProductSelected(AccountProduct),
    ModeSelected(PositionMode),
    KeyChanged(Redacted),
    SecretChanged(Redacted),
    PassphraseChanged(Redacted),
    Save,
    Saved(SaveOutcome),
    Test(Uuid),
    TestAll,
    Tested(Uuid, TestOutcome),
    RiskEdit(Uuid),
    RiskInput(Uuid, RiskField, String),
    RiskSave(Uuid),
    RiskCancel(Uuid),
    Delete(Uuid),
    ConfirmDelete(Uuid),
    Deleted(Uuid, Result<(), String>),
    CancelDelete,
}

#[derive(Default)]
struct Form {
    alias: String,
    venue: AccountVenue,
    env: AccountEnv,
    product: AccountProduct,
    mode: PositionMode,
    key: String,
    secret: String,
    passphrase: String,
}

impl std::fmt::Debug for Form {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Form")
            .field("alias", &self.alias)
            .field("venue", &self.venue)
            .field("env", &self.env)
            .field("product", &self.product)
            .field("mode", &self.mode)
            .field("key", &"***")
            .field("secret", &"***")
            .field("passphrase", &"***")
            .finish()
    }
}

impl Form {
    fn clear_secrets(&mut self) {
        self.key.zeroize();
        self.secret.zeroize();
        self.passphrase.zeroize();
    }
}

#[derive(Debug, Clone)]
enum Status {
    NotTested,
    Testing,
    Done(CheckReport),
    Failed(String),
}

#[derive(Debug)]
struct Row {
    meta: AccountMeta,
    status: Status,
    delete_error: Option<String>,
    deleting: bool,
    /// The risk editor, open while the user edits this account's limits.
    risk_form: Option<RiskForm>,
}

/// One text field of the risk editor per saved setting (amounts are decimal strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskField {
    MaxOrder,
    MaxSymbol,
    DailyLoss,
    DrawdownBps,
    ConfirmObservations,
    EquityAgeSecs,
    DayOffsetMinutes,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RiskForm {
    max_order: String,
    max_symbol: String,
    daily_loss: String,
    drawdown_bps: String,
    confirm_observations: String,
    equity_age_secs: String,
    day_offset_minutes: String,
    error: Option<String>,
}

impl RiskForm {
    fn from_settings(r: &AccountRiskSettings) -> Self {
        Self {
            max_order: r.max_order_notional.clone().unwrap_or_default(),
            max_symbol: r.max_symbol_notional.clone().unwrap_or_default(),
            daily_loss: r.daily_loss_limit.clone().unwrap_or_default(),
            drawdown_bps: r.drawdown_bps.map(|b| b.to_string()).unwrap_or_default(),
            confirm_observations: r.confirm_observations.to_string(),
            equity_age_secs: r.max_equity_age_secs.to_string(),
            day_offset_minutes: r.day_utc_offset_minutes.to_string(),
            error: None,
        }
    }

    fn field_mut(&mut self, f: RiskField) -> &mut String {
        match f {
            RiskField::MaxOrder => &mut self.max_order,
            RiskField::MaxSymbol => &mut self.max_symbol,
            RiskField::DailyLoss => &mut self.daily_loss,
            RiskField::DrawdownBps => &mut self.drawdown_bps,
            RiskField::ConfirmObservations => &mut self.confirm_observations,
            RiskField::EquityAgeSecs => &mut self.equity_age_secs,
            RiskField::DayOffsetMinutes => &mut self.day_offset_minutes,
        }
    }

    /// The settings the form describes on top of `base` (which keeps what the form does not
    /// show: the custom drawdown start and the automatic-close switch). An empty amount is "no
    /// limit"; anything that is not a valid value is an error, never a default. The result is
    /// checked with `limits()`, the same check the tracker is built with.
    fn parse(&self, base: &AccountRiskSettings) -> Result<AccountRiskSettings, String> {
        fn amount(v: &str) -> Option<String> {
            let v = v.trim();
            (!v.is_empty()).then(|| v.to_string())
        }
        fn whole<T: std::str::FromStr>(v: &str, what: &str) -> Result<T, String> {
            v.trim()
                .parse()
                .map_err(|_| format!("{what} must be a whole number"))
        }
        let drawdown_bps = match self.drawdown_bps.trim() {
            "" => None,
            v => Some(whole::<u32>(v, "Drawdown (basis points)")?),
        };
        let settings = AccountRiskSettings {
            max_order_notional: amount(&self.max_order),
            max_symbol_notional: amount(&self.max_symbol),
            daily_loss_limit: amount(&self.daily_loss),
            drawdown_bps,
            confirm_observations: whole(&self.confirm_observations, "Confirm readings")?,
            max_equity_age_secs: whole(&self.equity_age_secs, "Equity max age (seconds)")?,
            day_utc_offset_minutes: whole(&self.day_offset_minutes, "Day offset (minutes)")?,
            ..base.clone()
        };
        settings.limits().map_err(describe_risk_error)?;
        Ok(settings)
    }
}

fn describe_risk_error(e: RiskSettingsError) -> String {
    match e {
        RiskSettingsError::BadAmount(name) => {
            format!("{name} must be a positive decimal number (or empty for no limit)")
        }
        RiskSettingsError::BadDrawdown => "Drawdown must be 1 to 10000 basis points".into(),
        RiskSettingsError::BadConfirmCount => "Confirm readings must be 1 to 100".into(),
        RiskSettingsError::BadEquityAge => "Equity max age must be 10 to 3600 seconds".into(),
        RiskSettingsError::BadDayOffset => "Day offset must be within 14 hours of UTC".into(),
    }
}

#[derive(Debug)]
pub struct ExchangeAccounts {
    rows: Vec<Row>,
    form: Form,
    confirm_delete: Option<Uuid>,
    error: Option<String>,
    saving: bool,
}

impl ExchangeAccounts {
    pub fn new(saved: &data::ExchangeAccounts) -> Self {
        Self {
            rows: saved
                .accounts
                .iter()
                .cloned()
                .map(|meta| Row {
                    meta,
                    status: Status::NotTested,
                    delete_error: None,
                    deleting: false,
                    risk_form: None,
                })
                .collect(),
            form: Form::default(),
            confirm_delete: None,
            error: None,
            saving: false,
        }
    }

    /// The account is being deleted: no order, cancel all or close all may start for it.
    pub fn is_deleting(&self, id: uuid::Uuid) -> bool {
        self.rows.iter().any(|r| r.meta.id == id && r.deleting)
    }

    /// Non-secret account list for `saved-state.json`.
    pub fn persisted(&self) -> data::ExchangeAccounts {
        data::ExchangeAccounts {
            accounts: self.rows.iter().map(|r| r.meta.clone()).collect(),
        }
    }

    /// Wipe typed credentials (called whenever the modal is closed).
    pub fn clear_form(&mut self) {
        self.form.clear_secrets();
        self.confirm_delete = None;
    }

    pub fn update(
        &mut self,
        message: Message,
        signed: &Result<SignedHttp, String>,
        closer_for: CloserFor<'_>,
    ) -> (Task<Message>, Option<Action>) {
        match message {
            Message::GoBack => {
                self.clear_form();
                return (Task::none(), Some(Action::Exit));
            }
            Message::AliasChanged(v) => self.form.alias = v,
            Message::VenueSelected(v) => self.form.venue = v,
            Message::EnvSelected(v) => self.form.env = v,
            Message::ProductSelected(v) => self.form.product = v,
            Message::ModeSelected(v) => self.form.mode = v,
            // The previous string (a prefix of the secret) is wiped before it is replaced.
            Message::KeyChanged(mut v) => {
                self.form.key.zeroize();
                self.form.key = std::mem::take(&mut v.0);
            }
            Message::SecretChanged(mut v) => {
                self.form.secret.zeroize();
                self.form.secret = std::mem::take(&mut v.0);
            }
            Message::PassphraseChanged(mut v) => {
                self.form.passphrase.zeroize();
                self.form.passphrase = std::mem::take(&mut v.0);
            }
            Message::Save => {
                if self.saving {
                    return (Task::none(), None);
                }
                let prepared = self.prepare_save();
                // Wiped whether or not saving worked.
                self.form.clear_secrets();
                match prepared {
                    Ok((meta, creds)) => {
                        self.error = None;
                        self.saving = true;
                        let others = self.rows.iter().map(|r| r.meta.clone()).collect();
                        return (save_task(meta, creds, others), None);
                    }
                    Err(e) => self.error = Some(format!("{e}. Credential fields were cleared.")),
                }
            }
            Message::Saved(outcome) => {
                self.saving = false;
                match outcome {
                    Ok(meta) => {
                        self.rows.push(Row {
                            meta: *meta,
                            status: Status::NotTested,
                            delete_error: None,
                            deleting: false,
                            risk_form: None,
                        });
                        self.error = None;
                        return (Task::none(), Some(Action::Persist));
                    }
                    Err(e) => self.error = Some(format!("{e}. Credential fields were cleared.")),
                }
            }
            Message::Test(id) => return self.start_test(id, signed),
            Message::TestAll => return self.start_test_all(signed),
            Message::RiskEdit(id) => {
                if let Some(row) = self.rows.iter_mut().find(|r| r.meta.id == id) {
                    row.risk_form = Some(RiskForm::from_settings(&row.meta.risk));
                }
            }
            Message::RiskInput(id, field, value) => {
                if let Some(form) = self
                    .rows
                    .iter_mut()
                    .find(|r| r.meta.id == id)
                    .and_then(|r| r.risk_form.as_mut())
                {
                    *form.field_mut(field) = value;
                    form.error = None;
                }
            }
            Message::RiskCancel(id) => {
                if let Some(row) = self.rows.iter_mut().find(|r| r.meta.id == id) {
                    row.risk_form = None;
                }
            }
            Message::RiskSave(id) => {
                if let Some(row) = self.rows.iter_mut().find(|r| r.meta.id == id)
                    && let Some(form) = row.risk_form.as_mut()
                {
                    match form.parse(&row.meta.risk) {
                        Ok(settings) => {
                            row.meta.risk = settings.clone();
                            row.risk_form = None;
                            return (
                                Task::none(),
                                Some(Action::RiskSaved(id, Box::new(settings))),
                            );
                        }
                        Err(e) => form.error = Some(e),
                    }
                }
            }
            Message::Tested(id, outcome) => {
                if let Some(row) = self.rows.iter_mut().find(|r| r.meta.id == id) {
                    // A test that finishes while the account is being deleted must not start a
                    // new stream for it (N2): the delete already closed the old one.
                    if row.deleting {
                        // Not stuck in `Testing` if the delete then fails and the row stays.
                        if matches!(row.status, Status::Testing) {
                            row.status = Status::NotTested;
                        }
                        return (Task::none(), None);
                    }
                    let meta = Box::new(row.meta.clone());
                    return match outcome {
                        Ok(report) => {
                            row.status = Status::Done((*report).clone());
                            (Task::none(), Some(Action::Tested(meta, Some(report))))
                        }
                        Err(e) => {
                            row.status = Status::Failed(e);
                            (Task::none(), Some(Action::Tested(meta, None)))
                        }
                    };
                }
            }
            Message::Delete(id) => self.confirm_delete = Some(id),
            Message::CancelDelete => self.confirm_delete = None,
            Message::ConfirmDelete(id) => {
                self.confirm_delete = None;
                if let Some(row) = self.rows.iter_mut().find(|r| r.meta.id == id) {
                    if row.deleting {
                        return (Task::none(), None);
                    }
                    row.deleting = true;
                    row.delete_error = None;
                    return (
                        delete_task(row.meta.clone(), closer_for(id), trading::SHUTDOWN_TIMEOUT),
                        Some(Action::Deleting(id)),
                    );
                }
            }
            Message::Deleted(id, outcome) => {
                if let Some(pos) = self.rows.iter().position(|r| r.meta.id == id) {
                    match outcome {
                        Ok(()) => {
                            self.rows.remove(pos);
                            return (Task::none(), Some(Action::Removed(id)));
                        }
                        Err(e) => {
                            self.rows[pos].deleting = false;
                            self.rows[pos].delete_error = Some(e);
                            // The stream was closed before the Keychain was touched: it is
                            // gone, so the account needs a new test.
                            let meta = Box::new(self.rows[pos].meta.clone());
                            return (Task::none(), Some(Action::DeleteFailed(meta)));
                        }
                    }
                }
            }
        }
        (Task::none(), None)
    }

    fn prepare_save(&self) -> Result<(AccountMeta, ApiCredentials), String> {
        let alias = self.form.alias.trim().to_string();
        let key = self.form.key.trim();
        let secret = self.form.secret.trim();
        let passphrase = self.form.passphrase.trim();

        if alias.is_empty() {
            return Err("Alias is required".into());
        }
        if alias_taken(self.rows.iter().map(|r| r.meta.alias.as_str()), &alias) {
            return Err("An account with this alias already exists".into());
        }
        if key.is_empty() || secret.is_empty() {
            return Err("API key and secret are required".into());
        }
        let is_okx = self.form.venue == AccountVenue::Okx;
        if is_okx && passphrase.is_empty() {
            return Err("OKX requires a passphrase".into());
        }

        let creds = ApiCredentials::new(
            key.to_string(),
            secret.to_string(),
            is_okx.then(|| passphrase.to_string()),
        );
        let meta = AccountMeta::new(
            alias,
            self.form.venue,
            self.form.env,
            self.form.product,
            self.form.mode,
        );
        Ok((meta, creds))
    }

    fn start_test(
        &mut self,
        id: Uuid,
        signed: &Result<SignedHttp, String>,
    ) -> (Task<Message>, Option<Action>) {
        let Some(row) = self.rows.iter_mut().find(|r| r.meta.id == id) else {
            return (Task::none(), None);
        };
        // An account being deleted is not tested (N2): its stream was already closed.
        if matches!(row.status, Status::Testing) || row.deleting {
            return (Task::none(), None);
        }
        let client = match signed {
            Ok(c) => c.clone(),
            Err(e) => {
                // Not a result of the exchange: the status explains why, and the gate's
                // verdict for this account is dropped by the caller.
                row.status = Status::Failed(e.clone());
                let meta = Box::new(row.meta.clone());
                return (Task::none(), Some(Action::Tested(meta, None)));
            }
        };
        row.status = Status::Testing;
        let meta = row.meta.clone();

        let task = Task::perform(
            async move {
                let loader = {
                    let meta = meta.clone();
                    tokio::task::spawn_blocking(move || load_credentials(&meta))
                };
                match loader.await {
                    Ok(Ok(creds)) => Ok(Box::new(
                        check_account(
                            &client,
                            meta.venue,
                            meta.env,
                            meta.product,
                            &creds,
                            meta.position_mode,
                        )
                        .await,
                    )),
                    Ok(Err(e)) => Err(describe_store_error(&e)),
                    Err(_) => Err("Keychain access did not complete".to_string()),
                }
            },
            move |outcome| Message::Tested(id, outcome),
        );
        (task, None)
    }

    /// Test every account that is not being tested. Each one is an ordinary test (its own
    /// Keychain read); when no signed client exists, none can run and all verdicts are void.
    fn start_test_all(
        &mut self,
        signed: &Result<SignedHttp, String>,
    ) -> (Task<Message>, Option<Action>) {
        if let Err(e) = signed {
            let mut voided = Vec::new();
            for row in &mut self.rows {
                if !matches!(row.status, Status::Testing) && !row.deleting {
                    row.status = Status::Failed(e.clone());
                    voided.push(row.meta.clone());
                }
            }
            return (Task::none(), Some(Action::Voided(voided)));
        }
        let ids: Vec<Uuid> = self.rows.iter().map(|r| r.meta.id).collect();
        let tasks: Vec<_> = ids
            .into_iter()
            .map(|id| self.start_test(id, signed).0)
            .collect();
        (Task::batch(tasks), None)
    }

    pub fn view<'a>(&'a self, trading: &'a TradingState) -> Element<'a, Message> {
        let header = row![
            button(style::icon_text(style::Icon::Return, 11)).on_press(Message::GoBack),
            text("Exchange accounts").size(style::text_size::SECTION),
            button(text("Test all")).on_press_maybe(
                (!self.rows.is_empty()
                    && !self
                        .rows
                        .iter()
                        .any(|r| matches!(r.status, Status::Testing)))
                .then_some(Message::TestAll)
            ),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let mut content = column![header].spacing(12);

        if self.rows.is_empty() {
            content = content.push(text("No accounts yet.").size(style::text_size::SMALL));
        }
        for r in &self.rows {
            content = content.push(self.view_row(r, trading));
        }
        content = content.push(self.view_form());

        if let Some(err) = &self.error {
            content = content.push(banner(err, BannerKind::Error));
        }

        container(content)
            .max_width(360)
            .padding(24)
            .style(style::dashboard_modal)
            .into()
    }

    fn view_row<'a>(&'a self, r: &'a Row, trading: &'a TradingState) -> Element<'a, Message> {
        let meta = &r.meta;
        let venue_label = match meta.venue {
            AccountVenue::Binance => format!("{} {}", meta.venue, meta.product),
            AccountVenue::Okx => meta.venue.to_string(),
        };
        let title = row![
            text(&meta.alias),
            text(venue_label).size(style::text_size::SMALL),
            env_tag(meta.env),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let testing = matches!(r.status, Status::Testing);
        let test_btn = button(text("Test"))
            .on_press_maybe((!testing && !r.deleting).then_some(Message::Test(meta.id)));

        let delete_ctrl: Element<'a, Message> = if self.confirm_delete == Some(meta.id) {
            row![
                button(text("Confirm delete"))
                    .style(button::danger)
                    .on_press_maybe((!r.deleting).then_some(Message::ConfirmDelete(meta.id))),
                button(text("Cancel")).on_press(Message::CancelDelete),
            ]
            .spacing(4)
            .into()
        } else {
            button(text("Delete"))
                .on_press_maybe((!r.deleting).then_some(Message::Delete(meta.id)))
                .into()
        };

        let mut col = column![
            title,
            text(format!("Expected position mode: {}", meta.position_mode))
                .size(style::text_size::SMALL),
            row![test_btn, delete_ctrl].spacing(8),
        ]
        .spacing(6);

        match &r.status {
            Status::NotTested => {
                col = col.push(text("Not tested").size(style::text_size::SMALL));
            }
            Status::Testing => {
                col = col.push(text("Testing…").size(style::text_size::SMALL));
            }
            Status::Failed(e) => {
                col = col.push(banner(
                    format!("Cannot use this account: {e}"),
                    BannerKind::Error,
                ));
            }
            Status::Done(report) => {
                col = col.push(verdict_view(meta.env, report));
                for w in &report.warnings {
                    col = col.push(banner(w.as_str(), BannerKind::Warning));
                }
            }
        }
        if meta.venue == AccountVenue::Binance && meta.product == AccountProduct::Spot {
            col = col.push(banner(
                "Spot trading is disabled (not verified on the testnet yet).",
                BannerKind::Warning,
            ));
        }
        if let Some(line) = trading.account_line(meta.id) {
            col = col.push(text(line).size(style::text_size::SMALL));
        }
        col = col.push(self.view_risk(r));
        if let Some(e) = &r.delete_error {
            col = col.push(banner(e.as_str(), BannerKind::Error));
        }

        container(col)
            .padding(8)
            .style(style::modal_container)
            .into()
    }

    /// The risk limits of one account: a summary, and the editor while it is open. Automatic
    /// close is shown, not switchable (it stays off until Live sending is opened).
    fn view_risk<'a>(&'a self, r: &'a Row) -> Element<'a, Message> {
        let id = r.meta.id;
        let mut col = column![].spacing(4);
        let limits = &r.meta.risk;
        if limits.max_order_notional.is_none()
            && limits.max_symbol_notional.is_none()
            && limits.daily_loss_limit.is_none()
            && limits.drawdown_bps.is_none()
        {
            col = col.push(banner("No risk limits set.", BannerKind::Warning));
        }
        col = col.push(
            text(if limits.auto_close {
                "Automatic close: on in the saved settings, but not active in this version"
            } else {
                "Automatic close: off"
            })
            .size(style::text_size::SMALL),
        );
        let Some(form) = &r.risk_form else {
            return col
                .push(button(text("Edit risk limits")).on_press(Message::RiskEdit(id)))
                .into();
        };
        let input = |label: &'static str, value: &'a str, field: RiskField| {
            row![
                text(label).size(style::text_size::SMALL).width(150),
                text_input("", value).on_input(move |s| Message::RiskInput(id, field, s)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
        };
        col = col
            .push(input(
                "Max order value",
                &form.max_order,
                RiskField::MaxOrder,
            ))
            .push(input(
                "Max value per symbol",
                &form.max_symbol,
                RiskField::MaxSymbol,
            ))
            .push(input(
                "Daily loss limit",
                &form.daily_loss,
                RiskField::DailyLoss,
            ))
            .push(input(
                "Drawdown (basis points)",
                &form.drawdown_bps,
                RiskField::DrawdownBps,
            ))
            .push(input(
                "Confirm readings",
                &form.confirm_observations,
                RiskField::ConfirmObservations,
            ))
            .push(input(
                "Equity max age (s)",
                &form.equity_age_secs,
                RiskField::EquityAgeSecs,
            ))
            .push(input(
                "Day offset from UTC (min)",
                &form.day_offset_minutes,
                RiskField::DayOffsetMinutes,
            ))
            .push(
                text("Empty amount = no limit. Limits are in the account's quote currency.")
                    .size(style::text_size::SMALL),
            );
        if let Some(e) = &form.error {
            col = col.push(banner(e.as_str(), BannerKind::Error));
        }
        col.push(
            row![
                button(text("Save limits")).on_press(Message::RiskSave(id)),
                button(text("Cancel")).on_press(Message::RiskCancel(id)),
            ]
            .spacing(8),
        )
        .into()
    }

    fn view_form(&self) -> Element<'_, Message> {
        let f = &self.form;
        let mut col = column![
            text("Add account").size(style::text_size::SECTION),
            text_input("Alias", &f.alias).on_input(Message::AliasChanged),
            row![
                pick_list(AccountVenue::ALL, Some(f.venue), Message::VenueSelected),
                pick_list(AccountEnv::ALL, Some(f.env), Message::EnvSelected),
            ]
            .spacing(8),
            text_input("API key", &f.key)
                .on_input(|s| Message::KeyChanged(Redacted(s)))
                .secure(true),
            text_input("API secret", &f.secret)
                .on_input(|s| Message::SecretChanged(Redacted(s)))
                .secure(true),
        ]
        .spacing(8);

        if f.venue == AccountVenue::Binance {
            col = col.push(
                row![
                    text("Product").size(style::text_size::SMALL),
                    pick_list(
                        AccountProduct::ALL,
                        Some(f.product),
                        Message::ProductSelected
                    ),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            );
        }
        if f.venue == AccountVenue::Okx {
            col = col.push(
                text_input("API passphrase", &f.passphrase)
                    .on_input(|s| Message::PassphraseChanged(Redacted(s)))
                    .secure(true),
            );
        }
        if f.env == AccountEnv::Testnet {
            col = col.push(
                text(match (f.venue, f.product) {
                    (AccountVenue::Binance, AccountProduct::UsdM) => {
                        "Binance futures testnet: permissions cannot be read."
                    }
                    (AccountVenue::Binance, AccountProduct::Spot) => {
                        "Binance spot testnet: permissions cannot be read. \
                         Use a key created on the spot testnet."
                    }
                    (AccountVenue::Okx, _) => "OKX demo trading (x-simulated-trading: 1).",
                })
                .size(style::text_size::SMALL),
            );
        }
        col.push(
            row![
                text("Position mode").size(style::text_size::SMALL),
                pick_list(PositionMode::ALL, Some(f.mode), Message::ModeSelected),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        )
        .push(button(text("Save")).on_press_maybe((!self.saving).then_some(Message::Save)))
        .into()
    }
}

/// Keychain access can block on a system prompt, so it runs off the UI thread.
fn save_task(meta: AccountMeta, creds: ApiCredentials, others: Vec<AccountMeta>) -> Task<Message> {
    Task::perform(
        async move {
            let job = tokio::task::spawn_blocking(move || {
                // One exchange account must not be bound twice: refuse a key another account of
                // the same venue, environment and product already uses (or when that cannot be
                // checked). Only the other account's alias is shown.
                if let Err(e) = check_key_not_bound(&others, &meta, &creds) {
                    log::warn!("Refused to save exchange credentials: duplicate key check failed");
                    return Err(e.to_string());
                }
                // Keychain first: no account is listed unless its credentials were stored
                // and verified.
                match save_credentials(&meta, &creds) {
                    Ok(()) => Ok(Box::new(meta)),
                    Err(e) => {
                        log::warn!("Saving exchange credentials failed: {e}");
                        Err(format!("Could not save to the Keychain: {e}"))
                    }
                }
            });
            job.await
                .unwrap_or_else(|_| Err("Keychain access did not complete".to_string()))
        },
        Message::Saved,
    )
}

/// Close the account's private stream first, then delete the Keychain item (the listenKey
/// DELETE is signed with that item). The wait for the stream is bounded; the delete goes on
/// either way.
fn delete_task(meta: AccountMeta, closer: Option<StreamCloser>, limit: Duration) -> Task<Message> {
    let id = meta.id;
    Task::perform(
        async move {
            let close = closer.map(|c| async move { c.shutdown().await });
            delete_after_close(close, limit, move || {
                delete_credentials(&meta).map_err(|e| {
                    log::warn!("Deleting exchange credentials failed: {e}");
                    format!("Delete failed: {e}")
                })
            })
            .await
        },
        move |outcome| Message::Deleted(id, outcome),
    )
}

async fn delete_after_close<F>(
    close: Option<F>,
    limit: Duration,
    delete: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Result<(), String>
where
    F: std::future::Future<Output = ()>,
{
    if let Some(close) = close
        && !trading::close_within(limit, close).await
    {
        log::warn!("Private stream did not close in time; deleting the credentials anyway");
    }
    tokio::task::spawn_blocking(delete)
        .await
        .unwrap_or_else(|_| Err("Keychain access did not complete".to_string()))
}

fn describe_store_error(e: &StoreError) -> String {
    match e {
        StoreError::NoEntry => {
            "credentials missing from the Keychain; delete and add the account again".into()
        }
        other => other.to_string(),
    }
}

fn env_tag<'a>(env: AccountEnv) -> Element<'a, Message> {
    let label = match env {
        AccountEnv::Live => "LIVE",
        AccountEnv::Testnet => "TESTNET",
    };
    text(label)
        .size(style::text_size::SMALL)
        .style(move |theme: &Theme| {
            let p = theme.extended_palette();
            text::Style {
                color: Some(match env {
                    AccountEnv::Live => p.danger.base.color,
                    AccountEnv::Testnet => p.warning.base.color,
                }),
            }
        })
        .into()
}

fn verdict_view<'a>(env: AccountEnv, report: &'a CheckReport) -> Element<'a, Message> {
    match &report.verdict {
        Verdict::Rejected(why) => banner(
            format!("REJECTED - account not usable: {why}"),
            BannerKind::Error,
        ),
        Verdict::Unverified(why) => {
            let tail = if is_usable(env, &report.verdict) {
                "usable on testnet only"
            } else {
                "not usable"
            };
            banner(format!("Unverified ({tail}): {why}"), BannerKind::Warning)
        }
        Verdict::Usable => text("OK - no withdrawal permission")
            .size(style::text_size::SMALL)
            .into(),
    }
}

/// Whether `alias` is already used: trimmed and compared in lower case, so two accounts cannot
/// be mistaken for each other in a confirmation (I3).
pub(crate) fn alias_taken<'a>(mut existing: impl Iterator<Item = &'a str>, alias: &str) -> bool {
    let want = alias.trim().to_lowercase();
    existing.any(|a| a.trim().to_lowercase() == want)
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_alias_is_taken_without_regard_to_case_or_edge_spaces() {
        let existing = ["Main", "  Ünï  "];
        assert!(super::alias_taken(existing.iter().copied(), "main"));
        assert!(super::alias_taken(existing.iter().copied(), "  MAIN "));
        assert!(super::alias_taken(existing.iter().copied(), "ünï"));
        assert!(!super::alias_taken(existing.iter().copied(), "main2"));
        assert!(!super::alias_taken(std::iter::empty(), "main"));
    }

    use super::*;

    const FAKE_KEY: &str = "fake-key-AAAA1111";
    const FAKE_SECRET: &str = "fake-secret-BBBB2222";
    const FAKE_PASS: &str = "fake-pass-CCCC3333";

    fn no_closer(_: Uuid) -> Option<StreamCloser> {
        None
    }

    #[test]
    fn debug_output_hides_secret_input() {
        let msgs = [
            Message::KeyChanged(Redacted(FAKE_KEY.into())),
            Message::SecretChanged(Redacted(FAKE_SECRET.into())),
            Message::PassphraseChanged(Redacted(FAKE_PASS.into())),
        ];
        let form = Form {
            key: FAKE_KEY.into(),
            secret: FAKE_SECRET.into(),
            passphrase: FAKE_PASS.into(),
            ..Form::default()
        };
        let mut all = format!("{msgs:?} {form:?}");
        let mut modal = ExchangeAccounts::new(&data::ExchangeAccounts::default());
        modal.form = form;
        all.push_str(&format!("{modal:?}"));
        for s in [FAKE_KEY, FAKE_SECRET, FAKE_PASS] {
            assert!(!all.contains(s), "leaked {s}");
        }
    }

    #[test]
    fn save_failure_still_wipes_secret_fields() {
        let mut modal = ExchangeAccounts::new(&data::ExchangeAccounts::default());
        // Missing alias: validation fails before any Keychain access.
        modal.form.key = FAKE_KEY.into();
        modal.form.secret = FAKE_SECRET.into();
        modal.form.passphrase = FAKE_PASS.into();
        let (_, action) = modal.update(Message::Save, &Err("unused".into()), &no_closer);
        assert!(action.is_none());
        assert!(modal.error.is_some());
        assert!(modal.form.key.is_empty());
        assert!(modal.form.secret.is_empty());
        assert!(modal.form.passphrase.is_empty());
        assert!(modal.rows.is_empty());
    }

    #[test]
    fn only_a_real_menu_change_away_clears_the_form() {
        use data::sidebar::Menu;
        // Background ticks produce no action: handled by the caller, never reaching here.
        assert!(!should_clear_on_menu_change(Some(&Menu::ExchangeAccounts)));
        assert!(should_clear_on_menu_change(None));
        assert!(should_clear_on_menu_change(Some(&Menu::Settings)));
    }

    #[test]
    fn key_input_replaces_previous_text() {
        let mut modal = ExchangeAccounts::new(&data::ExchangeAccounts::default());
        let unused = Err("unused".to_string());
        let _ = modal.update(
            Message::SecretChanged(Redacted("ab".into())),
            &unused,
            &no_closer,
        );
        let _ = modal.update(
            Message::SecretChanged(Redacted("abc".into())),
            &unused,
            &no_closer,
        );
        assert_eq!(modal.form.secret, "abc");
    }

    #[test]
    fn closing_wipes_secret_fields() {
        let mut modal = ExchangeAccounts::new(&data::ExchangeAccounts::default());
        modal.form.secret = FAKE_SECRET.into();
        modal.clear_form();
        assert!(modal.form.secret.is_empty());
    }

    fn modal_with_account() -> (ExchangeAccounts, AccountMeta) {
        let meta = AccountMeta::new(
            "main".into(),
            AccountVenue::Binance,
            AccountEnv::Testnet,
            AccountProduct::Spot,
            PositionMode::OneWay,
        );
        let modal = ExchangeAccounts::new(&data::ExchangeAccounts {
            accounts: vec![meta.clone()],
        });
        (modal, meta)
    }

    #[test]
    fn test_results_are_handed_to_the_session_not_kept_in_the_modal() {
        let (mut modal, meta) = modal_with_account();
        let unused = Err("unused".to_string());
        let report = CheckReport {
            verdict: Verdict::Usable,
            warnings: vec![],
            actual_mode: None,
            can_trade: Some(true),
        };
        let (_, action) = modal.update(
            Message::Tested(meta.id, Ok(Box::new(report.clone()))),
            &unused,
            &no_closer,
        );
        match action {
            Some(Action::Tested(m, Some(r))) => {
                assert_eq!(m.id, meta.id);
                assert_eq!(*r, report);
            }
            _ => panic!("expected a recorded verdict"),
        }
        // A failed test voids the verdict.
        let (_, action) = modal.update(
            Message::Tested(meta.id, Err("x".into())),
            &unused,
            &no_closer,
        );
        assert!(matches!(action, Some(Action::Tested(_, None))));
    }

    #[test]
    fn a_test_that_cannot_start_voids_the_verdict() {
        let (mut modal, meta) = modal_with_account();
        // No usable signed client (for example the proxy changed): nothing was tested.
        let (_, action) = modal.update(Message::Test(meta.id), &Err("proxy".into()), &no_closer);
        assert!(matches!(action, Some(Action::Tested(_, None))));
    }

    #[test]
    fn a_confirmed_delete_voids_the_session_verdict_at_once() {
        let (mut modal, meta) = modal_with_account();
        let (_, action) = modal.update(
            Message::ConfirmDelete(meta.id),
            &Err("x".into()),
            &no_closer,
        );
        assert!(matches!(action, Some(Action::Deleting(id)) if id == meta.id));
        assert!(modal.rows[0].deleting);
        // A second confirm while it runs reports nothing new.
        let (_, action) = modal.update(
            Message::ConfirmDelete(meta.id),
            &Err("x".into()),
            &no_closer,
        );
        assert!(action.is_none());
    }

    #[test]
    fn deleting_an_account_reports_its_id_so_the_session_can_drop_it() {
        let (mut modal, meta) = modal_with_account();
        let (_, action) = modal.update(
            Message::Deleted(meta.id, Ok(())),
            &Err("x".into()),
            &no_closer,
        );
        assert!(matches!(action, Some(Action::Removed(id)) if id == meta.id));
        assert!(modal.rows.is_empty());
    }

    #[test]
    fn product_is_kept_for_binance_and_ignored_for_okx() {
        let mut modal = ExchangeAccounts::new(&data::ExchangeAccounts::default());
        modal.form.alias = "a".into();
        modal.form.key = "k".into();
        modal.form.secret = "s".into();
        modal.form.product = AccountProduct::Spot;
        let (meta, _) = modal.prepare_save().unwrap();
        assert_eq!(meta.product, AccountProduct::Spot);

        modal.form.venue = AccountVenue::Okx;
        modal.form.passphrase = "p".into();
        let (meta, _) = modal.prepare_save().unwrap();
        assert_eq!(meta.product, AccountProduct::default());
    }

    #[test]
    fn a_failed_delete_after_the_stream_was_closed_voids_the_account() {
        let (mut modal, meta) = modal_with_account();
        modal.rows[0].deleting = true;
        let (_, action) = modal.update(
            Message::Deleted(meta.id, Err("keychain".into())),
            &Err("x".into()),
            &no_closer,
        );
        assert!(matches!(action, Some(Action::DeleteFailed(m)) if m.id == meta.id));
        // The account stays listed, with the error, and can be deleted again.
        assert_eq!(modal.rows.len(), 1);
        assert!(!modal.rows[0].deleting);
        assert!(modal.rows[0].delete_error.is_some());
    }

    #[test]
    fn an_account_being_deleted_is_not_tested_and_a_late_result_is_ignored() {
        let (mut modal, meta) = modal_with_account();
        modal.rows[0].deleting = true;
        let signed =
            SignedHttp::build(exchange::account::ProxyState::Direct).map_err(|e| e.to_string());
        // Test, and Test all with no usable client, both skip it.
        let (_, action) = modal.update(Message::Test(meta.id), &signed, &no_closer);
        assert!(action.is_none());
        assert!(matches!(modal.rows[0].status, Status::NotTested));
        let (_, action) = modal.update(Message::TestAll, &signed, &no_closer);
        assert!(action.is_none());
        assert!(matches!(modal.rows[0].status, Status::NotTested));
        let (_, action) = modal.update(Message::TestAll, &Err("proxy".into()), &no_closer);
        assert!(matches!(action, Some(Action::Voided(m)) if m.is_empty()));
        // A test that was already running finishes: no stream may start for the account.
        modal.rows[0].status = Status::Testing;
        let report = CheckReport {
            verdict: Verdict::Usable,
            warnings: vec![],
            actual_mode: None,
            can_trade: Some(true),
        };
        let (_, action) = modal.update(
            Message::Tested(meta.id, Ok(Box::new(report))),
            &signed,
            &no_closer,
        );
        assert!(action.is_none());
        // The result was dropped and the delete then fails: the row can be tested again.
        assert!(matches!(modal.rows[0].status, Status::NotTested));
        let _ = modal.update(
            Message::Deleted(meta.id, Err("keychain".into())),
            &signed,
            &no_closer,
        );
        let _ = modal.update(Message::Test(meta.id), &signed, &no_closer);
        assert!(matches!(modal.rows[0].status, Status::Testing));
    }

    #[test]
    fn test_all_with_no_usable_client_voids_every_account() {
        let (mut modal, meta) = modal_with_account();
        let (_, action) = modal.update(Message::TestAll, &Err("proxy".into()), &no_closer);
        match action {
            Some(Action::Voided(metas)) => {
                assert_eq!(metas.len(), 1);
                assert_eq!(metas[0].id, meta.id);
            }
            _ => panic!("expected every verdict to be void"),
        }
        assert!(matches!(modal.rows[0].status, Status::Failed(_)));
    }

    #[test]
    fn test_all_with_a_client_starts_one_test_per_account_and_no_action() {
        let (mut modal, _) = modal_with_account();
        let signed =
            SignedHttp::build(exchange::account::ProxyState::Direct).map_err(|e| e.to_string());
        let (_, action) = modal.update(Message::TestAll, &signed, &no_closer);
        assert!(action.is_none());
        assert!(matches!(modal.rows[0].status, Status::Testing));
    }

    fn edit(modal: &mut ExchangeAccounts, id: Uuid, field: RiskField, value: &str) {
        let _ = modal.update(
            Message::RiskInput(id, field, value.into()),
            &Err("x".into()),
            &no_closer,
        );
    }

    #[test]
    fn valid_risk_limits_are_saved_on_top_of_what_the_form_does_not_show() {
        let (mut modal, meta) = modal_with_account();
        modal.rows[0].meta.risk.auto_close = true;
        modal.rows[0].meta.risk.drawdown_start_ms = Some(5);
        let _ = modal.update(Message::RiskEdit(meta.id), &Err("x".into()), &no_closer);
        edit(&mut modal, meta.id, RiskField::MaxOrder, " 1000 ");
        edit(&mut modal, meta.id, RiskField::DrawdownBps, "500");
        let (_, action) = modal.update(Message::RiskSave(meta.id), &Err("x".into()), &no_closer);
        let Some(Action::RiskSaved(id, settings)) = action else {
            panic!("expected the settings to be saved");
        };
        assert_eq!(id, meta.id);
        assert_eq!(settings.max_order_notional.as_deref(), Some("1000"));
        assert_eq!(settings.drawdown_bps, Some(500));
        assert!(settings.max_symbol_notional.is_none());
        // Not shown in the form, so kept as saved.
        assert!(settings.auto_close);
        assert_eq!(settings.drawdown_start_ms, Some(5));
        assert!(settings.limits().is_ok());
        assert_eq!(modal.rows[0].meta.risk, *settings);
        assert!(modal.rows[0].risk_form.is_none());
    }

    #[test]
    fn invalid_risk_input_shows_an_error_and_saves_nothing() {
        let bad: [(RiskField, &str); 7] = [
            (RiskField::MaxOrder, "-5"),
            (RiskField::MaxSymbol, "abc"),
            (RiskField::DailyLoss, "0"),
            (RiskField::DrawdownBps, "10001"),
            (RiskField::ConfirmObservations, "0"),
            (RiskField::EquityAgeSecs, "5"),
            (RiskField::DayOffsetMinutes, "900"),
        ];
        for (field, value) in bad {
            let (mut modal, meta) = modal_with_account();
            let before = modal.rows[0].meta.risk.clone();
            let _ = modal.update(Message::RiskEdit(meta.id), &Err("x".into()), &no_closer);
            edit(&mut modal, meta.id, field, value);
            let (_, action) =
                modal.update(Message::RiskSave(meta.id), &Err("x".into()), &no_closer);
            assert!(action.is_none(), "{field:?}={value} was accepted");
            let form = modal.rows[0].risk_form.as_ref().expect("editor stays open");
            assert!(form.error.is_some(), "{field:?}={value} gave no error");
            assert_eq!(modal.rows[0].meta.risk, before);
        }
        // Not a number at all.
        let (mut modal, meta) = modal_with_account();
        let _ = modal.update(Message::RiskEdit(meta.id), &Err("x".into()), &no_closer);
        edit(&mut modal, meta.id, RiskField::DrawdownBps, "ten");
        let (_, action) = modal.update(Message::RiskSave(meta.id), &Err("x".into()), &no_closer);
        assert!(action.is_none());
        assert!(modal.rows[0].risk_form.as_ref().unwrap().error.is_some());
    }

    #[test]
    fn the_risk_editor_starts_from_the_saved_values_and_cancel_discards_edits() {
        let (mut modal, meta) = modal_with_account();
        modal.rows[0].meta.risk.daily_loss_limit = Some("250".into());
        let _ = modal.update(Message::RiskEdit(meta.id), &Err("x".into()), &no_closer);
        assert_eq!(modal.rows[0].risk_form.as_ref().unwrap().daily_loss, "250");
        edit(&mut modal, meta.id, RiskField::DailyLoss, "999");
        let _ = modal.update(Message::RiskCancel(meta.id), &Err("x".into()), &no_closer);
        assert!(modal.rows[0].risk_form.is_none());
        assert_eq!(
            modal.rows[0].meta.risk.daily_loss_limit.as_deref(),
            Some("250")
        );
    }

    #[test]
    fn an_account_is_deleted_from_the_keychain_only_after_its_stream_was_closed() {
        use std::sync::{Arc, Mutex};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        // A close that finishes: it is recorded before the delete.
        let o = order.clone();
        let o2 = order.clone();
        let result = rt.block_on(delete_after_close(
            Some(async move { o.lock().unwrap().push("closed") }),
            Duration::from_secs(60),
            move || {
                o2.lock().unwrap().push("deleted");
                Ok(())
            },
        ));
        assert_eq!(result, Ok(()));
        assert_eq!(*order.lock().unwrap(), ["closed", "deleted"]);

        // A close that never finishes: the wait ends and the delete still runs.
        order.lock().unwrap().clear();
        let o3 = order.clone();
        let result = rt.block_on(delete_after_close(
            Some(std::future::pending::<()>()),
            Duration::ZERO,
            move || {
                o3.lock().unwrap().push("deleted");
                Ok(())
            },
        ));
        assert_eq!(result, Ok(()));
        assert_eq!(*order.lock().unwrap(), ["deleted"]);

        // No stream: only the delete, and its error is passed on.
        let result = rt.block_on(delete_after_close(
            None::<std::future::Ready<()>>,
            Duration::ZERO,
            || Err("keychain".to_string()),
        ));
        assert_eq!(result, Err("keychain".to_string()));
    }
}
