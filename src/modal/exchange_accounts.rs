//! Settings modal for exchange API accounts (Binance, OKX).
//!
//! Secrets never enter `Message` in plain form (they are wrapped in [`Redacted`]), are never
//! pre-filled from the Keychain, and are wiped from the form on save, failure and close.

use crate::{
    style,
    widget::{BannerKind, banner},
};
use data::config::exchange_accounts::{
    AccountMeta, StoreError, delete_credentials, load_credentials, save_credentials,
};
use exchange::account::{
    AccountEnv, AccountVenue, ApiCredentials, CheckReport, PositionMode, Verdict, check_account,
    is_usable,
};

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
}

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
type SaveOutcome = Result<AccountMeta, String>;

#[derive(Debug, Clone)]
pub enum Message {
    GoBack,
    AliasChanged(String),
    VenueSelected(AccountVenue),
    EnvSelected(AccountEnv),
    ModeSelected(PositionMode),
    KeyChanged(Redacted),
    SecretChanged(Redacted),
    PassphraseChanged(Redacted),
    Save,
    Saved(SaveOutcome),
    Test(Uuid),
    Tested(Uuid, TestOutcome),
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
                })
                .collect(),
            form: Form::default(),
            confirm_delete: None,
            error: None,
            saving: false,
        }
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
        signed: &Result<reqwest::Client, String>,
    ) -> (Task<Message>, Option<Action>) {
        match message {
            Message::GoBack => {
                self.clear_form();
                return (Task::none(), Some(Action::Exit));
            }
            Message::AliasChanged(v) => self.form.alias = v,
            Message::VenueSelected(v) => self.form.venue = v,
            Message::EnvSelected(v) => self.form.env = v,
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
                        return (save_task(meta, creds), None);
                    }
                    Err(e) => self.error = Some(format!("{e}. Credential fields were cleared.")),
                }
            }
            Message::Saved(outcome) => {
                self.saving = false;
                match outcome {
                    Ok(meta) => {
                        self.rows.push(Row {
                            meta,
                            status: Status::NotTested,
                            delete_error: None,
                            deleting: false,
                        });
                        self.error = None;
                        return (Task::none(), Some(Action::Persist));
                    }
                    Err(e) => self.error = Some(format!("{e}. Credential fields were cleared.")),
                }
            }
            Message::Test(id) => return (self.start_test(id, signed), None),
            Message::Tested(id, outcome) => {
                if let Some(row) = self.rows.iter_mut().find(|r| r.meta.id == id) {
                    row.status = match outcome {
                        Ok(report) => Status::Done(*report),
                        Err(e) => Status::Failed(e),
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
                    return (delete_task(row.meta.clone()), None);
                }
            }
            Message::Deleted(id, outcome) => {
                if let Some(pos) = self.rows.iter().position(|r| r.meta.id == id) {
                    match outcome {
                        Ok(()) => {
                            self.rows.remove(pos);
                            return (Task::none(), Some(Action::Persist));
                        }
                        Err(e) => {
                            self.rows[pos].deleting = false;
                            self.rows[pos].delete_error = Some(e);
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
        if self
            .rows
            .iter()
            .any(|r| r.meta.alias.eq_ignore_ascii_case(&alias))
        {
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
        let meta = AccountMeta::new(alias, self.form.venue, self.form.env, self.form.mode);
        Ok((meta, creds))
    }

    fn start_test(&mut self, id: Uuid, signed: &Result<reqwest::Client, String>) -> Task<Message> {
        let Some(row) = self.rows.iter_mut().find(|r| r.meta.id == id) else {
            return Task::none();
        };
        if matches!(row.status, Status::Testing) {
            return Task::none();
        }
        let client = match signed {
            Ok(c) => c.clone(),
            Err(e) => {
                row.status = Status::Failed(e.clone());
                return Task::none();
            }
        };
        row.status = Status::Testing;
        let meta = row.meta.clone();

        Task::perform(
            async move {
                let loader = {
                    let meta = meta.clone();
                    tokio::task::spawn_blocking(move || load_credentials(&meta))
                };
                match loader.await {
                    Ok(Ok(creds)) => Ok(Box::new(
                        check_account(&client, meta.venue, meta.env, &creds, meta.position_mode)
                            .await,
                    )),
                    Ok(Err(e)) => Err(describe_store_error(&e)),
                    Err(_) => Err("Keychain access did not complete".to_string()),
                }
            },
            move |outcome| Message::Tested(id, outcome),
        )
    }

    pub fn view(&self) -> Element<'_, Message> {
        let header = row![
            button(style::icon_text(style::Icon::Return, 11)).on_press(Message::GoBack),
            text("Exchange accounts").size(style::text_size::SECTION),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let mut content = column![header].spacing(12);

        if self.rows.is_empty() {
            content = content.push(text("No accounts yet.").size(style::text_size::SMALL));
        }
        for r in &self.rows {
            content = content.push(self.view_row(r));
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

    fn view_row<'a>(&'a self, r: &'a Row) -> Element<'a, Message> {
        let meta = &r.meta;
        let title = row![
            text(&meta.alias),
            text(meta.venue.to_string()).size(style::text_size::SMALL),
            env_tag(meta.env),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);

        let testing = matches!(r.status, Status::Testing);
        let test_btn =
            button(text("Test")).on_press_maybe((!testing).then_some(Message::Test(meta.id)));

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
        if let Some(e) = &r.delete_error {
            col = col.push(banner(e.as_str(), BannerKind::Error));
        }

        container(col)
            .padding(8)
            .style(style::modal_container)
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

        if f.venue == AccountVenue::Okx {
            col = col.push(
                text_input("API passphrase", &f.passphrase)
                    .on_input(|s| Message::PassphraseChanged(Redacted(s)))
                    .secure(true),
            );
        }
        if f.env == AccountEnv::Testnet {
            col = col.push(
                text(match f.venue {
                    AccountVenue::Binance => "Binance futures testnet: permissions cannot be read.",
                    AccountVenue::Okx => "OKX demo trading (x-simulated-trading: 1).",
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
fn save_task(meta: AccountMeta, creds: ApiCredentials) -> Task<Message> {
    Task::perform(
        async move {
            let job = tokio::task::spawn_blocking(move || {
                // Keychain first: no account is listed unless its credentials were stored
                // and verified.
                match save_credentials(&meta, &creds) {
                    Ok(()) => Ok(meta),
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

fn delete_task(meta: AccountMeta) -> Task<Message> {
    let id = meta.id;
    Task::perform(
        async move {
            let job = tokio::task::spawn_blocking(move || {
                delete_credentials(&meta).map_err(|e| {
                    log::warn!("Deleting exchange credentials failed: {e}");
                    format!("Delete failed: {e}")
                })
            });
            job.await
                .unwrap_or_else(|_| Err("Keychain access did not complete".to_string()))
        },
        move |outcome| Message::Deleted(id, outcome),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE_KEY: &str = "fake-key-AAAA1111";
    const FAKE_SECRET: &str = "fake-secret-BBBB2222";
    const FAKE_PASS: &str = "fake-pass-CCCC3333";

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
        let (_, action) = modal.update(Message::Save, &Err("unused".into()));
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
        let _ = modal.update(Message::SecretChanged(Redacted("ab".into())), &unused);
        let _ = modal.update(Message::SecretChanged(Redacted("abc".into())), &unused);
        assert_eq!(modal.form.secret, "abc");
    }

    #[test]
    fn closing_wipes_secret_fields() {
        let mut modal = ExchangeAccounts::new(&data::ExchangeAccounts::default());
        modal.form.secret = FAKE_SECRET.into();
        modal.clear_form();
        assert!(modal.form.secret.is_empty());
    }
}
