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
use data::config::protect::{
    ProtectDefaults, ProtectOffset, TakeProfitLeg, TrailingDefault, TriggerPriceType,
};
use data::config::risk::{AccountRiskSettings, RiskSettingsError};
use exchange::account::{
    AccountEnv, AccountProduct, AccountVenue, ApiCredentials, CheckReport, PositionMode, Verdict,
    check_account, is_usable,
};
use exchange::signed_http::SignedHttp;
use exchange::trade::MAX_TARGETS;
use exchange::trade::stream::StreamCloser;
use std::time::Duration;

use iced::{
    Element, Task, Theme,
    widget::{button, checkbox, column, container, pick_list, row, text, text_input},
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
    /// New protective order defaults were saved for the account (already checked by
    /// `ProtectDefaults::spec`): the running copy takes them and the list must be persisted.
    ProtectSaved(Uuid, Box<ProtectDefaults>),
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
    ProtectEdit(Uuid),
    ProtectInput(Uuid, ProtectField),
    ProtectSave(Uuid),
    ProtectCancel(Uuid),
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
    /// The protective defaults editor, open while the user edits them.
    protect_form: Option<ProtectForm>,
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

/// How an offset field is read: not set, a percent of the fill price, or a price distance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OffsetKind {
    #[default]
    None,
    Percent,
    Distance,
}

impl OffsetKind {
    const OPTIONAL: [OffsetKind; 3] = [OffsetKind::None, OffsetKind::Percent, OffsetKind::Distance];
    const SET: [OffsetKind; 2] = [OffsetKind::Percent, OffsetKind::Distance];

    fn of(o: &ProtectOffset) -> (Self, String) {
        match o {
            ProtectOffset::Percent(v) => (OffsetKind::Percent, v.clone()),
            ProtectOffset::Distance(v) => (OffsetKind::Distance, v.clone()),
        }
    }

    /// `None` when not set. The value is kept as typed (trimmed); the check is `spec`.
    fn offset(self, value: &str) -> Option<ProtectOffset> {
        let v = value.trim().to_string();
        match self {
            OffsetKind::None => None,
            OffsetKind::Percent => Some(ProtectOffset::Percent(v)),
            OffsetKind::Distance => Some(ProtectOffset::Distance(v)),
        }
    }
}

impl std::fmt::Display for OffsetKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OffsetKind::None => "None",
            OffsetKind::Percent => "Percent",
            OffsetKind::Distance => "Price distance",
        })
    }
}

/// One edit of the protective defaults editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtectField {
    StopKind(OffsetKind),
    StopValue(String),
    AddTarget,
    RemoveTarget(usize),
    TargetKind(usize, OffsetKind),
    TargetValue(usize, String),
    TargetShare(usize, String),
    Trailing(bool),
    Callback(String),
    ActivationKind(OffsetKind),
    ActivationValue(String),
    MarkPrice(bool),
    AutoAttach(bool),
    AutoAttachLive(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TargetForm {
    kind: OffsetKind,
    value: String,
    share: String,
}

/// The editor of one account's protective order defaults. Numbers stay text until saved; the
/// only check is `ProtectDefaults::spec`, the one the order ticket uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ProtectForm {
    stop_kind: OffsetKind,
    stop_value: String,
    targets: Vec<TargetForm>,
    trailing: bool,
    callback: String,
    activation_kind: OffsetKind,
    activation_value: String,
    mark_price: bool,
    auto_attach: bool,
    auto_attach_live: bool,
    error: Option<String>,
}

impl ProtectForm {
    fn from_defaults(d: &ProtectDefaults) -> Self {
        let (stop_kind, stop_value) = d.stop_loss.as_ref().map(OffsetKind::of).unwrap_or_default();
        let (trailing, callback, activation_kind, activation_value) = match &d.trailing {
            Some(t) => {
                let (k, v) = t
                    .activation
                    .as_ref()
                    .map(OffsetKind::of)
                    .unwrap_or_default();
                (true, t.callback_pct.clone(), k, v)
            }
            None => (false, String::new(), OffsetKind::None, String::new()),
        };
        Self {
            stop_kind,
            stop_value,
            targets: d
                .take_profits
                .iter()
                .map(|t| {
                    let (kind, value) = OffsetKind::of(&t.offset);
                    TargetForm {
                        kind,
                        value,
                        share: t.share_pct.to_string(),
                    }
                })
                .collect(),
            trailing,
            callback,
            activation_kind,
            activation_value,
            mark_price: d.trigger_price == TriggerPriceType::Mark,
            auto_attach: d.auto_attach,
            auto_attach_live: d.auto_attach_live,
            error: None,
        }
    }

    /// `live`: the account is a Live one. The second switch is ignored on any other account.
    fn apply(&mut self, field: ProtectField, live: bool) {
        self.error = None;
        match field {
            ProtectField::StopKind(k) => self.stop_kind = k,
            ProtectField::StopValue(v) => self.stop_value = v,
            ProtectField::AddTarget => {
                if self.targets.len() < MAX_TARGETS {
                    self.targets.push(TargetForm {
                        kind: OffsetKind::Percent,
                        value: String::new(),
                        share: String::new(),
                    });
                }
            }
            ProtectField::RemoveTarget(i) => {
                if i < self.targets.len() {
                    self.targets.remove(i);
                }
            }
            ProtectField::TargetKind(i, k) => {
                if let Some(t) = self.targets.get_mut(i) {
                    t.kind = k;
                }
            }
            ProtectField::TargetValue(i, v) => {
                if let Some(t) = self.targets.get_mut(i) {
                    t.value = v;
                }
            }
            ProtectField::TargetShare(i, v) => {
                if let Some(t) = self.targets.get_mut(i) {
                    t.share = v;
                }
            }
            ProtectField::Trailing(on) => self.trailing = on,
            ProtectField::Callback(v) => self.callback = v,
            ProtectField::ActivationKind(k) => self.activation_kind = k,
            ProtectField::ActivationValue(v) => self.activation_value = v,
            ProtectField::MarkPrice(on) => self.mark_price = on,
            ProtectField::AutoAttach(on) => self.auto_attach = on,
            ProtectField::AutoAttachLive(on) => {
                if live {
                    self.auto_attach_live = on;
                }
            }
        }
    }

    /// The defaults the form describes, on top of the account's saved ones (which keep the
    /// second switch of a non-Live account). Checked by `ProtectDefaults::spec` against the
    /// account's expected position mode (the real mode is known only after a test; the order
    /// is refused again at preparation if it differs). Anything not valid is an error.
    fn parse(&self, meta: &AccountMeta) -> Result<ProtectDefaults, String> {
        let mut take_profits = Vec::with_capacity(self.targets.len());
        for t in &self.targets {
            let Some(offset) = t.kind.offset(&t.value) else {
                return Err("Every take-profit level needs percent or price distance.".into());
            };
            let share_pct =
                t.share.trim().parse::<u8>().map_err(|_| {
                    "A take-profit share must be a whole number, 1 to 100.".to_string()
                })?;
            take_profits.push(TakeProfitLeg { offset, share_pct });
        }
        let trailing = self.trailing.then(|| TrailingDefault {
            callback_pct: self.callback.trim().to_string(),
            activation: self.activation_kind.offset(&self.activation_value),
        });
        let defaults = ProtectDefaults {
            stop_loss: self.stop_kind.offset(&self.stop_value),
            take_profits,
            trailing,
            trigger_price: if self.mark_price {
                TriggerPriceType::Mark
            } else {
                TriggerPriceType::Last
            },
            auto_attach: self.auto_attach,
            auto_attach_live: if meta.env == AccountEnv::Live {
                self.auto_attach_live
            } else {
                meta.protect.auto_attach_live
            },
        };
        let mode = match meta.product {
            AccountProduct::Spot => PositionMode::OneWay,
            AccountProduct::UsdM => meta.position_mode,
        };
        defaults
            .spec(meta.venue, mode)
            .map_err(|e| format!("Cannot save: {e}."))?;
        Ok(defaults)
    }
}

/// One line saying what an account's saved defaults do.
fn protect_summary(meta: &AccountMeta) -> String {
    let p = &meta.protect;
    let offset = |o: &ProtectOffset| match o {
        ProtectOffset::Percent(v) => format!("{v}%"),
        ProtectOffset::Distance(v) => format!("{v} price"),
    };
    let mut parts = Vec::new();
    if let Some(s) = &p.stop_loss {
        parts.push(format!("stop {}", offset(s)));
    }
    if !p.take_profits.is_empty() {
        parts.push(format!("{} take-profit level(s)", p.take_profits.len()));
    }
    if let Some(t) = &p.trailing {
        parts.push(format!("trailing {}%", t.callback_pct));
    }
    if parts.is_empty() {
        return "Protective defaults: none".into();
    }
    let auto = if p.attaches_automatically(meta.env) {
        "automatic attach on"
    } else if p.auto_attach {
        "automatic attach waits for the LIVE switch"
    } else {
        "automatic attach off"
    };
    format!("Protective defaults: {} · {auto}", parts.join(", "))
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
                    protect_form: None,
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
                            protect_form: None,
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
                // An account that is being deleted takes no new settings.
                if let Some(row) = self
                    .rows
                    .iter_mut()
                    .find(|r| r.meta.id == id && !r.deleting)
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
            Message::ProtectEdit(id) => {
                if let Some(row) = self.rows.iter_mut().find(|r| r.meta.id == id) {
                    row.protect_form = Some(ProtectForm::from_defaults(&row.meta.protect));
                }
            }
            Message::ProtectInput(id, field) => {
                if let Some(row) = self.rows.iter_mut().find(|r| r.meta.id == id) {
                    let live = row.meta.env == AccountEnv::Live;
                    if let Some(form) = row.protect_form.as_mut() {
                        form.apply(field, live);
                    }
                }
            }
            Message::ProtectCancel(id) => {
                if let Some(row) = self.rows.iter_mut().find(|r| r.meta.id == id) {
                    row.protect_form = None;
                }
            }
            Message::ProtectSave(id) => {
                // An account that is being deleted takes no new settings.
                if let Some(row) = self
                    .rows
                    .iter_mut()
                    .find(|r| r.meta.id == id && !r.deleting)
                    && let Some(form) = row.protect_form.as_mut()
                {
                    match form.parse(&row.meta) {
                        Ok(defaults) => {
                            row.meta.protect = defaults.clone();
                            row.protect_form = None;
                            return (
                                Task::none(),
                                Some(Action::ProtectSaved(id, Box::new(defaults))),
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
        col = col.push(self.view_protect(r));
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

    /// The protective order defaults of one account: a summary, and the editor while it is
    /// open. Saving changes what the order ticket applies; nothing is sent from here.
    fn view_protect<'a>(&'a self, r: &'a Row) -> Element<'a, Message> {
        let id = r.meta.id;
        let mut col =
            column![text(protect_summary(&r.meta)).size(style::text_size::SMALL)].spacing(4);
        let Some(form) = &r.protect_form else {
            return col
                .push(button(text("Edit protective defaults")).on_press(Message::ProtectEdit(id)))
                .into();
        };
        let small = style::text_size::SMALL;
        let field = move |f: ProtectField| Message::ProtectInput(id, f);
        let labelled = |label: &'static str, input: Element<'a, Message>| {
            row![text(label).size(small).width(150), input]
                .spacing(8)
                .align_y(iced::Alignment::Center)
        };

        col = col.push(labelled(
            "Stop-loss",
            row![
                pick_list(OffsetKind::OPTIONAL, Some(form.stop_kind), move |k| field(
                    ProtectField::StopKind(k)
                )),
                text_input("", &form.stop_value)
                    .on_input(move |s| field(ProtectField::StopValue(s))),
            ]
            .spacing(8)
            .into(),
        ));
        for (i, t) in form.targets.iter().enumerate() {
            col = col.push(labelled(
                "Take-profit",
                row![
                    pick_list(OffsetKind::SET, Some(t.kind), move |k| field(
                        ProtectField::TargetKind(i, k)
                    )),
                    text_input("", &t.value)
                        .on_input(move |s| field(ProtectField::TargetValue(i, s))),
                    text_input("share %", &t.share)
                        .on_input(move |s| field(ProtectField::TargetShare(i, s)))
                        .width(80),
                    button(text("Remove").size(small))
                        .on_press(field(ProtectField::RemoveTarget(i))),
                ]
                .spacing(8)
                .into(),
            ));
        }
        if form.targets.len() < MAX_TARGETS {
            col = col.push(
                button(text("Add take-profit level").size(small))
                    .on_press(field(ProtectField::AddTarget)),
            );
        }
        col = col.push(
            checkbox(form.trailing)
                .label("Trailing stop")
                .on_toggle(move |on| field(ProtectField::Trailing(on))),
        );
        if form.trailing {
            col = col
                .push(labelled(
                    "Callback rate %",
                    text_input("", &form.callback)
                        .on_input(move |s| field(ProtectField::Callback(s)))
                        .into(),
                ))
                .push(labelled(
                    "Trail starts at",
                    row![
                        pick_list(OffsetKind::OPTIONAL, Some(form.activation_kind), move |k| {
                            field(ProtectField::ActivationKind(k))
                        }),
                        text_input("", &form.activation_value)
                            .on_input(move |s| field(ProtectField::ActivationValue(s))),
                    ]
                    .spacing(8)
                    .into(),
                ));
        }
        col = col
            .push(
                checkbox(form.mark_price)
                    .label("Trigger on mark price (otherwise last price)")
                    .on_toggle(move |on| field(ProtectField::MarkPrice(on))),
            )
            .push(
                checkbox(form.auto_attach)
                    .label("Attach automatically with entries")
                    .on_toggle(move |on| field(ProtectField::AutoAttach(on))),
            );
        if r.meta.env == AccountEnv::Live {
            col = col
                .push(
                    checkbox(form.auto_attach_live)
                        .label("Also attach automatically on this LIVE account")
                        .on_toggle(move |on| field(ProtectField::AutoAttachLive(on))),
                )
                .push(text("Live trading is not open yet.").size(small));
        }
        let note = if r.meta.venue == AccountVenue::Okx {
            "OKX takes one take-profit level and no trailing stop with an entry."
        } else if r.meta.position_mode == PositionMode::Hedge {
            "A hedge account takes one take-profit level and no trailing stop."
        } else {
            "Offsets are measured from the fill price. Shares add up to 100. Up to 3 levels."
        };
        col = col.push(text(note).size(small));
        if let Some(e) = &form.error {
            col = col.push(banner(e.as_str(), BannerKind::Error));
        }
        col.push(
            row![
                button(text("Save defaults")).on_press(Message::ProtectSave(id)),
                button(text("Cancel")).on_press(Message::ProtectCancel(id)),
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
    fn an_account_being_deleted_takes_no_risk_or_protect_save() {
        let (mut modal, meta) = modal_with_account();
        let unused = Err("x".to_string());
        let _ = modal.update(Message::RiskEdit(meta.id), &unused, &no_closer);
        let _ = modal.update(Message::ProtectEdit(meta.id), &unused, &no_closer);
        modal.rows[0].deleting = true;
        let before = modal.rows[0].meta.clone();
        for msg in [Message::RiskSave(meta.id), Message::ProtectSave(meta.id)] {
            let (_, action) = modal.update(msg, &unused, &no_closer);
            assert!(action.is_none());
        }
        assert_eq!(modal.rows[0].meta, before);
        assert!(modal.rows[0].risk_form.is_some());
        assert!(modal.rows[0].protect_form.is_some());
        // Not deleting: the same save goes through.
        modal.rows[0].deleting = false;
        let (_, action) = modal.update(Message::RiskSave(meta.id), &unused, &no_closer);
        assert!(matches!(action, Some(Action::RiskSaved(..))));
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

    fn account(env: AccountEnv, venue: AccountVenue, mode: PositionMode) -> AccountMeta {
        AccountMeta::new("p".into(), venue, env, AccountProduct::UsdM, mode)
    }

    fn modal_of(meta: &AccountMeta) -> ExchangeAccounts {
        ExchangeAccounts::new(&data::ExchangeAccounts {
            accounts: vec![meta.clone()],
        })
    }

    fn protect(modal: &mut ExchangeAccounts, id: Uuid, f: ProtectField) {
        let _ = modal.update(Message::ProtectInput(id, f), &Err("x".into()), &no_closer);
    }

    fn pct(v: &str) -> ProtectOffset {
        ProtectOffset::Percent(v.into())
    }

    fn rich() -> ProtectDefaults {
        ProtectDefaults {
            stop_loss: Some(ProtectOffset::Distance("150.5".into())),
            take_profits: vec![
                TakeProfitLeg {
                    offset: pct("1"),
                    share_pct: 50,
                },
                TakeProfitLeg {
                    offset: ProtectOffset::Distance("300".into()),
                    share_pct: 30,
                },
                TakeProfitLeg {
                    offset: pct("5"),
                    share_pct: 20,
                },
            ],
            trailing: Some(TrailingDefault {
                callback_pct: "1.5".into(),
                activation: Some(pct("2")),
            }),
            trigger_price: TriggerPriceType::Mark,
            auto_attach: true,
            auto_attach_live: false,
        }
    }

    #[test]
    fn the_protect_form_round_trips_what_it_shows() {
        let meta = account(
            AccountEnv::Testnet,
            AccountVenue::Binance,
            PositionMode::OneWay,
        );
        for d in [rich(), ProtectDefaults::default()] {
            let form = ProtectForm::from_defaults(&d);
            assert_eq!(form.parse(&meta), Ok(d));
        }
        // A trailing stop that starts at once keeps no start condition.
        let d = ProtectDefaults {
            trailing: Some(TrailingDefault {
                callback_pct: "2".into(),
                activation: None,
            }),
            ..Default::default()
        };
        assert_eq!(ProtectForm::from_defaults(&d).parse(&meta), Ok(d));
    }

    #[test]
    fn a_saved_protect_form_is_kept_and_handed_on_and_the_switch_follows_the_env() {
        for (env, live_switch) in [(AccountEnv::Testnet, false), (AccountEnv::Live, false)] {
            let meta = account(env, AccountVenue::Binance, PositionMode::OneWay);
            let mut modal = modal_of(&meta);
            let id = meta.id;
            let _ = modal.update(Message::ProtectEdit(id), &Err("x".into()), &no_closer);
            protect(&mut modal, id, ProtectField::StopKind(OffsetKind::Percent));
            protect(&mut modal, id, ProtectField::StopValue(" 1.5 ".into()));
            protect(&mut modal, id, ProtectField::AutoAttach(true));
            let (_, action) = modal.update(Message::ProtectSave(id), &Err("x".into()), &no_closer);
            let Some(Action::ProtectSaved(got, d)) = action else {
                panic!("expected a saved action");
            };
            assert_eq!(got, id);
            assert_eq!(d.stop_loss, Some(pct("1.5")));
            assert!(modal.rows[0].protect_form.is_none());
            assert_eq!(modal.persisted().accounts[0].protect, *d);
            // The saved copy is what the ticket reads: automatic only if the env allows.
            assert_eq!(
                modal.persisted().accounts[0]
                    .protect
                    .attaches_automatically(env),
                env != AccountEnv::Live || live_switch
            );
        }
        // With the second switch on, a Live account attaches too.
        let meta = account(
            AccountEnv::Live,
            AccountVenue::Binance,
            PositionMode::OneWay,
        );
        let mut modal = modal_of(&meta);
        let _ = modal.update(Message::ProtectEdit(meta.id), &Err("x".into()), &no_closer);
        protect(
            &mut modal,
            meta.id,
            ProtectField::StopKind(OffsetKind::Percent),
        );
        protect(&mut modal, meta.id, ProtectField::StopValue("2".into()));
        protect(&mut modal, meta.id, ProtectField::AutoAttach(true));
        protect(&mut modal, meta.id, ProtectField::AutoAttachLive(true));
        let _ = modal.update(Message::ProtectSave(meta.id), &Err("x".into()), &no_closer);
        assert!(
            modal.persisted().accounts[0]
                .protect
                .attaches_automatically(AccountEnv::Live)
        );
    }

    #[test]
    fn the_live_switch_is_ignored_on_an_account_that_is_not_live() {
        let meta = account(
            AccountEnv::Testnet,
            AccountVenue::Binance,
            PositionMode::OneWay,
        );
        let mut modal = modal_of(&meta);
        let _ = modal.update(Message::ProtectEdit(meta.id), &Err("x".into()), &no_closer);
        protect(&mut modal, meta.id, ProtectField::AutoAttachLive(true));
        assert!(
            !modal.rows[0]
                .protect_form
                .as_ref()
                .unwrap()
                .auto_attach_live
        );
    }

    #[test]
    fn invalid_protect_input_is_not_written_and_says_why() {
        let meta = account(
            AccountEnv::Testnet,
            AccountVenue::Binance,
            PositionMode::OneWay,
        );
        let leg = |kind, value: &str, share: &str| TargetForm {
            kind,
            value: value.into(),
            share: share.into(),
        };
        let cases: Vec<ProtectForm> = vec![
            // A negative stop, a stop of 100 %, an empty amount with a kind chosen.
            ProtectForm {
                stop_kind: OffsetKind::Percent,
                stop_value: "-1".into(),
                ..Default::default()
            },
            ProtectForm {
                stop_kind: OffsetKind::Percent,
                stop_value: "100".into(),
                ..Default::default()
            },
            ProtectForm {
                stop_kind: OffsetKind::Distance,
                ..Default::default()
            },
            // Four levels; shares that do not add up; a share that is not a whole number.
            ProtectForm {
                targets: (0..4)
                    .map(|_| leg(OffsetKind::Percent, "1", "25"))
                    .collect(),
                ..Default::default()
            },
            ProtectForm {
                targets: vec![
                    leg(OffsetKind::Percent, "1", "60"),
                    leg(OffsetKind::Percent, "2", "30"),
                ],
                ..Default::default()
            },
            ProtectForm {
                targets: vec![leg(OffsetKind::Percent, "1", "-100")],
                ..Default::default()
            },
            ProtectForm {
                targets: vec![leg(OffsetKind::Percent, "1", "100.5")],
                ..Default::default()
            },
            // A trailing stop without a callback rate.
            ProtectForm {
                trailing: true,
                ..Default::default()
            },
        ];
        for form in cases {
            let mut modal = modal_of(&meta);
            modal.rows[0].protect_form = Some(form);
            let before = modal.persisted();
            let (_, action) =
                modal.update(Message::ProtectSave(meta.id), &Err("x".into()), &no_closer);
            assert!(action.is_none());
            assert_eq!(modal.persisted(), before);
            let shown = modal.rows[0]
                .protect_form
                .as_ref()
                .expect("editor stays open");
            assert!(shown.error.is_some());
        }
    }

    #[test]
    fn several_levels_are_refused_on_hedge_and_okx_and_the_form_caps_the_levels() {
        let multi = ProtectForm::from_defaults(&ProtectDefaults {
            take_profits: vec![
                TakeProfitLeg {
                    offset: pct("1"),
                    share_pct: 50,
                },
                TakeProfitLeg {
                    offset: pct("2"),
                    share_pct: 50,
                },
            ],
            ..Default::default()
        });
        let one_way = account(
            AccountEnv::Testnet,
            AccountVenue::Binance,
            PositionMode::OneWay,
        );
        assert!(multi.parse(&one_way).is_ok());
        for meta in [
            account(
                AccountEnv::Testnet,
                AccountVenue::Binance,
                PositionMode::Hedge,
            ),
            account(AccountEnv::Testnet, AccountVenue::Okx, PositionMode::OneWay),
        ] {
            assert!(multi.parse(&meta).is_err());
        }
        let mut form = ProtectForm::default();
        for _ in 0..5 {
            form.apply(ProtectField::AddTarget, false);
        }
        assert_eq!(form.targets.len(), MAX_TARGETS);
        // A new level starts empty: no number is made up.
        assert!(form.targets[0].value.is_empty() && form.targets[0].share.is_empty());
    }

    #[test]
    fn cancelling_the_protect_editor_changes_nothing() {
        let meta = account(
            AccountEnv::Testnet,
            AccountVenue::Binance,
            PositionMode::OneWay,
        );
        let mut modal = modal_of(&meta);
        let _ = modal.update(Message::ProtectEdit(meta.id), &Err("x".into()), &no_closer);
        protect(
            &mut modal,
            meta.id,
            ProtectField::StopKind(OffsetKind::Percent),
        );
        let _ = modal.update(
            Message::ProtectCancel(meta.id),
            &Err("x".into()),
            &no_closer,
        );
        assert!(modal.rows[0].protect_form.is_none());
        assert_eq!(
            modal.persisted().accounts[0].protect,
            ProtectDefaults::default()
        );
    }
}
