#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod account_flow;
mod audio;
mod auto_flow;
mod connector;
mod layout;
mod logger;
mod modal;
mod notify;
mod notify_os;
mod order_flow;
mod order_lines;
mod protect_attach;
mod replace_flow;
mod screen;
mod style;
mod theme;
mod trading;
mod version;
mod widget;
mod window;

use connector::client::DataSources;
use data::chart::kline::indicators::manifest;
use data::config::theme::default_theme;
use data::config::trade_gate::TradeSession;
use data::{layout::WindowSpec, sidebar};
use exchange::account::{AccountError, ProxyState, check_proxy_snapshot};
use exchange::adapter::UniqueStreams;
use exchange::signed_http::SignedHttp;
use layout::{LayoutId, configuration};
use modal::dashboard_modal;
use modal::{
    LayoutManager, ThemeEditor,
    audio::AudioStream,
    indicator_editor::{self, IndicatorEditor},
    network_editor::{self, NetworkEditor},
};
use notify::Notifications;
use screen::dashboard::{self, Dashboard};
use theme::ThemeState;
use widget::{
    BannerKind,
    toast::{self, Toast},
    tooltip,
};

use iced::{
    Alignment, Element, Subscription, Task, keyboard, padding,
    widget::{
        button, column, container, pane_grid, pick_list, row, rule, scrollable, text,
        tooltip::Position as TooltipPosition,
    },
};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

fn main() {
    logger::install_panic_hook();

    // Before the logger creates the data directory, so a first start can still import.
    let imported_state = data::import_upstream_saved_state();

    if let Err(err) = logger::setup(cfg!(debug_assertions)) {
        logger::report_stderr(&format!("Failed to initialize logger: {err}"));
    }

    match imported_state {
        Ok(Some(path)) => log::info!("Imported upstream saved state into {}", path.display()),
        Ok(None) => {}
        Err(err) => log::warn!("Could not import upstream saved state: {err}"),
    }

    if let Err(error) = data::initialize_indicator_catalog() {
        logger::report_stderr(&error);
        std::process::exit(1);
    }

    std::thread::spawn(data::cleanup_old_market_data);

    let daemon = iced::daemon(Flowsurface::new, Flowsurface::update, Flowsurface::view)
        .settings(iced::Settings {
            antialiasing: data::load_antialiasing(),
            fonts: vec![
                Cow::Borrowed(style::AZERET_MONO_BYTES),
                Cow::Borrowed(style::ICONS_BYTES),
            ],
            default_text_size: style::text_size::BODY.into(),
            ..Default::default()
        })
        .title(Flowsurface::title)
        .theme(Flowsurface::theme)
        .scale_factor(Flowsurface::scale_factor)
        .subscription(Flowsurface::subscription);

    if let Err(err) = daemon.run() {
        let message = format!("Runtime error: {err}");
        log::error!("{message}");
        logger::report_stderr(&message);
    }
}

struct Flowsurface {
    main_window: window::Window,
    sidebar: screen::sidebar::Sidebar,
    layout_manager: LayoutManager,
    theme_editor: ThemeEditor,
    indicator_editor: IndicatorEditor,
    network_editor: NetworkEditor,
    exchange_accounts: modal::exchange_accounts::ExchangeAccounts,
    /// The manual order form (memory only; nothing of it is saved).
    order_ticket: modal::order_ticket::OrderTicket,
    /// What the panes draw of the orders and positions; rebuilt from the trading state when a
    /// pane draws any. Never saved.
    trade_overlay: dashboard::TradeOverlay,
    /// The confirmation window of one prepared order, while it is open.
    order_confirm: Option<modal::order_confirm::PendingConfirm>,
    /// The account section of the ticket (unknown results, cancel all, close all).
    account_panel: modal::account_panel::AccountPanel,
    /// The confirmation of a cancel all, a close all or a lifted risk lock, while it is open.
    /// Never open together with `order_confirm`: one confirmation at a time.
    bulk_confirm: Option<modal::bulk_confirm::PendingBulk>,
    /// The request being sent, kept so an expired preview can be made again.
    /// The request of each send in flight and the window its review started in (a review made
    /// again after the rules expired opens its confirmation in the same window).
    order_requests: HashMap<
        (uuid::Uuid, String),
        (
            order_flow::PrepareRequest,
            window::Id,
            order_flow::OrderSource,
        ),
    >,
    /// The review being prepared was started by a drag on the chart (f3a2): a refusal of it is
    /// said in a toast, since the ticket the other refusals show on may be closed.
    replace_review: bool,
    /// When the ticker list of each venue was last fetched successfully. The list keeps its old
    /// prices when a fetch fails; this stamp is what tells a kept price from a fresh one (see
    /// `auto_flow::reference_price`).
    quote_stamps: HashMap<exchange::adapter::Venue, std::time::Instant>,
    /// Origin of the monotonic clock the automatic protection waits on.
    started: std::time::Instant,
    network_config: data::Network,
    /// The saved state could not be read, so the proxy setting is unknown (see `SavedState`).
    proxy_unreadable: bool,
    /// Connection-test verdicts of this run, read by the send gate. Memory only.
    trade_session: TradeSession,
    /// Startup, private streams, account state and standing alerts. Memory only.
    trading: trading::TradingState,
    data_sources: Arc<DataSources>,
    audio_stream: AudioStream,
    volume_size_unit: exchange::SizeUnit,
    applied_volume_size_unit: exchange::SizeUnit,
    ui_scale_factor: data::ScaleFactor,
    timezone: data::UserTimezone,
    theme: ThemeState,
    notifications: Notifications,
    /// When the app last held back a close request because a resting limit entry still owes
    /// its protection (condition 81); a second request soon after closes.
    close_warned_at: Option<std::time::Instant>,
    window_scale_factors: window::WindowScaleFactors,
    antialiasing: bool,
    applied_antialiasing: bool,
    /// Restored detached windows whose monitor has not been compared with the saved one yet.
    restoring_windows: HashSet<window::Id>,
    /// Tab being dragged by its grip, with the last cursor position seen in the main window.
    tab_drag: Option<TabDrag>,
}

#[derive(Debug, Clone, Copy)]
struct TabDrag {
    layout: uuid::Uuid,
    cursor: Option<iced::Point>,
}

/// Dropping a tab below this many pixels from the window top places the new window at the cursor.
const TAB_DROP_MIN_Y: f32 = 90.0;

#[derive(Debug, Clone, Copy)]
enum Hotkey {
    Escape,
    CycleLayout { forward: bool },
    NewLayout,
    CloseLayout,
    SelectLayout(usize),
    TickerSearch,
    Save,
}

#[derive(Debug, Clone)]
enum Message {
    Sidebar(screen::sidebar::Message),
    MarketWsEvent(exchange::Event),
    Dashboard {
        /// If `None`, the active layout is used for the event.
        layout_id: Option<uuid::Uuid>,
        event: dashboard::Message,
    },
    Tick(std::time::Instant),
    WindowEvent(window::Event),
    ExitRequested(HashMap<window::Id, WindowSpec>),
    SaveStateRequested(HashMap<window::Id, WindowSpec>),
    GoBack,
    /// Cursor position while a tab is being dragged, in main window coordinates.
    TabDragMoved(window::Id, iced::Point),
    TabDragReleased,
    /// Monitor size reported for a detached window, `None` if the platform cannot tell.
    DetachedMonitorChecked(window::Id, Option<iced::Size>),
    Hotkey(Hotkey),
    DataFolderRequested,
    OpenUrlRequested(Cow<'static, str>),
    ThemeSelected(iced_core::Theme),
    ScaleFactorChanged(data::ScaleFactor),
    SetTimezone(data::UserTimezone),
    SetMetadataCaching(bool),
    RefreshMetadata,
    ApplyVolumeSizeUnit(exchange::SizeUnit),
    ApplyAntialiasing(bool),
    RemoveNotification(usize),
    WindowScaleFactorUpdated {
        window: window::Id,
        scale_factor: window::WindowScaleFactor,
    },
    ThemeEditor(modal::theme_editor::Message),
    IndicatorEditor(indicator_editor::Message),
    NetworkEditor(modal::network_editor::Message),
    ExchangeAccounts(modal::exchange_accounts::Message),
    OrderTicket(modal::order_ticket::Message),
    OrderConfirm(modal::order_confirm::Message),
    OrderSent(order_flow::SendSummary),
    /// A stop or target replace ended (f3a2).
    ReplaceSent(replace_flow::ReplaceSummary),
    /// The result of the orders sent by the app itself after an entry filled (M5 d3).
    AutoDone(Box<auto_flow::AutoSummary>),
    /// A slow timer so the bare-position grace time runs out on a quiet account too.
    AutoPoll,
    AccountPanel(modal::account_panel::Message),
    BulkConfirm(modal::bulk_confirm::Message),
    Account(account_flow::Event),
    Trading(trading::Message),
    Layouts(modal::layout_manager::Message),
    SharedChartResources(manifest::Message),
    AudioStream(modal::audio::Message),
}

impl Flowsurface {
    /// The signed-request client, unless the proxy settings changed since it was built:
    /// it must not silently keep using (or bypassing) an outdated proxy.
    /// The proxy setting as it is now. `Broken` when it could not be read at startup.
    fn current_proxy_state(&self) -> ProxyState {
        if self.proxy_unreadable {
            ProxyState::Broken
        } else {
            ProxyState::from_config(self.network_config.proxy.as_ref())
        }
    }

    /// Whether the proxy snapshot of the signed client still matches the current setting.
    /// Checked before every signed request (and, later, every private stream connect, which
    /// must use this same snapshot).
    fn proxy_snapshot_ok(&self) -> Result<(), AccountError> {
        match &self.data_sources.signed {
            Ok(signed) => check_proxy_snapshot(signed.proxy_state(), &self.current_proxy_state()),
            // No client was built: nothing may be signed, whatever the proxy says now.
            Err(_) if self.proxy_unreadable => Err(AccountError::ProxyBroken),
            Err(_) => Err(AccountError::ClientBuild),
        }
    }

    /// The signed-request client, unless the proxy settings changed since it was built:
    /// it must not silently keep using (or bypassing) an outdated proxy.
    fn signed_client_for_current_proxy(&self) -> Result<SignedHttp, String> {
        self.proxy_snapshot_ok().map_err(|e| e.to_string())?;
        self.data_sources.signed.clone()
    }

    /// Drops the pending preview and the confirmation (the ticket was left, or Escape).
    fn discard_order(&mut self) {
        // Only the sends of the confirmation that goes away: another order may be in flight and
        // needs its request when it comes back (a resend after expired rules, the bare position
        // warning cleared by an accepted whole-position stop).
        if let Some(conf) = self.order_confirm.as_ref() {
            order_flow::forget_request(
                &mut self.order_requests,
                conf.request.account,
                &conf.request.symbol,
            );
        }
        self.order_confirm = None;
        self.trading.clear_stray_marks();
        self.bulk_confirm = None;
        self.account_panel.reset_form();
        self.account_panel.finish_bulk_prepare();
        // Makes any preview still on its way stale.
        self.order_ticket.finish_prepare();
    }

    /// A confirmation window of either kind is open (one at a time).
    fn confirm_open(&self) -> bool {
        self.order_confirm.is_some() || self.bulk_confirm.is_some()
    }

    /// The saved account behind an exchange-layer key.
    fn meta_of_key(
        &self,
        key: exchange::account::AccountKey,
    ) -> Option<data::config::exchange_accounts::AccountMeta> {
        self.exchange_accounts
            .persisted()
            .accounts
            .into_iter()
            .find(|m| trading::account_key(m.id) == key)
    }

    fn client_input(
        &self,
        meta: data::config::exchange_accounts::AccountMeta,
    ) -> Result<account_flow::ClientInput, String> {
        Ok(account_flow::ClientInput {
            meta,
            signed: self.signed_client_for_current_proxy()?,
            proxy: self.trading.proxy_reader(),
        })
    }

    fn reload_records() -> Task<Message> {
        Task::perform(account_flow::load_records(), |r| {
            Message::Account(account_flow::Event::RecordsLoaded(r))
        })
    }

    /// A window went away: its scale factor goes, and so does a confirmation that was open in
    /// it (nobody could press it, and it would block every other review). It is dropped like
    /// Escape does.
    fn forget_window(&mut self, window: window::Id) {
        self.window_scale_factors.remove(window);
        if order_flow::confirm_lost_with(
            self.order_confirm.as_ref().map(|c| c.window),
            self.bulk_confirm.as_ref().map(|c| c.window),
            window,
        ) {
            self.discard_order();
        }
    }

    /// The window a confirmation opens in: the one that asked, or the main window when that one
    /// is gone.
    fn live_origin(&self, origin: window::Id) -> window::Id {
        order_flow::live_origin(origin, self.main_window.id, |w| {
            self.window_scale_factors.contains(w)
        })
    }

    /// Rebuilds what the panes say about the trading account (the ladder title bar).
    fn refresh_trade_overlay(&mut self) {
        let accounts = self.exchange_accounts.persisted().accounts;
        let live_open = data::config::trade_gate::live_sending_enabled();
        self.trade_overlay = dashboard::TradeOverlay {
            accounts: accounts
                .iter()
                .map(|m| (self.account_choice(m), m.alias.clone()))
                .collect(),
            ticket_account: self.order_ticket.account(),
            live_open,
            lines: accounts
                .iter()
                .map(|m| dashboard::AccountLines {
                    account: m.id,
                    lines: self.lines_of_account(m, live_open),
                })
                .filter(|a| !a.lines.is_empty())
                .collect(),
        };
        // Each pane compares what it gets and redraws only for a change, so a refresh that
        // changed nothing costs no redraw.
        let main_window = self.main_window.id;
        for id in self.layout_manager.visible_ids() {
            if let Some(dashboard) = self.layout_manager.mut_dashboard(id) {
                dashboard.apply_trade_overlay(main_window, &self.trade_overlay);
            }
        }
    }

    /// The lines of one account. A stop or target line says whether it can be dragged and, when
    /// it cannot, why (f3a2); the chart adds its own reason (a percent or log scale).
    fn lines_of_account(
        &self,
        m: &data::config::exchange_accounts::AccountMeta,
        live_open: bool,
    ) -> Vec<order_lines::SymbolLine> {
        use order_lines::Grab;
        let guard = self.trading.replace_guard(m.id, live_open);
        let grab = |row: &exchange::trade::OpenOrder| match &guard {
            None => Grab::Blocked(replace_flow::DragBlock::Stream),
            Some(g) => {
                match replace_flow::drag_block(row, g, self.trading.is_auto_order(m.id, row)) {
                    Some(why) => Grab::Blocked(why),
                    None => Grab::Yes(row.id.clone()),
                }
            }
        };
        order_lines::lines_with(
            self.trading.open_orders(m.id),
            self.trading.positions(m.id),
            self.trading.lists_stale(m.id),
            &order_lines::Ctx {
                spot: m.product == exchange::account::AccountProduct::Spot,
                contracts: m.venue == exchange::account::AccountVenue::Okx
                    && m.product == exchange::account::AccountProduct::UsdM,
                grab: &grab,
            },
        )
    }

    fn account_choice(
        &self,
        m: &data::config::exchange_accounts::AccountMeta,
    ) -> order_flow::AccountChoice {
        order_flow::AccountChoice {
            id: m.id,
            venue: m.venue,
            product: m.product,
            env: m.env,
            tested: self.trade_session.get(m.id).is_some(),
        }
    }

    /// A limit order a pane asked for (a command click on the ladder) opens the ordinary review
    /// in the pane's own window (main, popout or detached layout). The account is the ticket's,
    /// or the only one that fits; the quantity is the ticket's. A refusal is a toast. Nothing is
    /// sent from here: the confirmation does that, and only after the user confirms.
    fn start_trade_request(
        &mut self,
        order: order_flow::PaneOrder,
        window: window::Id,
    ) -> Task<Message> {
        let accounts: Vec<_> = self
            .exchange_accounts
            .persisted()
            .accounts
            .iter()
            .map(|m| self.account_choice(m))
            .collect();
        match order_flow::pane_request(
            &order,
            self.order_ticket.account(),
            &accounts,
            data::config::trade_gate::live_sending_enabled(),
            self.order_ticket.quantity(),
        ) {
            Ok((request, _)) => {
                // The click takes over the preview the ticket was making (one review at a
                // time): say so, the ticket may not be on screen.
                if self.order_ticket.is_preparing() {
                    self.notifications.push(Toast::warn(
                        "This price replaces the order the ticket was preparing.".to_string(),
                    ));
                }
                self.start_review(request, window, order_flow::OrderSource::Dom)
            }
            Err(text) => {
                self.review_refused(text, BannerKind::Warning);
                Task::none()
            }
        }
    }

    /// Why a review did not start: on the ticket, and in a toast when the ticket is closed (a
    /// click in a pane has no ticket to show it).
    fn review_refused(&mut self, text: String, kind: BannerKind) {
        if self.sidebar.active_menu() != Some(sidebar::Menu::OrderTicket) {
            self.notifications.push(match kind {
                BannerKind::Error => Toast::error(text.clone()),
                _ => Toast::warn(text.clone()),
            });
        }
        self.order_ticket.set_status(text, kind);
    }

    /// Checks the gate again and starts the background preview of a request. The confirmation
    /// opens in `origin`, the window the request came from.
    fn start_review(
        &mut self,
        request: order_flow::PrepareRequest,
        origin: window::Id,
        source: order_flow::OrderSource,
    ) -> Task<Message> {
        self.start_review_of(request, origin, source, None)
    }

    /// [`Self::start_review`] with the old order of a replace (f3a2).
    fn start_review_of(
        &mut self,
        request: order_flow::PrepareRequest,
        origin: window::Id,
        source: order_flow::OrderSource,
        old: Option<exchange::trade::OpenOrder>,
    ) -> Task<Message> {
        use modal::order_ticket as ticket;
        if self.trading.bulk_running(request.account) || self.account_panel.is_bulk_preparing() {
            self.review_refused(
                "A cancel all or close all is being prepared or sent. Wait for it.".to_string(),
                BannerKind::Warning,
            );
            return Task::none();
        }
        let accounts = self.exchange_accounts.persisted().accounts;
        let Some(meta) = accounts.iter().find(|m| m.id == request.account).cloned() else {
            self.review_refused(
                "The account no longer exists.".to_string(),
                BannerKind::Error,
            );
            return Task::none();
        };
        // OKX: this build sends orders in cross margin. When the account holds the symbol in
        // isolated margin the orders would not match the position: nothing is sent. A cancel has
        // no margin mode and is never refused for this.
        if !matches!(request.spec, exchange::trade::ManualSpec::Cancel { .. })
            && self.trading.margin_conflict(meta.id, &request.symbol)
        {
            self.review_refused(
                "This position is held in isolated margin, but this app sends OKX orders in \
                 cross margin, so the order would not match it. Nothing was sent. Change the \
                 position on the exchange or use its own screen."
                    .to_string(),
                BannerKind::Error,
            );
            return Task::none();
        }
        // `Ticker::new` asserts on odd text: never let typed text reach it unchecked.
        if !ticket::valid_symbol(&request.symbol) {
            self.review_refused(
                "The symbol may hold up to 28 letters, digits, '-' or '_'.".to_string(),
                BannerKind::Error,
            );
            return Task::none();
        }
        let signed = self.signed_client_for_current_proxy();
        let gate = ticket::gate_for(
            Some(&meta),
            ticket::kind_of(&request.spec),
            &request.symbol,
            &self.trading,
            &self.trade_session,
            signed.is_ok(),
            false,
        );
        if let Some(why) = ticket::blocked_reason(&gate) {
            self.review_refused(why.to_string(), BannerKind::Error);
            return Task::none();
        }
        let Ok(signed) = signed else {
            self.review_refused(
                "No usable connection to the exchange.".to_string(),
                BannerKind::Error,
            );
            return Task::none();
        };
        let ref_price = order_flow::trade_market(order_flow::market_of(meta.venue, meta.product))
            .and_then(|_| {
                let ticker = exchange::Ticker::new(
                    &request.symbol,
                    order_flow::market_of(meta.venue, meta.product),
                );
                self.sidebar.tickers_table.mark_price(&ticker)
            })
            .and_then(order_flow::price_to_decimal);
        let mode = self.trade_session.get(meta.id).and_then(|e| e.actual_mode);
        let held = self.trading.holdings(meta.id, &request.symbol);
        let auto_active = self.trading.auto_active(meta.id, &request.symbol);
        let nonce = self.order_ticket.start_prepare();
        let input = order_flow::PrepareInput {
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
            proxy: self.trading.proxy_reader(),
        };
        self.replace_review = source == order_flow::OrderSource::Replace;
        Task::perform(order_flow::prepare(input), move |result| {
            Message::OrderTicket(modal::order_ticket::Message::Prepared(
                nonce,
                result.map(Box::new),
            ))
        })
    }

    fn update_order_ticket(&mut self, msg: modal::order_ticket::Message) -> Task<Message> {
        use modal::order_ticket::{Action, Message as T};
        if let T::Prepared(nonce, result) = msg {
            // An answer for a review that was replaced or discarded is dropped.
            if !self.order_ticket.is_current(nonce) {
                return Task::none();
            }
            self.order_ticket.finish_prepare();
            match result {
                Ok(prepared) => {
                    // Never replace a confirmation that is open: the user may be about to
                    // press its Confirm (M2). The new preview is dropped.
                    if self.confirm_open() {
                        // Also a toast when the ticket is closed (a click in a pane started
                        // this review).
                        self.review_refused(
                            "Another order is waiting for confirmation. Review again after it."
                                .to_string(),
                            BannerKind::Warning,
                        );
                        return Task::none();
                    }
                    // The account of the preview itself, not the one the ticket shows now.
                    let accounts = self.exchange_accounts.persisted().accounts;
                    // Opens in the window the review was started in, or in the main window
                    // when that one was closed meanwhile.
                    let origin = self.live_origin(prepared.origin);
                    match modal::order_confirm::account_of(&accounts, &prepared.request) {
                        Some(meta) => {
                            self.order_confirm = Some(modal::order_confirm::PendingConfirm::new(
                                origin,
                                meta,
                                &accounts,
                                *prepared,
                                std::time::Instant::now(),
                            ));
                        }
                        None => self.order_ticket.set_status(
                            "The account no longer exists.".to_string(),
                            BannerKind::Error,
                        ),
                    }
                }
                Err(e) => {
                    if std::mem::take(&mut self.replace_review) {
                        self.notifications.push(Toast::error(e.clone()));
                    }
                    self.order_ticket.set_status(e, BannerKind::Error);
                }
            }
            self.replace_review = false;
            return Task::none();
        }
        let account_before = self.order_ticket.account();
        let action = self.order_ticket.update(msg);
        if self.order_ticket.account() != account_before {
            // A form typed for the previous account never carries over to the new one.
            self.account_panel.reset_form();
        }
        match action {
            // The ticket is a sidebar menu: it only exists in the main window.
            Some(Action::Review(request)) => self.start_review(
                *request,
                self.main_window.id,
                order_flow::OrderSource::Ticket,
            ),
            Some(Action::Exit) => {
                self.discard_order();
                self.sidebar.set_menu(None);
                Task::none()
            }
            None => Task::none(),
        }
    }

    fn update_order_confirm(&mut self, msg: modal::order_confirm::Message) -> Task<Message> {
        use modal::order_confirm::Message as C;
        match msg {
            C::DelayElapsed => Task::none(),
            C::Cancel => {
                self.order_confirm = None;
                self.trading.clear_stray_marks();
                Task::none()
            }
            C::Confirm => {
                if self
                    .order_confirm
                    .as_ref()
                    .is_some_and(|c| c.replace_info().is_some())
                {
                    self.confirm_replace()
                } else {
                    self.confirm_order()
                }
            }
        }
    }

    fn update_account_panel(&mut self, msg: modal::account_panel::Message) -> Task<Message> {
        use modal::account_panel::Action;
        let accounts = self.exchange_accounts.persisted().accounts;
        let meta = self
            .order_ticket
            .account()
            .and_then(|id| accounts.iter().find(|m| m.id == id))
            .cloned();
        let ticket_deleting = meta
            .as_ref()
            .is_some_and(|m| self.exchange_accounts.is_deleting(m.id));
        let action = self
            .account_panel
            .update(msg, &self.trading, meta.as_ref(), ticket_deleting);
        let Some(action) = action else {
            return Task::none();
        };
        // An account that is being deleted starts nothing (no Keychain read, no client). It is
        // the account of the record that counts, not the one the ticket shows now (N4).
        let record_account = match &action {
            Action::BeginRelease(r) | Action::Release(_, r) | Action::Discard(_, r) => {
                Some(r.account)
            }
            Action::Bulk(_)
            | Action::AckTrip
            | Action::Retry(_)
            | Action::Handled(_)
            | Action::AddStop(..)
            | Action::CancelStray(_) => None,
        };
        if record_account
            .and_then(|key| self.meta_of_key(key))
            .is_some_and(|m| self.exchange_accounts.is_deleting(m.id))
        {
            self.account_panel.form_done();
            self.account_panel.set_status(
                "The account is being deleted.".to_string(),
                BannerKind::Error,
            );
            return Task::none();
        }
        match action {
            Action::Bulk(kind) => self.start_bulk(kind),
            Action::AckTrip => self.start_ack_trip(),
            Action::Retry(key) => self.start_protection_retry(&key),
            Action::AddStop(symbol, dir) => self.start_add_stop(&symbol, dir),
            Action::CancelStray(id) => self.start_stray_cancel(&id),
            Action::Handled(key) => {
                self.handle_protection_failure(&key);
                Task::none()
            }
            Action::BeginRelease(record) => {
                let Some(meta) = self.meta_of_key(record.account) else {
                    self.account_panel.form_done();
                    return Task::none();
                };
                match self.client_input(meta) {
                    Ok(input) => {
                        let shown = record.clone();
                        Task::perform(account_flow::begin_release(input, *record), move |r| {
                            Message::Account(account_flow::Event::ReleaseBegun(shown.clone(), r))
                        })
                    }
                    Err(e) => {
                        self.account_panel.form_done();
                        self.account_panel.set_status(e, BannerKind::Error);
                        Task::none()
                    }
                }
            }
            Action::Release(token, record) => {
                let Some(meta) = self.meta_of_key(record.account) else {
                    self.account_panel.form_done();
                    return Task::none();
                };
                match self.client_input(meta) {
                    Ok(input) => Task::perform(account_flow::release(input, token, *record), |s| {
                        Message::Account(account_flow::Event::Released(Box::new(s)))
                    }),
                    Err(e) => {
                        self.account_panel.form_done();
                        self.account_panel.set_status(e, BannerKind::Error);
                        Task::none()
                    }
                }
            }
            Action::Discard(token, record) => {
                let Some(meta) = self.meta_of_key(record.account) else {
                    self.account_panel.form_done();
                    return Task::none();
                };
                Task::perform(account_flow::discard(token, meta, *record), |s| {
                    Message::Account(account_flow::Event::Discarded(Box::new(s)))
                })
            }
        }
    }

    /// The account the panel and the ticket show, if it still exists.
    fn panel_account(&self) -> Option<data::config::exchange_accounts::AccountMeta> {
        let id = self.order_ticket.account()?;
        self.exchange_accounts
            .persisted()
            .accounts
            .into_iter()
            .find(|m| m.id == id)
    }

    /// "Retry protection" (condition 65): a stop for the whole position is offered again through
    /// the ordinary manual path, so it gets a new preview, a new client id and a new
    /// confirmation token, and nothing is sent before the user confirms it. It is not an
    /// automatic order: no grant of the failed entry is used or made.
    fn start_protection_retry(&mut self, key: &str) -> Task<Message> {
        let Some(meta) = self.panel_account() else {
            self.account_panel
                .set_status("Pick an account.".to_string(), BannerKind::Error);
            return Task::none();
        };
        if self.confirm_open() || self.order_ticket.is_preparing() {
            self.account_panel.set_status(
                "Another order is open or being prepared. Finish it first.".to_string(),
                BannerKind::Warning,
            );
            return Task::none();
        }
        let Some(failure) = self
            .trading
            .auto_failures(meta.id)
            .into_iter()
            .find(|f| f.key == key && f.may_retry)
        else {
            self.account_panel.set_status(
                "That warning is no longer there, or a stop cannot be offered for it.".to_string(),
                BannerKind::Warning,
            );
            return Task::none();
        };
        self.open_stop_review(&meta, failure.symbol, failure.dir)
    }

    /// "Add a stop-loss" on a bare position the lists show (d5): the same offer as the retry, for
    /// a position found by the watch rather than named by a failed entry. The position is looked
    /// up again here by value (symbol and direction), and the stop is built from the current
    /// lists and price; nothing is sent before the confirmation.
    fn start_add_stop(&mut self, symbol: &str, dir: exchange::trade::Dir) -> Task<Message> {
        let Some(meta) = self.panel_account() else {
            self.account_panel
                .set_status("Pick an account.".to_string(), BannerKind::Error);
            return Task::none();
        };
        if self.confirm_open() || self.order_ticket.is_preparing() {
            self.account_panel.set_status(
                "Another order is open or being prepared. Finish it first.".to_string(),
                BannerKind::Warning,
            );
            return Task::none();
        }
        if !self
            .trading
            .bare_rows(meta.id)
            .iter()
            .any(|r| r.symbol == symbol && r.dir == dir)
        {
            self.account_panel.set_status(
                "That position no longer shows as unprotected. Nothing was changed.".to_string(),
                BannerKind::Warning,
            );
            return Task::none();
        }
        self.open_stop_review(&meta, symbol.to_string(), dir)
    }

    /// The shared part of the two stop offers: build a whole-position stop from the current
    /// lists and price and open its review (a new preview, client id and confirmation token).
    fn open_stop_review(
        &mut self,
        meta: &data::config::exchange_accounts::AccountMeta,
        symbol: String,
        dir: exchange::trade::Dir,
    ) -> Task<Message> {
        // The symbol comes from the exchange's position list: checked before it reaches
        // `Ticker::new`, which panics on a long one.
        if !modal::order_ticket::valid_symbol(&symbol) {
            self.account_panel.set_status(
                "The symbol of that position cannot be used for an order here. Nothing was \
                 changed."
                    .to_string(),
                BannerKind::Error,
            );
            return Task::none();
        }
        let mode = self.trade_session.get(meta.id).and_then(|e| e.actual_mode);
        let held = self.trading.holdings(meta.id, &symbol);
        let quote = self.quote_of(meta, &symbol);
        match protect_attach::retry_stop(meta, mode, dir, &held, quote) {
            Ok(spec) => {
                // `start_review` may refuse (it says why on the ticket): only claim a preview
                // when one is really being prepared.
                let task = self.start_review(
                    order_flow::PrepareRequest {
                        account: meta.id,
                        symbol,
                        spec,
                        protect: None,
                    },
                    // Started from the account panel, a sidebar menu of the main window.
                    self.main_window.id,
                    order_flow::OrderSource::Ticket,
                );
                if self.order_ticket.is_preparing() {
                    self.account_panel.set_status(
                        "Preparing the stop. Check it in the confirmation window.".to_string(),
                        BannerKind::Normal,
                    );
                } else {
                    self.account_panel.set_status(
                        "The stop could not be prepared. See the order ticket for the reason."
                            .to_string(),
                        BannerKind::Error,
                    );
                }
                task
            }
            Err(why) => {
                self.account_panel.set_status(why, BannerKind::Error);
                Task::none()
            }
        }
    }

    /// "Cancel" on an order with no position, or one that looks left over (d5): the cancel of
    /// that one order goes through the ordinary review and confirmation (it is never the stop-loss
    /// of a position still held: such an order is only shown, see `stray_orders`), never through "Cancel all", which would also cancel the stops of other
    /// positions. The order is checked on fresh lists now and again when the user confirms.
    fn start_stray_cancel(&mut self, order_id: &str) -> Task<Message> {
        let Some(meta) = self.panel_account() else {
            self.account_panel
                .set_status("Pick an account.".to_string(), BannerKind::Error);
            return Task::none();
        };
        if self.confirm_open() || self.order_ticket.is_preparing() {
            self.account_panel.set_status(
                "Another order is open or being prepared. Finish it first.".to_string(),
                BannerKind::Warning,
            );
            return Task::none();
        }
        let found = self
            .trading
            .open_orders(meta.id)
            .iter()
            .find(|o| o.id == order_id)
            .map(|o| (o.symbol.clone(), o.kind.clone()));
        let (Some((symbol, kind)), true) = (found, self.trading.is_stray(meta.id, order_id)) else {
            self.account_panel.set_status(
                "That order is no longer one to cancel, or the lists may be out of date. \
                 Nothing was changed."
                    .to_string(),
                BannerKind::Warning,
            );
            return Task::none();
        };
        let task = self.start_review(
            order_flow::PrepareRequest {
                account: meta.id,
                symbol,
                spec: exchange::trade::ManualSpec::Cancel {
                    id: order_id.to_string(),
                    kind,
                },
                protect: None,
            },
            // Started from the account panel, a sidebar menu of the main window.
            self.main_window.id,
            order_flow::OrderSource::Ticket,
        );
        if self.order_ticket.is_preparing() {
            self.trading.mark_stray_cancel(meta.id, order_id);
            self.account_panel.set_status(
                "Preparing the cancel. Check it in the confirmation window.".to_string(),
                BannerKind::Normal,
            );
        } else {
            self.account_panel.set_status(
                "The cancel could not be prepared. See the order ticket for the reason."
                    .to_string(),
                BannerKind::Error,
            );
        }
        task
    }

    /// "I've handled it", after its second click: the entry leaves the book, its symbol is no
    /// longer blocked by it and a line goes to the risk log. Nothing is sent, and no unknown
    /// result is released by this (that keeps its own confirmation phrase).
    fn handle_protection_failure(&mut self, key: &str) {
        let Some(meta) = self.panel_account() else {
            self.account_panel
                .set_status("Pick an account.".to_string(), BannerKind::Error);
            return;
        };
        match self.trading.dismiss_failure(meta.id, key) {
            Ok(symbol) => {
                let logged = data::config::risk_log::RiskLog::in_data_dir()
                    .append(&data::config::risk_log::RiskLogEntry::protection_handled(
                        &meta,
                        chrono::Utc::now().timestamp_millis(),
                        &symbol,
                    ))
                    .is_ok();
                let text = if logged {
                    "The warning was removed.".to_string()
                } else {
                    "The warning was removed, but the risk log could not be written.".to_string()
                };
                let kind = if logged {
                    BannerKind::Normal
                } else {
                    BannerKind::Warning
                };
                self.account_panel.set_status(text, kind);
            }
            Err(auto_flow::DismissError::NotFound) => self.account_panel.set_status(
                "That warning is already gone.".to_string(),
                BannerKind::Normal,
            ),
            Err(auto_flow::DismissError::Busy) => self.account_panel.set_status(
                "An automatic order of this entry is being sent. Try again in a moment."
                    .to_string(),
                BannerKind::Warning,
            ),
        }
    }

    /// One system notification per entry whose protection failed (macOS only, fixed text with
    /// the alias).
    fn notify_failures(&mut self) {
        let accounts = self.exchange_accounts.persisted().accounts;
        for id in notify_os::unique_accounts(self.trading.take_failure_notices()) {
            let alias = accounts
                .iter()
                .find(|m| m.id == id)
                .map(|m| m.alias.clone())
                .unwrap_or_default();
            notify_os::send(notify_os::Notice::ProtectionFailed, &alias);
        }
    }

    /// One system notification per account with a bare position that was confirmed on fresh
    /// lists and not told about yet (macOS only, fixed text with the alias).
    fn notify_bare(&mut self) {
        let accounts = self.exchange_accounts.persisted().accounts;
        for id in notify_os::unique_accounts(self.trading.take_bare_notices()) {
            let alias = accounts
                .iter()
                .find(|m| m.id == id)
                .map(|m| m.alias.clone())
                .unwrap_or_default();
            notify_os::send(notify_os::Notice::BarePosition, &alias);
        }
    }

    /// Cancel all or close all: gate, then a fresh read for the confirmation window.
    fn start_bulk(&mut self, action: exchange::trade::BulkAction) -> Task<Message> {
        use modal::{account_panel as panel, order_ticket as ticket};
        let accounts = self.exchange_accounts.persisted().accounts;
        let Some(meta) = self
            .order_ticket
            .account()
            .and_then(|id| accounts.iter().find(|m| m.id == id))
            .cloned()
        else {
            self.account_panel
                .set_status("Pick an account.".to_string(), BannerKind::Error);
            return Task::none();
        };
        let signed = self.signed_client_for_current_proxy();
        let gate = ticket::gate_for(
            Some(&meta),
            None,
            "",
            &self.trading,
            &self.trade_session,
            signed.is_ok(),
            self.order_ticket.is_preparing() || self.account_panel.is_bulk_preparing(),
        );
        let why = panel::bulk_blocked_reason(
            &gate,
            self.exchange_accounts.is_deleting(meta.id),
            self.trading.bulk_running(meta.id),
            self.trading.replace_busy(meta.id),
            self.confirm_open(),
        )
        .or_else(|| {
            (action == exchange::trade::BulkAction::Flatten
                && !panel::flatten_offered(meta.venue, meta.product))
            .then_some("Close all is not offered for a spot account.")
        });
        if let Some(why) = why {
            self.account_panel
                .set_status(why.to_string(), BannerKind::Error);
            return Task::none();
        }
        let signed = match signed {
            Ok(s) => s,
            Err(e) => {
                self.account_panel.set_status(e, BannerKind::Error);
                return Task::none();
            }
        };
        let nonce = self.account_panel.start_bulk();
        // The account panel is a sidebar menu: the confirmation opens in the main window.
        let origin = self.main_window.id;
        let request = account_flow::BulkRequest {
            account: meta.id,
            action,
        };
        let input = account_flow::BulkPrepareInput {
            client: account_flow::ClientInput {
                meta,
                signed,
                proxy: self.trading.proxy_reader(),
            },
            request,
        };
        Task::perform(account_flow::prepare_bulk(input), move |r| {
            Message::Account(account_flow::Event::BulkPrepared(
                nonce,
                origin,
                r.map(Box::new),
            ))
        })
    }

    /// Lifting a risk lock needs its own confirmation step.
    fn start_ack_trip(&mut self) -> Task<Message> {
        let accounts = self.exchange_accounts.persisted().accounts;
        let Some(meta) = self
            .order_ticket
            .account()
            .and_then(|id| accounts.iter().find(|m| m.id == id))
        else {
            return Task::none();
        };
        if self.confirm_open() {
            self.account_panel.set_status(
                "Another confirmation is open. Finish or cancel it first.".to_string(),
                BannerKind::Warning,
            );
            return Task::none();
        }
        let Some(reason) = self.trading.trip_reason(meta.id) else {
            self.account_panel.set_status(
                "The account has no risk lock.".to_string(),
                BannerKind::Normal,
            );
            return Task::none();
        };
        let lines = vec![
            format!("The risk lock is on: {reason}."),
            "Lifting it allows new entries again for this account. Nothing is sent.".to_string(),
        ];
        self.bulk_confirm = Some(modal::bulk_confirm::PendingBulk::new(
            self.main_window.id,
            modal::bulk_confirm::Kind::AckTrip,
            meta,
            &accounts,
            lines,
            true,
            std::time::Instant::now(),
        ));
        Task::none()
    }

    fn update_bulk_confirm(&mut self, msg: modal::bulk_confirm::Message) -> Task<Message> {
        use modal::bulk_confirm::Message as B;
        match msg {
            B::Cancel => {
                self.bulk_confirm = None;
                Task::none()
            }
            B::Confirm => self.confirm_bulk(),
        }
    }

    /// The one click of a bulk confirmation: the account is the one stored when the window
    /// opened, the gate is checked once more, and the token comes out of the window (once).
    fn confirm_bulk(&mut self) -> Task<Message> {
        use modal::{account_panel as panel, bulk_confirm::Confirmed, order_ticket as ticket};
        let Some(conf) = self.bulk_confirm.as_ref() else {
            return Task::none();
        };
        let (id, kind) = (conf.account, conf.kind());
        let accounts = self.exchange_accounts.persisted().accounts;
        let meta = accounts.iter().find(|m| m.id == id).cloned();
        let signed = self.signed_client_for_current_proxy();
        if let modal::bulk_confirm::Kind::Bulk(_) = kind {
            let gate = ticket::gate_for(
                meta.as_ref(),
                None,
                "",
                &self.trading,
                &self.trade_session,
                signed.is_ok(),
                false,
            );
            let risk_missing = self.trading.risk_for(id).is_none();
            let why = panel::bulk_blocked_reason(
                &gate,
                self.exchange_accounts.is_deleting(id),
                self.trading.bulk_running(id),
                self.trading.replace_busy(id),
                false,
            )
            .or_else(|| {
                risk_missing.then_some(
                    "The account has no running trading state. Test it again in Exchange \
                     accounts.",
                )
            });
            if let Some(why) = why {
                self.bulk_confirm = None;
                self.account_panel
                    .set_status(why.to_string(), BannerKind::Error);
                return Task::none();
            }
        }
        // Once: the window closes with the token, so a second click finds none.
        let Some(confirmed) =
            modal::bulk_confirm::take_confirmed(&mut self.bulk_confirm, std::time::Instant::now())
        else {
            return Task::none();
        };
        match confirmed {
            Confirmed::AckTrip => {
                // Only a local change, but not while the account is deleted or a bulk runs.
                if self.exchange_accounts.is_deleting(id) || self.trading.bulk_running(id) {
                    self.account_panel.set_status(
                        "The risk lock was not lifted: the account is being deleted or a cancel \
                         all or close all is running."
                            .to_string(),
                        BannerKind::Error,
                    );
                    return Task::none();
                }
                let (text, kind) = if self.trading.acknowledge_trip(id) {
                    ("The risk lock was lifted.", BannerKind::Normal)
                } else {
                    (
                        "The risk lock could not be lifted (the risk state is unavailable).",
                        BannerKind::Error,
                    )
                };
                self.account_panel.set_status(text.to_string(), kind);
                Task::none()
            }
            Confirmed::Bulk(token) => {
                let (Some(meta), Ok(signed), Some(risk), modal::bulk_confirm::Kind::Bulk(action)) =
                    (meta, signed, self.trading.risk_for(id), kind)
                else {
                    return Task::none();
                };
                self.trading.begin_bulk(id);
                self.account_panel
                    .set_status("Sending…".to_string(), BannerKind::Normal);
                let input = account_flow::BulkSendInput {
                    session: self.trade_session.get(meta.id).cloned(),
                    client: account_flow::ClientInput {
                        meta,
                        signed,
                        proxy: self.trading.proxy_reader(),
                    },
                    risk,
                    token,
                    request: account_flow::BulkRequest {
                        account: id,
                        action,
                    },
                };
                Task::perform(account_flow::send_bulk(input), |s| {
                    Message::Account(account_flow::Event::BulkDone(Box::new(s)))
                })
            }
        }
    }

    fn say(&mut self, text: String, kind: BannerKind) {
        // With the ticket closed nobody sees the panel: say it in a toast as well.
        if self.sidebar.active_menu() != Some(sidebar::Menu::OrderTicket) {
            self.notifications.push(match kind {
                BannerKind::Normal => Toast::info(text.clone()),
                BannerKind::Warning => Toast::warn(text.clone()),
                BannerKind::Error => Toast::error(text.clone()),
            });
        }
        self.account_panel.set_status(text, kind);
    }

    fn on_account_event(&mut self, event: account_flow::Event) -> Task<Message> {
        use account_flow::Event as E;
        match event {
            E::BulkPrepared(nonce, origin, result) => {
                // An answer for a request that was replaced or discarded is dropped.
                if !self.account_panel.is_bulk_current(nonce) {
                    return Task::none();
                }
                self.account_panel.finish_bulk_prepare();
                match result {
                    Err(e) => self.account_panel.set_status(e, BannerKind::Error),
                    Ok(preview) => {
                        // Never replace a confirmation that is open (M2).
                        if self.confirm_open() {
                            self.account_panel.set_status(
                                "Another confirmation is open. This one was dropped; start it \
                                 again afterwards."
                                    .to_string(),
                                BannerKind::Warning,
                            );
                            return Task::none();
                        }
                        let accounts = self.exchange_accounts.persisted().accounts;
                        match accounts.iter().find(|m| m.id == preview.request.account) {
                            Some(meta) => {
                                self.bulk_confirm = Some(modal::bulk_confirm::PendingBulk::new(
                                    origin,
                                    modal::bulk_confirm::Kind::Bulk(preview.request.action),
                                    meta,
                                    &accounts,
                                    preview.lines(),
                                    !preview.nothing_to_do(),
                                    std::time::Instant::now(),
                                ));
                            }
                            None => self.account_panel.set_status(
                                "The account no longer exists.".to_string(),
                                BannerKind::Error,
                            ),
                        }
                    }
                }
                Task::none()
            }
            E::BulkDone(summary) => {
                let account_flow::BulkSummary { request, outcome } = *summary;
                self.trading.finish_bulk(request.account);
                if let account_flow::BulkOutcome::Done(n) = &outcome {
                    if n.journal_failed {
                        self.trading.set_journal_failed();
                    }
                    for symbol in &n.unresolved {
                        self.trading.finish_send(request.account, symbol, true);
                    }
                }
                let (lines, kind) = account_flow::describe_bulk(request.action, &outcome);
                self.say(lines.join(" "), kind);
                Self::reload_records()
            }
            E::RecordsLoaded(result) => {
                match result {
                    Ok(records) => self.account_panel.set_records(records),
                    Err(e) => self.account_panel.set_status(
                        format!("The saved unknown results could not be read: {e}."),
                        BannerKind::Error,
                    ),
                }
                Task::none()
            }
            E::ReleaseBegun(record, result) => {
                self.account_panel.release_begun(&record, result);
                Task::none()
            }
            E::Released(summary) => {
                self.account_panel.form_done();
                let (text, kind) = account_flow::describe_release(&summary);
                if let account_flow::ReleaseResult::Released { .. } = summary.result
                    && let Some(meta) = self.meta_of_key(summary.record.account)
                {
                    let still_stored = summary.remaining.as_ref().is_none_or(|rs| {
                        rs.iter().any(|r| {
                            r.account == summary.record.account && r.symbol == summary.record.symbol
                        })
                    });
                    self.trading.entry_gone(&summary.record.client_id);
                    self.trading.record_released(
                        meta.id,
                        &summary.record.symbol,
                        &summary.record.client_id,
                        still_stored,
                    );
                }
                self.say(text, kind);
                match summary.remaining {
                    Some(records) => {
                        self.account_panel.set_records(records);
                        Task::none()
                    }
                    None => Self::reload_records(),
                }
            }
            E::Discarded(summary) => {
                self.account_panel.form_done();
                let (text, kind) = account_flow::describe_discard(&summary.result);
                if summary.result.is_ok()
                    && let Some(meta) = self.meta_of_key(summary.record.account)
                {
                    // The account stays blocked until restart: only the list entry changes.
                    self.trading.entry_gone(&summary.record.client_id);
                    self.trading
                        .record_discarded(meta.id, &summary.record.client_id);
                }
                self.say(text, kind);
                match summary.remaining {
                    Some(records) => {
                        self.account_panel.set_records(records);
                        Task::none()
                    }
                    None => Self::reload_records(),
                }
            }
            E::Looked(summary) => {
                for (record, end) in &summary.ends {
                    if matches!(end, trading::LookupEnd::NotPlaced) {
                        self.trading.entry_gone(&record.client_id);
                    }
                    self.trading.lookup_finished(record, *end);
                    let (text, kind) = account_flow::describe_lookup(&record.symbol, *end);
                    self.say(text, kind);
                }
                match summary.remaining {
                    Some(records) => {
                        self.account_panel.set_records(records);
                        Task::none()
                    }
                    None => Self::reload_records(),
                }
            }
        }
    }

    /// The one click that sends: checks the gate once more, takes the intent out of the
    /// confirmation (once) and starts the send in the background.
    fn confirm_order(&mut self) -> Task<Message> {
        use modal::order_ticket as ticket;
        let Some(conf) = self.order_confirm.as_ref() else {
            return Task::none();
        };
        let request = conf.request.clone();
        let origin = conf.window;
        let source = conf.source;
        if self.trading.bulk_running(request.account) {
            self.order_confirm = None;
            self.order_ticket.set_status(
                "A cancel all or close all of this account is running. Review the order again \
                 after it."
                    .to_string(),
                BannerKind::Warning,
            );
            return Task::none();
        }
        let accounts = self.exchange_accounts.persisted().accounts;
        let meta = accounts.iter().find(|m| m.id == request.account).cloned();
        let signed = self.signed_client_for_current_proxy();
        let gate = ticket::gate_for(
            meta.as_ref(),
            ticket::kind_of(&request.spec),
            &request.symbol,
            &self.trading,
            &self.trade_session,
            signed.is_ok(),
            false,
        );
        let risk = self.trading.risk_for(request.account);
        let risk_missing = risk.is_none();
        let (Some(meta), Ok(signed), Some(risk), None) =
            (meta, signed, risk, ticket::blocked_reason(&gate))
        else {
            self.order_confirm = None;
            let why = ticket::blocked_reason(&gate)
                .unwrap_or(if risk_missing {
                    "The account has no running trading state. Test it again in Exchange accounts."
                } else {
                    "The order can no longer be sent."
                })
                .to_string();
            self.order_ticket.set_status(why, BannerKind::Error);
            return Task::none();
        };
        // A cancel opened from a stray-order button is judged again now: the lists must be fresh
        // and the order still one that is offered for cancelling.
        if let exchange::trade::ManualSpec::Cancel { id, .. } = &request.spec
            && self.trading.has_stray_mark(meta.id, id)
            && !self.trading.is_stray(meta.id, id)
        {
            self.trading.take_stray_mark(meta.id, id);
            self.order_confirm = None;
            self.order_ticket.set_status(
                "That order is no longer one to cancel, or the lists may be out of date. Nothing \
                 was sent. Review the account panel again."
                    .to_string(),
                BannerKind::Warning,
            );
            return Task::none();
        }
        let Some(conf) = self.order_confirm.as_mut() else {
            return Task::none();
        };
        // What the confirmation listed, read before the intent leaves it.
        let attach = conf.attach_plan().cloned();
        let mode = self.trade_session.get(meta.id).and_then(|e| e.actual_mode);
        // N1: the positions may have changed since the preview. The protection was listed for a
        // new entry; when the account now holds the other way (or cannot tell), nothing is sent
        // and the order is reviewed again.
        if let Some(plan) = &attach {
            let held = self.trading.holdings(meta.id, &request.symbol);
            if protect_attach::check_new_entry(meta.product, mode, plan.dir, &held).is_err() {
                self.order_confirm = None;
                self.order_ticket.set_status(
                    "The positions of the account changed after the review, so the protection \
                     listed no longer fits. Nothing was sent. Review the order again."
                        .to_string(),
                    BannerKind::Warning,
                );
                return Task::none();
            }
            // M3: one entry at a time per symbol while its automatic protection is at work.
            if plan.kind == protect_attach::AttachKind::AfterFill
                && self.trading.auto_active(meta.id, &request.symbol)
            {
                self.order_confirm = None;
                self.order_ticket.set_status(
                    protect_attach::AUTO_STILL_ACTIVE.to_string(),
                    BannerKind::Warning,
                );
                return Task::none();
            }
        }
        let Some(conf) = self.order_confirm.as_mut() else {
            return Task::none();
        };
        let Some((token, intent)) = conf.confirm(std::time::Instant::now()) else {
            return Task::none();
        };
        self.order_confirm = None;
        if let exchange::trade::ManualSpec::Cancel { id, .. } = &request.spec {
            self.trading.take_stray_mark(meta.id, id);
        }
        // Remembered for this session only: an order of the app's shape with an id that is not
        // in this list may be left over from an earlier one (d5).
        self.trading.note_sent_ids(&intent.client_ids());
        // The entry's own client id (the first one of the request), kept in memory only.
        if let (Some(plan), Some(entry_id)) = (attach, intent.client_ids().first()) {
            // Binance futures protection follows the fill: the grant is made from this very
            // intent. If it cannot be made, the entry is not sent without its protection.
            let auto = if plan.kind == protect_attach::AttachKind::AfterFill {
                // The position mode decides which protective orders are valid; an account
                // whose mode was not read is not guessed at.
                let Some(position_mode) = mode else {
                    self.order_ticket.set_status(
                        "The position mode of the account is not known, so the protective orders \
                         cannot be planned. Nothing was sent. Test the account again."
                            .to_string(),
                        BannerKind::Error,
                    );
                    return Task::none();
                };
                match auto_flow::mint(
                    &intent,
                    request.account,
                    &request.symbol,
                    &plan.spec,
                    position_mode,
                    chrono::Utc::now().timestamp_millis(),
                ) {
                    Ok(init) => Some(init),
                    Err(why) => {
                        self.order_ticket
                            .set_status(format!("{why} Nothing was sent."), BannerKind::Error);
                        return Task::none();
                    }
                }
            } else {
                None
            };
            let pending = protect_attach::PendingAttach {
                account: request.account,
                symbol: request.symbol.clone(),
                plan,
            };
            let listed = match auto {
                Some(init) => self
                    .trading
                    .register_attach((*entry_id).to_string(), pending, init),
                None => self
                    .trading
                    .register_okx_attach((*entry_id).to_string(), pending),
            };
            // Fail closed: the confirmation said this entry would be protected.
            if !listed {
                self.order_ticket.set_status(
                    "The protection could not be listed for this entry (another send for the \
                     symbol is in flight). Nothing was sent. Review the order again."
                        .to_string(),
                    BannerKind::Error,
                );
                return Task::none();
            }
        }
        self.trading.begin_send(request.account, &request.symbol);
        self.order_ticket
            .set_status("Sending…".to_string(), BannerKind::Normal);
        let input = order_flow::SendInput {
            session: self.trade_session.get(meta.id).cloned(),
            symbol: request.symbol.clone(),
            meta,
            signed,
            proxy: self.trading.proxy_reader(),
            risk,
            token,
            intent,
        };
        self.order_requests.insert(
            (request.account, request.symbol.clone()),
            (request, origin, source),
        );
        Task::perform(order_flow::send(input), Message::OrderSent)
    }

    /// The last price the ticker list holds for the symbol of the account, with its age (the
    /// time since the list of its venue was last fetched successfully). `None` when the list has
    /// no price or was never fetched.
    fn quote_of(
        &self,
        meta: &data::config::exchange_accounts::AccountMeta,
        symbol: &str,
    ) -> Option<(exchange::trade::Decimal, std::time::Duration)> {
        if !modal::order_ticket::valid_symbol(symbol) {
            return None;
        }
        let market = order_flow::market_of(meta.venue, meta.product);
        let ticker = exchange::Ticker::new(symbol, market);
        let price = self
            .sidebar
            .tickers_table
            .mark_price(&ticker)
            .and_then(order_flow::price_to_decimal)?;
        let fetched = self.quote_stamps.get(&market.venue())?;
        Some((price, fetched.elapsed()))
    }

    /// Start the automatic orders that are due (protective orders after a fill, the cancel of
    /// the other one). Each job is sent once; a failure is written down, never retried.
    fn run_auto_jobs(&mut self) -> Task<Message> {
        let task = self.start_auto_jobs();
        // Failures found while polling (or recorded by the last result) are told once.
        self.notify_failures();
        self.notify_bare();
        task
    }

    fn start_auto_jobs(&mut self) -> Task<Message> {
        let accounts = self.exchange_accounts.persisted().accounts;
        // Read again at the moment of sending (S3), not only when the entry was confirmed.
        let switch = |id: uuid::Uuid| {
            accounts
                .iter()
                .find(|m| m.id == id)
                .is_some_and(|m| m.protect.attaches_automatically(m.env))
        };
        let now = chrono::Utc::now().timestamp_millis();
        let jobs = self
            .trading
            .poll_auto(now, self.started.elapsed().as_millis() as i64, &switch);
        if jobs.is_empty() {
            return Task::none();
        }
        let signed = self.signed_client_for_current_proxy();
        let mut tasks = Vec::new();
        for job in jobs {
            let meta = accounts.iter().find(|m| m.id == job.account).cloned();
            let risk = self.trading.risk_for(job.account);
            let (Some(meta), Ok(signed), Some(risk)) = (meta, signed.clone(), risk) else {
                let summary = auto_flow::failed_summary(
                    &job,
                    auto_flow::FailReason::SetupFailed,
                    "the account has no usable connection or trading state",
                );
                self.trading.auto_done(summary);
                continue;
            };
            // The price the triggers are checked against, and how old it is. A quote that is too
            // old is not used: the orders of the stage are not sent and the failure goes to the
            // panel (nothing is sent again by itself).
            let quote = self.quote_of(&meta, &job.symbol);
            let ref_price = if job.places() {
                match auto_flow::reference_price(job.trigger_basis(), quote) {
                    Ok(price) => price,
                    Err(reason) => {
                        let summary =
                            auto_flow::failed_summary(&job, reason, "the market price is too old");
                        self.trading.auto_done(summary);
                        continue;
                    }
                }
            } else {
                None
            };
            let input = auto_flow::RunInput {
                session: self.trade_session.get(meta.id).cloned(),
                job,
                meta,
                signed,
                proxy: self.trading.proxy_reader(),
                risk,
                ref_price,
            };
            tasks.push(Task::perform(auto_flow::run(input), |s| {
                Message::AutoDone(Box::new(s))
            }));
        }
        Task::batch(tasks)
    }

    /// A stop or target line was dragged to a price (f3a2). The account is the pane's, by the
    /// same rule as a click on the ladder; the order is read again from the account's list, and
    /// the new order is built from that row. Only the review opens here: nothing is sent before
    /// the confirmation, and a confirmation checks everything again.
    fn start_replace_request(
        &mut self,
        req: replace_flow::PaneReplace,
        window: window::Id,
    ) -> Task<Message> {
        let warn = |this: &mut Self, text: String| this.notifications.push(Toast::warn(text));
        if self.confirm_open() || self.order_ticket.is_preparing() {
            warn(
                self,
                "Another order is open or being prepared. Finish it first.".to_string(),
            );
            return Task::none();
        }
        let live_open = data::config::trade_gate::live_sending_enabled();
        let saved = self.exchange_accounts.persisted().accounts;
        let choices: Vec<_> = saved.iter().map(|m| self.account_choice(m)).collect();
        let pick = match order_flow::resolve_pane_account(
            req.market,
            self.order_ticket.account(),
            &choices,
            live_open,
        ) {
            Ok(p) => p,
            Err(e) => {
                warn(self, e.message().to_string());
                return Task::none();
            }
        };
        let Some(meta) = saved.iter().find(|m| m.id == pick.id) else {
            warn(self, "The account no longer exists.".to_string());
            return Task::none();
        };
        let Some(guard) = self.trading.replace_guard(meta.id, live_open) else {
            warn(
                self,
                "The account has no running trading state.".to_string(),
            );
            return Task::none();
        };
        // The latest read of the order, never the line's own copy.
        let Some(row) = self
            .trading
            .open_orders(meta.id)
            .iter()
            .find(|o| o.id == req.order_id && o.symbol == req.symbol)
            .cloned()
        else {
            warn(self, "That order is no longer open.".to_string());
            return Task::none();
        };
        let auto = self.trading.is_auto_order(meta.id, &row);
        let plan = match replace_flow::plan(&row, req.price, &guard, auto) {
            Ok(plan) => plan,
            // The same price as the order's own: no action.
            Err(replace_flow::PlanRefusal::Unchanged) => return Task::none(),
            Err(why) => {
                warn(self, why.text().to_string());
                return Task::none();
            }
        };
        let task = self.start_review_of(
            order_flow::PrepareRequest {
                account: meta.id,
                symbol: req.symbol,
                spec: plan.spec,
                protect: None,
            },
            window,
            order_flow::OrderSource::Replace,
            Some(row),
        );
        if !self.order_ticket.is_preparing() {
            // `start_review_of` refused and said why on the ticket: the drag needs a toast too.
            self.replace_review = false;
            warn(
                self,
                "The replace could not be prepared. See the order ticket for the reason."
                    .to_string(),
            );
        }
        task
    }

    /// Confirm of a replace (f3a2): the conditions are judged again on the latest lists, the
    /// grant is made from the row read now, and one detached task cancels the old order and
    /// places the new one (`order_flow::run_replace`).
    fn confirm_replace(&mut self) -> Task<Message> {
        use modal::order_ticket as ticket;
        let Some(conf) = self.order_confirm.as_ref() else {
            return Task::none();
        };
        let Some(info) = conf.replace_info().cloned() else {
            return Task::none();
        };
        let request = conf.request.clone();
        // Judged on the latest lists: the order must still be there and unchanged, and every
        // condition that made it draggable must still hold.
        let stop = |this: &mut Self, text: String| {
            this.order_confirm = None;
            this.notifications.push(Toast::warn(text.clone()));
            this.order_ticket.set_status(text, BannerKind::Warning);
        };
        if self.trading.bulk_running(request.account) {
            stop(
                self,
                "A cancel all or close all of this account is running. Nothing was sent."
                    .to_string(),
            );
            return Task::none();
        }
        let accounts = self.exchange_accounts.persisted().accounts;
        let meta = accounts.iter().find(|m| m.id == request.account).cloned();
        let signed = self.signed_client_for_current_proxy();
        let gate = ticket::gate_for(
            meta.as_ref(),
            ticket::kind_of(&request.spec),
            &request.symbol,
            &self.trading,
            &self.trade_session,
            signed.is_ok(),
            false,
        );
        let risk = self.trading.risk_for(request.account);
        let (Some(meta), Ok(signed), Some(risk), None) =
            (meta, signed, risk, ticket::blocked_reason(&gate))
        else {
            let why = ticket::blocked_reason(&gate)
                .unwrap_or("The order can no longer be sent.")
                .to_string();
            stop(self, format!("{why} Nothing was sent."));
            return Task::none();
        };
        let live_open = data::config::trade_gate::live_sending_enabled();
        let Some(guard) = self.trading.replace_guard(meta.id, live_open) else {
            stop(
                self,
                "The account has no running trading state. Nothing was sent.".to_string(),
            );
            return Task::none();
        };
        let latest = replace_flow::recheck(
            &info.old,
            self.trading.open_orders(meta.id),
            &guard,
            |row| self.trading.is_auto_order(meta.id, row),
        )
        .cloned();
        let old = match latest {
            Ok(row) => row,
            Err(why) => {
                stop(self, why.text().to_string());
                return Task::none();
            }
        };
        let Some(conf) = self.order_confirm.as_mut() else {
            return Task::none();
        };
        // The delay of a fresh confirmation and the single use: a click that does not count
        // leaves the window as it is.
        let Some((token, intent, info)) = conf.confirm_replace(std::time::Instant::now()) else {
            return Task::none();
        };
        self.order_confirm = None;
        if !self.trading.replace_flow_mut().begin_send(meta.id, &old.id) {
            self.notifications.push(Toast::warn(
                "Another replace of this account is running. Nothing was sent.".to_string(),
            ));
            return Task::none();
        }
        // The symbol is in flight like any send: no other order is prepared on it meanwhile.
        self.trading.begin_send(request.account, &request.symbol);
        self.trading.note_sent_ids(&intent.client_ids());
        self.trading.replace_begun(meta.id, &old);
        let input = order_flow::ReplaceSendInput {
            session: self.trade_session.get(meta.id).cloned(),
            symbol: request.symbol,
            meta,
            signed,
            proxy: self.trading.proxy_reader(),
            risk,
            token,
            intent,
            old,
            kind: info.kind,
            dir: info.dir,
            old_trigger: info.old_trigger,
            new_trigger: info.new_trigger,
        };
        Task::perform(order_flow::run_replace(input), Message::ReplaceSent)
    }

    /// The end of a replace: the line moves (the lists are read again), or the old line stays,
    /// or a red banner says the position may have lost the order.
    fn on_replace_sent(&mut self, summary: replace_flow::ReplaceSummary) -> Task<Message> {
        use replace_flow::Level;
        self.trading
            .replace_flow_mut()
            .finish_send(summary.account, &summary.old_id);
        // Whatever the result, the hold-back on the old order ends before anything is adopted.
        self.trading.replace_ended(summary.account);
        // Only a result that stayed unknown after the lookups keeps the symbol blocked: a new
        // order of unknown fate, or a task that ended with no answer (either leg may have gone
        // out), as `Unresolved` does for a plain send.
        let unresolved = self.trading.on_replace_result(&summary);
        // A completed replace of an order the automatic protection placed: it follows the new
        // order from now on (a result that is not `Done` carries no proof, and adopts nothing).
        if let Some(replacement) = &summary.replacement {
            // An order of an entry that already left the book cannot be followed: say so, the
            // new order stands on the exchange without the app's automatic protection.
            if self.trading.adopt_replacement(summary.account, replacement)
                == crate::auto_flow::Adoption::Dropped
            {
                self.notifications.push(Toast::warn(
                    "The new order is no longer followed by the automatic protection. Check \
                     the open orders. If the position is already closed, cancel this order."
                        .to_string(),
                ));
            }
        }
        let handling = replace_flow::handle(&summary);
        // Both legs go to the log, in fixed words (no order id, no exchange text).
        log::info!("replace protective order ended: {}", summary.result.tag());
        // The order lists are not marked here. While the stream is connected every algo order
        // change arrives as an event, and the event marks the lists and reads them again; a
        // result that changed nothing (cancel refused or not sent) sends no event, so a mark
        // set here would stay and switch off the bare-position watch and Retry protection.
        let toast = match handling.level {
            Level::Info => Toast::info(handling.text.clone()),
            Level::Warn => Toast::warn(handling.text.clone()),
            Level::Error => Toast::error(handling.text.clone()),
        };
        self.notifications.push(toast);
        self.order_ticket.set_status(
            handling.text.clone(),
            match handling.level {
                Level::Info => BannerKind::Normal,
                Level::Warn => BannerKind::Warning,
                Level::Error => BannerKind::Error,
            },
        );
        if let Some(reason) = handling.failure {
            self.trading
                .replace_flow_mut()
                .record_failure(&summary, reason);
        }
        if handling.notify_os {
            self.notify_failures();
        }
        // An unknown new order was saved: the unknown list is read again.
        let task = if unresolved {
            Self::reload_records()
        } else {
            Task::none()
        };
        Task::batch([task, self.run_auto_jobs()])
    }

    fn on_order_sent(&mut self, summary: order_flow::SendSummary) -> Task<Message> {
        let task = self.on_order_sent_inner(summary);
        Task::batch([task, self.run_auto_jobs()])
    }

    fn on_order_sent_inner(&mut self, summary: order_flow::SendSummary) -> Task<Message> {
        // Only an unknown result keeps the symbol blocked; it is never erased by a later send.
        self.trading.finish_send(
            summary.account,
            &summary.symbol,
            summary.pair_after().is_some(),
        );
        if summary.journal_failed {
            self.trading.set_journal_failed();
        }
        // The protection listed for the entry stays or goes with the result.
        self.trading
            .settle_attach(summary.account, &summary.symbol, &summary.result);
        // The request of this very send (account and symbol), never another one's.
        let (request, origin, source) = self
            .order_requests
            .remove(&(summary.account, summary.symbol.clone()))
            .map_or((None, None, None), |(r, o, s)| (Some(r), Some(o), Some(s)));
        // A whole-position stop was accepted: the entry's failures that left the position bare
        // are cleared and its own stop is never sent. Other results keep the warning.
        if let (
            order_flow::SendResult::Accepted { .. },
            Some(order_flow::PrepareRequest {
                spec:
                    exchange::trade::ManualSpec::Stop {
                        dir,
                        size: exchange::trade::Size::ClosePosition,
                        ..
                    },
                ..
            }),
        ) = (&summary.result, &request)
        {
            self.trading
                .protection_added(summary.account, &summary.symbol, *dir);
        }
        if summary.result == order_flow::SendResult::RulesExpired
            && let (Some(request), Some(origin), Some(source)) = (request, origin, source)
        {
            // Not a failure: the data aged between the preview and the click. A new preview
            // is made only when nothing else is open or being prepared; it would replace or
            // outdate that other order (M2). Otherwise the user is told to review again.
            if order_flow::may_review_again(self.confirm_open(), self.order_ticket.is_preparing()) {
                let task = self.start_review(request, self.live_origin(origin), source);
                if self.order_ticket.is_preparing() {
                    self.order_ticket.set_status(
                        "The trading data expired. Review the order again.".to_string(),
                        BannerKind::Warning,
                    );
                }
                return task;
            }
            self.order_ticket.set_status(
                format!(
                    "The trading data of the {} order expired. Review it again.",
                    summary.symbol
                ),
                BannerKind::Warning,
            );
            return Task::none();
        }
        let line = order_flow::describe_result(&summary.result, summary.client_tail.as_deref());
        // With the ticket closed nobody sees its banner: say it in a toast as well.
        if self.sidebar.active_menu() != Some(sidebar::Menu::OrderTicket) {
            use order_flow::SendResult as R;
            let text = format!("{}: {line}", summary.symbol);
            self.notifications.push(match summary.result {
                R::Accepted { .. } | R::Found => Toast::info(text),
                R::NotPlacedPresumed | R::RulesExpired | R::ConfirmationMismatch => {
                    Toast::warn(text)
                }
                _ => Toast::error(text),
            });
        }
        self.order_ticket.show_result(&summary, line);
        // An unknown result was saved: the unknown list is read again.
        if summary.pair_after().is_some() {
            return Self::reload_records();
        }
        Task::none()
    }

    fn new() -> (Self, Task<Message>) {
        let saved_state = layout::load_saved_state();

        let (main_window_id, open_main_window) = {
            let (position, size) = saved_state.window();
            let config = window::Settings {
                size,
                position,
                exit_on_close_request: false,
                ..window::settings()
            };
            window::open(config)
        };

        let data_sources = Arc::new(DataSources::new(
            &saved_state.network,
            saved_state.proxy_unreadable,
        ));

        let (sidebar, launch_sidebar) = screen::sidebar::Sidebar::new(
            &saved_state.sidebar,
            saved_state.cache_market_metadata,
            data_sources.exchange.clone(),
        );

        let (audio_stream, audio_init_err) = AudioStream::new(saved_state.audio_cfg);

        let mut state = Self {
            main_window: window::Window::new(main_window_id),
            layout_manager: saved_state.layout_manager,
            theme_editor: ThemeEditor::new(saved_state.custom_theme),
            indicator_editor: IndicatorEditor::new(),
            window_scale_factors: window::WindowScaleFactors::new(main_window_id),
            audio_stream,
            sidebar,
            data_sources,
            timezone: saved_state.timezone,
            ui_scale_factor: saved_state.scale_factor,
            volume_size_unit: saved_state.volume_size_unit,
            applied_volume_size_unit: saved_state.volume_size_unit,
            theme: ThemeState::new(saved_state.theme),
            notifications: Notifications::new(),
            close_warned_at: None,
            network_config: saved_state.network.clone(),
            proxy_unreadable: saved_state.proxy_unreadable,
            trade_session: TradeSession::default(),
            trading: trading::TradingState::new(if saved_state.proxy_unreadable {
                ProxyState::Broken
            } else {
                ProxyState::from_config(saved_state.network.proxy.as_ref())
            }),
            network_editor: NetworkEditor::new(&saved_state.network, None),
            exchange_accounts: modal::exchange_accounts::ExchangeAccounts::new(
                &saved_state.exchange_accounts,
            ),
            order_ticket: modal::order_ticket::OrderTicket::default(),
            trade_overlay: dashboard::TradeOverlay::default(),
            order_confirm: None,
            account_panel: modal::account_panel::AccountPanel::default(),
            bulk_confirm: None,
            order_requests: HashMap::new(),
            replace_review: false,
            quote_stamps: HashMap::new(),
            started: std::time::Instant::now(),
            antialiasing: saved_state.antialiasing,
            applied_antialiasing: saved_state.antialiasing,
            restoring_windows: HashSet::new(),
            tab_drag: None,
        };

        for diagnostic in data::chart::kline::indicators::catalog().diagnostics() {
            state.notifications.push(Toast::warn(format!(
                "Indicator source quarantined: {diagnostic}"
            )));
        }

        if let Some(err) = audio_init_err {
            state
                .notifications
                .push(Toast::error(format!("Audio disabled: {err}")));
        }

        if state.layout_manager.layouts.is_empty() {
            log::error!("No layouts available after loading state; creating a default layout");
            state.layout_manager.ensure_layout();
        }

        // Must run before the active layout is chosen: it keeps the active layout attached.
        let restore_detached = state.restore_detached_windows();

        let active_layout_id = state
            .layout_manager
            .active_layout_id()
            .or_else(|| {
                state
                    .layout_manager
                    .layouts
                    .first()
                    .map(|layout| &layout.id)
            })
            .map(|layout| layout.unique);

        let load_layout = active_layout_id
            .map(|uid| state.load_layout(uid, main_window_id))
            .unwrap_or_else(|| {
                log::error!("No active layout could be selected at startup");
                Task::none()
            });

        let open_main_window = open_main_window.then(|window_id| {
            window::scale_factor(window_id, |window, scale_factor| {
                Message::WindowScaleFactorUpdated {
                    window,
                    scale_factor,
                }
            })
        });

        (
            state,
            Task::batch([
                open_main_window
                    .chain(load_layout)
                    .chain(restore_detached)
                    .chain(launch_sidebar.map(Message::Sidebar)),
                // Locks the saved unresolved sends before any private stream can start. It runs
                // beside the rest so a slow sidebar fetch cannot hold it back; no order can be
                // prepared or sent until it is applied.
                Task::perform(trading::boot(), |report| {
                    Message::Trading(trading::Message::Booted(report))
                }),
            ]),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        // Market data cannot change an account, and it is the hot path: no refresh for it.
        let market = matches!(message, Message::MarketWsEvent(_));
        let task = self.update_message(message);
        // The panes read accounts, session and the ticket's account from here.
        if !market {
            self.refresh_trade_overlay();
        }
        task
    }

    fn update_message(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::MarketWsEvent(event) => {
                // Detached layouts keep running, so every visible layout gets the event.
                let visible = self.layout_manager.visible_ids();
                if visible.is_empty() {
                    return Task::none();
                }

                match &event {
                    exchange::Event::Connected(_streams) => {}
                    exchange::Event::Disconnected(_streams, reason) => {
                        log::info!("a stream disconnected from WS: {reason:?}");
                    }
                    exchange::Event::TradesReceived(stream, _, buffer) => {
                        if let Some(msg) = self.audio_stream.try_play_sound(stream, buffer) {
                            self.notifications.push(Toast::error(msg));
                        }
                    }
                    exchange::Event::DepthReceived(..) | exchange::Event::KlineReceived(..) => {}
                }

                let mut tasks = vec![];
                for layout_id in visible {
                    let window_id = self.layout_window_id(layout_id);
                    let Some(dashboard) = self.layout_manager.mut_dashboard(layout_id) else {
                        continue;
                    };

                    let task = match &event {
                        exchange::Event::Connected(_) | exchange::Event::Disconnected(..) => {
                            continue;
                        }
                        exchange::Event::DepthReceived(stream, update_t, depth) => {
                            dashboard.ingest_live_depth(stream, *update_t, depth.clone(), window_id)
                        }
                        exchange::Event::TradesReceived(stream, update_t, buffer) => {
                            dashboard.ingest_live_trades(stream, buffer, *update_t, window_id)
                        }
                        exchange::Event::KlineReceived(stream, kline) => {
                            dashboard.ingest_live_kline(stream, kline, window_id)
                        }
                    };
                    tasks.push(self.map_dashboard_task(task, Some(layout_id)));
                }
                return Task::batch(tasks);
            }
            Message::Tick(now) => {
                let main_window_id = self.main_window.id;
                let data_sources = Arc::clone(&self.data_sources);
                let visible = self.layout_manager.visible_ids();
                if visible.is_empty() {
                    return Task::none();
                }

                self.layout_manager.sync_drawings(main_window_id);

                let mut tasks = vec![];
                for layout_id in visible {
                    let window_id = self.layout_window_id(layout_id);
                    let Some(dashboard) = self.layout_manager.mut_dashboard(layout_id) else {
                        continue;
                    };
                    let task = dashboard.tick(now, &data_sources, window_id);
                    tasks.push(self.map_dashboard_task(task, Some(layout_id)));
                }
                return Task::batch(tasks);
            }
            Message::WindowEvent(event) => return self.handle_window_event(event),
            Message::WindowScaleFactorUpdated {
                window,
                scale_factor,
            } => {
                self.window_scale_factors.set(window, scale_factor);
            }
            Message::ExitRequested(windows) => {
                self.save_state_to_disk(&windows);
                return iced::exit();
            }
            Message::SaveStateRequested(windows) => {
                self.save_state_to_disk(&windows);
            }
            Message::GoBack => {
                let main_window = self.main_window.id;
                if self.layout_manager.closing().is_some() {
                    let _ = self
                        .layout_manager
                        .update(modal::layout_manager::Message::CancelClose);
                    return Task::none();
                }

                // Escape closes the confirmation first, and never confirms.
                if self.confirm_open() {
                    self.order_confirm = None;
                    self.bulk_confirm = None;
                    self.trading.clear_stray_marks();
                    return Task::none();
                }

                if self.sidebar.active_menu().is_some() {
                    self.discard_order();
                    self.exchange_accounts.clear_form();
                    self.sidebar.set_menu(None);
                } else {
                    if self.active_dashboard_mut().go_back(main_window) {
                        return Task::none();
                    } else if self.active_dashboard().focus.is_some() {
                        self.active_dashboard_mut().clear_focus();
                    } else {
                        self.sidebar.hide_tickers_table();
                    }
                }
            }
            Message::ThemeSelected(theme) => {
                self.theme_editor.reset_input();
                self.replace_theme(theme);
            }
            Message::Dashboard {
                layout_id: id,
                event: msg,
            } => {
                let Some(active_layout) = self.layout_manager.active_layout_id() else {
                    log::error!("No active layout to handle dashboard message");
                    return Task::none();
                };

                let active_layout_id = active_layout.unique;
                let layout_id = id.unwrap_or(active_layout_id);

                // A detached layout treats its own window as its main window.
                let main_window = match self.layout_manager.detached_window(layout_id) {
                    Some(id) => window::Window::new(id),
                    None => self.main_window,
                };

                // Panes of a detached layout cannot be popped out.
                if self.layout_manager.is_detached(layout_id)
                    && matches!(
                        msg,
                        dashboard::Message::Pane(_, dashboard::pane::Message::Popout)
                    )
                {
                    return Task::none();
                }

                let Some((main_task, event)) = self.layout_manager.update_dashboard(
                    msg,
                    &main_window,
                    &layout_id,
                    &self.data_sources,
                ) else {
                    return Task::none();
                };

                let additional_task = match event {
                    Some(dashboard::Event::OpenUrlRequested(url)) => {
                        self.open_url(&url);
                        Task::none()
                    }
                    Some(dashboard::Event::Notification(toast)) => {
                        self.notifications.push(toast);
                        Task::none()
                    }
                    Some(dashboard::Event::ResolveStreams { pane_id, streams }) => {
                        let tickers_info = self.sidebar.tickers_info();

                        let resolved_streams =
                            streams.into_iter().try_fold(vec![], |mut acc, persist| {
                                let resolver =
                                    |t: &exchange::Ticker| tickers_info.get(t).and_then(|opt| *opt);

                                match persist.into_stream_kinds(resolver) {
                                    Ok(mut resolved) => {
                                        acc.append(&mut resolved);
                                        Ok(acc)
                                    }
                                    Err(err) => Err(err),
                                }
                            });

                        match resolved_streams {
                            Ok(resolved) => {
                                if resolved.is_empty() {
                                    Task::none()
                                } else {
                                    let task = self
                                        .layout_manager
                                        .mut_dashboard(layout_id)
                                        .map(|dashboard| {
                                            dashboard.resolve_streams(
                                                main_window.id,
                                                pane_id,
                                                resolved,
                                            )
                                        })
                                        .unwrap_or_else(Task::none);
                                    self.map_dashboard_task(task, Some(layout_id))
                                }
                            }
                            Err(err) => {
                                if self.sidebar.is_metadata_loading() {
                                    // Metadata fetches are still in flight
                                    log::debug!(
                                        "Deferring stream resolution for pane {pane_id}: metadata still loading ({err})"
                                    );
                                } else if let Some(dashboard) =
                                    self.layout_manager.mut_dashboard(layout_id)
                                {
                                    log::debug!("Blocking streams for pane {pane_id}: {err}");
                                    dashboard.block_streams(
                                        main_window.id,
                                        pane_id,
                                        format!("Metadata not available: {err}"),
                                    );
                                }
                                Task::none()
                            }
                        }
                    }
                    Some(dashboard::Event::OpenNetworkSettings) => {
                        return self.update(Message::Sidebar(
                            screen::sidebar::Message::ToggleSidebarMenu(Some(
                                sidebar::Menu::Network,
                            )),
                        ));
                    }
                    Some(dashboard::Event::RequestPalette) => {
                        let theme = self.theme.current().clone();

                        if let Some(dashboard) = self.layout_manager.mut_dashboard(layout_id) {
                            dashboard.theme_updated(main_window.id, &theme);
                        }

                        Task::none()
                    }
                    Some(dashboard::Event::PresetCommand {
                        window,
                        pane,
                        command,
                    }) => {
                        self.layout_manager.apply_preset_command(
                            main_window.id,
                            layout_id,
                            window,
                            pane,
                            command,
                        );
                        Task::none()
                    }
                    Some(dashboard::Event::IndicatorCatalog(message)) => {
                        self.update_shared_chart_resources(message)
                    }
                    Some(dashboard::Event::TradeRequest { window, order }) => {
                        self.start_trade_request(order, window)
                    }
                    Some(dashboard::Event::ReplaceRequest { window, request }) => {
                        self.start_replace_request(request, window)
                    }
                    None => Task::none(),
                };

                return self
                    .map_dashboard_task(main_task, Some(layout_id))
                    .chain(additional_task);
            }
            Message::RemoveNotification(index) => {
                self.notifications.remove(index);
            }
            Message::SetTimezone(tz) => {
                self.timezone = tz;
            }
            Message::SetMetadataCaching(enabled) => {
                self.sidebar.set_cache_enabled(enabled);
            }
            Message::RefreshMetadata => {
                if let Some(task) = self.sidebar.force_refresh_metadata() {
                    return task.map(Message::Sidebar);
                }
            }
            Message::SharedChartResources(message) => {
                return self.update_shared_chart_resources(message);
            }
            Message::ScaleFactorChanged(value) => {
                self.ui_scale_factor = value;
            }
            Message::Layouts(message) => {
                // `SelectActive` switches the active layout before `load_layout` runs, so
                // publish the outgoing layout's drawing edits first.
                self.layout_manager.sync_drawings(self.main_window.id);
                let action = self.layout_manager.update(message);

                match action {
                    Some(modal::layout_manager::Action::Select(layout)) => {
                        let active_popout_keys = self
                            .active_dashboard()
                            .popout
                            .keys()
                            .copied()
                            .collect::<Vec<_>>();

                        // The popouts close below: a confirmation open in one of them goes with
                        // them now, so it cannot block anything while the windows are closing.
                        // Their scale factors are removed by the `Closed` events.
                        if order_flow::confirm_lost_with_any(
                            self.order_confirm.as_ref().map(|c| c.window),
                            self.bulk_confirm.as_ref().map(|c| c.window),
                            &active_popout_keys,
                        ) {
                            self.discard_order();
                        }

                        let window_tasks = Task::batch(
                            active_popout_keys
                                .iter()
                                .map(|&popout_id| window::close::<window::Id>(popout_id))
                                .collect::<Vec<_>>(),
                        )
                        .discard();

                        let old_layout_id = self
                            .layout_manager
                            .active_layout_id()
                            .map(|layout| layout.unique)
                            .expect("No active layout");

                        return self
                            .map_dashboard_task(
                                window::collect_window_specs(
                                    active_popout_keys,
                                    dashboard::Message::SavePopoutSpecs,
                                ),
                                Some(old_layout_id),
                            )
                            .chain(window_tasks)
                            .chain(self.load_layout(layout, self.main_window.id));
                    }
                    Some(modal::layout_manager::Action::Apply { name, dashboard }) => {
                        let unique = self.add_layout_from_config(&name, *dashboard);
                        return self.update(Message::Layouts(
                            modal::layout_manager::Message::SelectActive(unique),
                        ));
                    }
                    Some(modal::layout_manager::Action::Clone(id)) => {
                        let manager = &mut self.layout_manager;

                        let source_data = manager.get(id).map(|layout| {
                            (
                                layout.id.name.clone(),
                                data::Dashboard::from(&layout.dashboard),
                            )
                        });

                        if let Some((name, ser_dashboard)) = source_data {
                            self.add_layout_from_config(&name, ser_dashboard);
                        }
                    }
                    Some(modal::layout_manager::Action::Close { select, remove }) => {
                        let popouts = self
                            .active_dashboard()
                            .popout
                            .keys()
                            .copied()
                            .collect::<Vec<_>>();
                        for popout in &popouts {
                            self.forget_window(*popout);
                        }
                        let close_popouts = Task::batch(
                            popouts
                                .into_iter()
                                .map(window::close::<window::Id>)
                                .collect::<Vec<_>>(),
                        )
                        .discard();

                        let load = self.load_layout(select, self.main_window.id);
                        self.layout_manager
                            .layouts
                            .retain(|layout| layout.id.unique != remove);

                        return close_popouts.chain(load);
                    }
                    Some(modal::layout_manager::Action::TabGrabbed(id)) => {
                        self.tab_drag = Some(TabDrag {
                            layout: id,
                            cursor: None,
                        });
                    }
                    Some(modal::layout_manager::Action::Detach(id)) => {
                        return self.detach_layout(id, None);
                    }
                    Some(modal::layout_manager::Action::DetachRefused) => {
                        self.notifications.push(Toast::info(
                            "At least one layout must stay in the main window".to_string(),
                        ));
                    }
                    Some(modal::layout_manager::Action::CloseWindow(window_id)) => {
                        self.forget_window(window_id);
                        return window::close(window_id);
                    }
                    None => {}
                }
            }
            Message::TabDragMoved(window_id, position) => {
                if window_id == self.main_window.id
                    && let Some(drag) = self.tab_drag.as_mut()
                {
                    drag.cursor = Some(position);
                }
            }
            Message::TabDragReleased => {
                let Some(drag) = self.tab_drag.take() else {
                    return Task::none();
                };
                // Dropped on the tab bar: default placement. Dropped below it: at the cursor.
                let position = drag
                    .cursor
                    .filter(|cursor| cursor.y > TAB_DROP_MIN_Y)
                    .zip(self.main_window.position)
                    .map(|(cursor, origin)| origin + iced::Vector::new(cursor.x, cursor.y));
                return self.detach_layout(drag.layout, position);
            }
            Message::DetachedMonitorChecked(window_id, monitor) => {
                return self.check_detached_monitor(window_id, monitor);
            }
            Message::AudioStream(message) => {
                if let Some(event) = self.audio_stream.update(message) {
                    match event {
                        modal::audio::UpdateEvent::RetryFailed(err) => {
                            self.notifications
                                .push(Toast::error(format!("Audio still unavailable: {err}")));
                        }
                        modal::audio::UpdateEvent::RetrySucceeded => {
                            self.notifications.push(Toast::info(
                                "Audio output re-initialized successfully".to_string(),
                            ));
                        }
                    }
                }
            }
            Message::DataFolderRequested => {
                if let Err(err) = data::open_data_folder() {
                    self.notifications
                        .push(Toast::error(format!("Failed to open data folder: {err}")));
                }
            }
            Message::OpenUrlRequested(url) => self.open_url(url.as_ref()),
            Message::Hotkey(hotkey) => return self.handle_hotkey(hotkey),
            Message::IndicatorEditor(msg) => {
                let (task, action) = self.indicator_editor.update(msg);
                let task = task.map(Message::IndicatorEditor);
                if let Some(indicator_editor::Action::Reloaded(id)) = action {
                    return Task::batch([task, self.reload_indicator(&id)]);
                }
                return task;
            }
            Message::ThemeEditor(msg) => {
                let action = self.theme_editor.update(msg, self.theme.current());

                match action {
                    Some(modal::theme_editor::Action::Exit) => {
                        self.sidebar.set_menu(Some(sidebar::Menu::Settings));
                    }
                    Some(modal::theme_editor::Action::UpdateTheme(theme)) => {
                        self.replace_theme(theme);
                    }
                    None => {}
                }
            }
            Message::NetworkEditor(msg) => {
                let action = self.network_editor.update(msg);

                match action {
                    Some(network_editor::Action::ApplyProxy(ref proxy)) => {
                        if let Some(proxy) = proxy {
                            data::config::auth::save_proxy_auth(proxy);
                        } else if let Some(ref old_proxy) = self.network_config.proxy {
                            data::config::auth::delete_proxy_auth(old_proxy);
                        }
                        self.network_config.proxy = proxy.clone();
                        // An explicit choice settles the unknown proxy for the next start; this
                        // session stays `Broken` (`proxy_unreadable`) until it is restarted.
                        self.network_config.proxy_unknown = false;
                        // Streams and clients read the proxy at every connect and request:
                        // from now on they refuse to run with the snapshot they were built on.
                        self.trading.set_proxy(self.current_proxy_state());

                        let main_window = self.main_window.id;
                        let dashboard = self.active_dashboard_mut();

                        let mut active_windows = dashboard
                            .popout
                            .keys()
                            .copied()
                            .collect::<Vec<window::Id>>();
                        active_windows.push(main_window);

                        return window::collect_window_specs(
                            active_windows,
                            Message::SaveStateRequested,
                        );
                    }
                    Some(network_editor::Action::ApplyServerConfig {
                        mode,
                        url,
                        auth_token,
                    }) => {
                        if let Some(ref url) = url
                            && let Some(ref token) = auth_token
                        {
                            data::config::auth::save_server_token(url, token);
                        } else if let Some(ref old_url) = self.network_config.server_url {
                            data::config::auth::delete_server_token(old_url);
                        }
                        self.network_config.server_url = url;
                        self.network_config.server_auth_token = auth_token;
                        self.network_config.trade_fetch_mode = mode;

                        let main_window = self.main_window.id;
                        let dashboard = self.active_dashboard_mut();

                        let mut active_windows = dashboard
                            .popout
                            .keys()
                            .copied()
                            .collect::<Vec<window::Id>>();
                        active_windows.push(main_window);

                        return window::collect_window_specs(
                            active_windows,
                            Message::SaveStateRequested,
                        );
                    }
                    Some(network_editor::Action::Exit) => {
                        self.sidebar.set_menu(Some(sidebar::Menu::Settings));
                    }
                    None => {}
                }
            }
            Message::ExchangeAccounts(msg) => {
                let signed = self.signed_client_for_current_proxy();
                let trading = &self.trading;
                let (task, action) = self
                    .exchange_accounts
                    .update(msg, &signed, &|id| trading.closer_for(id));
                let task = task.map(Message::ExchangeAccounts);

                let mut persist = false;
                let mut extra = Vec::new();
                match action {
                    Some(modal::exchange_accounts::Action::Exit) => {
                        self.sidebar.set_menu(Some(sidebar::Menu::Settings));
                    }
                    Some(modal::exchange_accounts::Action::Tested(meta, report)) => {
                        // The gate reads this session, never the modal's row status.
                        let usable = match &report {
                            Some(report) => {
                                self.trade_session.record(&meta, report);
                                exchange::account::is_usable(meta.env, &report.verdict)
                            }
                            None => {
                                self.trade_session.invalidate(meta.id);
                                false
                            }
                        };
                        // A usable account gets a new stream (a re-test brings an AuthFailed
                        // account back); anything else closes its stream.
                        // Saved unresolved sends of this account are looked up now (not before:
                        // until the account is tested this session they only say "pending").
                        if usable && let Ok(signed) = signed.clone() {
                            let records = self.trading.begin_lookup(meta.id);
                            if !records.is_empty() {
                                let input = account_flow::ClientInput {
                                    meta: (*meta).clone(),
                                    signed,
                                    proxy: self.trading.proxy_reader(),
                                };
                                extra.push(Task::perform(
                                    account_flow::look_up(input, records),
                                    |s| Message::Account(account_flow::Event::Looked(Box::new(s))),
                                ));
                            }
                        }
                        extra.push(
                            self.trading
                                .on_tested(&meta, usable, signed)
                                .map(Message::Trading),
                        );
                    }
                    Some(modal::exchange_accounts::Action::Voided(metas)) => {
                        for meta in &metas {
                            self.trade_session.invalidate(meta.id);
                            extra.push(
                                self.trading
                                    .on_tested(meta, false, signed.clone())
                                    .map(Message::Trading),
                            );
                        }
                    }
                    Some(modal::exchange_accounts::Action::DeleteFailed(meta)) => {
                        // The account stays listed: its runtime, with the one risk tracker, is
                        // kept (N1). Only the stream is closed and the account needs a new test.
                        self.trade_session.invalidate(meta.id);
                        extra.push(
                            self.trading
                                .on_tested(&meta, false, signed.clone())
                                .map(Message::Trading),
                        );
                    }
                    Some(modal::exchange_accounts::Action::RiskSaved(id, settings)) => {
                        self.trading.apply_risk_settings(id, &settings);
                        persist = true;
                    }
                    Some(modal::exchange_accounts::Action::ProtectSaved(id, defaults)) => {
                        self.trading.apply_protect_defaults(id, &defaults);
                        persist = true;
                    }
                    Some(modal::exchange_accounts::Action::Deleting(id)) => {
                        // From here the gate refuses every order of the account (L4).
                        self.trade_session.invalidate(id);
                    }
                    Some(modal::exchange_accounts::Action::Removed(id)) => {
                        self.trade_session.invalidate(id);
                        self.trading.remove(id);
                        persist = true;
                    }
                    Some(modal::exchange_accounts::Action::Persist) => persist = true,
                    None => {}
                }

                if persist {
                    let main_window = self.main_window.id;
                    let dashboard = self.active_dashboard_mut();

                    let mut active_windows = dashboard
                        .popout
                        .keys()
                        .copied()
                        .collect::<Vec<window::Id>>();
                    active_windows.push(main_window);

                    extra.push(window::collect_window_specs(
                        active_windows,
                        Message::SaveStateRequested,
                    ));
                }
                extra.push(task);
                return Task::batch(extra);
            }
            Message::OrderTicket(msg) => return self.update_order_ticket(msg),
            Message::OrderConfirm(msg) => return self.update_order_confirm(msg),
            Message::OrderSent(summary) => return self.on_order_sent(summary),
            Message::ReplaceSent(summary) => return self.on_replace_sent(summary),
            Message::AccountPanel(msg) => return self.update_account_panel(msg),
            Message::BulkConfirm(msg) => return self.update_bulk_confirm(msg),
            Message::Account(event) => return self.on_account_event(event),
            Message::Trading(msg) => {
                let (task, effects) = self
                    .trading
                    .update(msg, chrono::Utc::now().timestamp_millis());
                // The stream says the exchange refused the login or the Keychain failed: the
                // session verdict is void until the account is tested again (condition 57).
                for id in effects.invalidate {
                    self.trade_session.invalidate(id);
                }
                return Task::batch([task.map(Message::Trading), self.run_auto_jobs()]);
            }
            Message::AutoPoll => return self.run_auto_jobs(),
            Message::AutoDone(summary) => {
                // An automatic order may have left an unknown result: read the list again so the
                // panel offers the same release flow as for a manual one.
                let unknown = summary.has_unknown();
                self.trading.auto_done(*summary);
                let jobs = self.run_auto_jobs();
                return if unknown {
                    Task::batch([Self::reload_records(), jobs])
                } else {
                    jobs
                };
            }
            Message::Sidebar(message) => {
                if let screen::sidebar::Message::TickersTable(
                    screen::sidebar::tickers_table::Message::UpdateStats(venue, _),
                ) = &message
                {
                    self.quote_stamps.insert(*venue, std::time::Instant::now());
                }
                let (task, action) = self.sidebar.update(message);

                if let Some(screen::sidebar::Action::MenuChanged(menu)) = &action
                    && modal::exchange_accounts::should_clear_on_menu_change(menu.as_ref())
                {
                    // Closing the modal (or switching away) drops any typed credentials.
                    // Background ticks yield no action and must not wipe the form.
                    self.exchange_accounts.clear_form();
                }
                if let Some(screen::sidebar::Action::MenuChanged(menu)) = &action
                    && *menu != Some(sidebar::Menu::OrderTicket)
                {
                    // Leaving the ticket drops its pending preview and confirmation.
                    self.discard_order();
                }

                match action {
                    Some(screen::sidebar::Action::MenuChanged(Some(sidebar::Menu::Network))) => {
                        self.network_editor = NetworkEditor::new(
                            &self.network_config,
                            self.network_editor.pending_apply().cloned(),
                        );
                    }
                    Some(screen::sidebar::Action::MenuChanged(Some(sidebar::Menu::Layout))) => {
                        self.layout_manager.refresh_templates();
                    }
                    Some(screen::sidebar::Action::MenuChanged(Some(
                        sidebar::Menu::IndicatorEditor,
                    ))) => {
                        self.indicator_editor.refresh();
                    }
                    Some(screen::sidebar::Action::MenuChanged(Some(
                        sidebar::Menu::OrderTicket,
                    ))) => {
                        return Task::batch([task.map(Message::Sidebar), Self::reload_records()]);
                    }
                    Some(screen::sidebar::Action::MenuChanged(_)) => {}
                    Some(screen::sidebar::Action::TickerSelected(ticker_info, content)) => {
                        let main_window_id = self.main_window.id;
                        let data_sources = Arc::clone(&self.data_sources);
                        if self.layout_manager.active_layout_id().is_none() {
                            return task.map(Message::Sidebar);
                        }

                        let dashboard_task = {
                            if let Some(kind) = content {
                                self.active_dashboard_mut().init_focused_pane(
                                    &data_sources,
                                    main_window_id,
                                    ticker_info,
                                    kind,
                                )
                            } else {
                                self.active_dashboard_mut().switch_tickers_in_group(
                                    &data_sources,
                                    main_window_id,
                                    ticker_info,
                                )
                            }
                        };

                        return self.map_dashboard_task(dashboard_task, None);
                    }
                    Some(screen::sidebar::Action::ErrorOccurred(err)) => {
                        self.notifications.push(Toast::error(err.to_string()));
                    }
                    None => {}
                }

                return task.map(Message::Sidebar);
            }
            Message::ApplyVolumeSizeUnit(pref) => {
                self.volume_size_unit = pref;

                let mut active_windows: Vec<window::Id> =
                    self.active_dashboard().popout.keys().copied().collect();
                active_windows.push(self.main_window.id);

                return window::collect_window_specs(active_windows, |windows| {
                    Message::SaveStateRequested(windows)
                });
            }
            Message::ApplyAntialiasing(enabled) => {
                self.antialiasing = enabled;

                let mut active_windows: Vec<window::Id> =
                    self.active_dashboard().popout.keys().copied().collect();
                active_windows.push(self.main_window.id);

                return window::collect_window_specs(active_windows, |windows| {
                    Message::SaveStateRequested(windows)
                });
            }
        }
        Task::none()
    }

    fn view(&self, id: window::Id) -> Element<'_, Message> {
        if let Some(layout_id) = self.layout_manager.layout_for_window(id) {
            return self.view_detached(id, layout_id);
        }

        let dashboard = self.active_dashboard();
        let sidebar_pos = self.sidebar.position();

        let pixel_ratio = self
            .window_scale_factors
            .pixel_ratio_for_window(id, self.ui_scale_factor);

        let tickers_table = &self.sidebar.tickers_table;

        let content = if id == self.main_window.id {
            let sidebar_view = self
                .sidebar
                .view(self.audio_stream.volume())
                .map(Message::Sidebar);
            let dashboard_view = dashboard
                .view(
                    &self.main_window,
                    tickers_table,
                    self.timezone,
                    pixel_ratio,
                    &self.theme,
                    self.layout_manager.chart_resources(),
                    &self.trade_overlay,
                )
                .map(move |msg| Message::Dashboard {
                    layout_id: None,
                    event: msg,
                });

            let header_title = {
                #[cfg(target_os = "macos")]
                {
                    iced::widget::center(
                        text("FLOWSURFACE")
                            .font(iced::Font {
                                weight: iced::font::Weight::Bold,
                                ..Default::default()
                            })
                            .size(crate::style::text_size::TITLE)
                            .style(style::title_text),
                    )
                    .height(20)
                    .align_y(Alignment::Center)
                    .padding(padding::top(4))
                }
                #[cfg(not(target_os = "macos"))]
                {
                    column![]
                }
            };

            let dashboard_row = match sidebar_pos {
                sidebar::Position::Left => row![sidebar_view, dashboard_view],
                sidebar::Position::Right => row![dashboard_view, sidebar_view],
            };

            let tab_bar = screen::tab_bar::view(&self.layout_manager).map(Message::Layouts);

            let base = column![header_title, tab_bar, dashboard_row.spacing(4).padding(8),];

            if let Some(menu) = self.sidebar.active_menu() {
                self.view_with_modal(base.into(), dashboard, menu)
            } else {
                base.into()
            }
        } else {
            container(
                dashboard
                    .view_window(
                        id,
                        &self.main_window,
                        tickers_table,
                        self.timezone,
                        pixel_ratio,
                        &self.theme,
                        self.layout_manager.chart_resources(),
                        &self.trade_overlay,
                    )
                    .map(move |msg| Message::Dashboard {
                        layout_id: None,
                        event: msg,
                    }),
            )
            .padding(padding::top(style::TITLE_PADDING_TOP))
            .into()
        };

        let content = self.with_standing_alerts(content);
        let content = self.with_order_confirm(id, content);

        toast::Manager::new(
            content,
            self.notifications.toasts(),
            match sidebar_pos {
                sidebar::Position::Left => Alignment::Start,
                sidebar::Position::Right => Alignment::End,
            },
            Message::RemoveNotification,
        )
        .into()
    }

    /// The alerts that must stay on screen, above the content of every window (the main window
    /// and each popout; a toast fades, these do not).
    fn with_standing_alerts<'a>(&'a self, content: Element<'a, Message>) -> Element<'a, Message> {
        let saved = self.exchange_accounts.persisted().accounts;
        let alerts = trading::standing_alerts(&self.trading, &self.trade_session, &saved);
        match trading::alerts_view(alerts) {
            Some(banners) => column![banners, content].into(),
            None => content,
        }
    }

    /// The confirmation of a prepared order, over everything in the window it was opened in.
    /// The backdrop takes every click, so nothing behind it can be pressed.
    fn with_order_confirm<'a>(
        &'a self,
        id: window::Id,
        content: Element<'a, Message>,
    ) -> Element<'a, Message> {
        let now = std::time::Instant::now();
        let dialog: Element<'a, Message> =
            if let Some(confirm) = self.order_confirm.as_ref().filter(|c| c.window == id) {
                confirm.view(now).map(Message::OrderConfirm)
            } else if let Some(bulk) = self.bulk_confirm.as_ref().filter(|c| c.window == id) {
                bulk.view(now).map(Message::BulkConfirm)
            } else {
                return content;
            };
        iced::widget::stack![
            content,
            iced::widget::opaque(iced::widget::mouse_area(
                container(iced::widget::opaque(dialog))
                    .width(iced::Length::Fill)
                    .height(iced::Length::Fill)
                    .align_x(Alignment::Center)
                    .align_y(Alignment::Center)
                    .style(|_| container::Style {
                        background: Some(iced::Color::from_rgba(0.0, 0.0, 0.0, 0.5).into()),
                        ..container::Style::default()
                    }),
            ))
        ]
        .into()
    }

    fn view_detached(&self, id: window::Id, layout_id: uuid::Uuid) -> Element<'_, Message> {
        let pixel_ratio = self
            .window_scale_factors
            .pixel_ratio_for_window(id, self.ui_scale_factor);
        let content: Element<'_, Message> = match self
            .layout_manager
            .get(layout_id)
            .zip(self.layout_manager.window_handle(id))
        {
            Some((layout, own_window)) => container(
                layout
                    .dashboard
                    .view(
                        own_window,
                        &self.sidebar.tickers_table,
                        self.timezone,
                        pixel_ratio,
                        &self.theme,
                        self.layout_manager.chart_resources(),
                        &self.trade_overlay,
                    )
                    .map(move |event| Message::Dashboard {
                        layout_id: Some(layout_id),
                        event,
                    }),
            )
            .padding(padding::top(style::TITLE_PADDING_TOP))
            .into(),
            None => container(text("Layout not found")).into(),
        };

        let content = self.with_standing_alerts(content);
        let content = self.with_order_confirm(id, content);

        toast::Manager::new(
            content,
            self.notifications.toasts(),
            Alignment::Start,
            Message::RemoveNotification,
        )
        .into()
    }

    fn theme(&self, _window: window::Id) -> iced_core::Theme {
        self.theme.current().clone()
    }

    fn title(&self, window: window::Id) -> String {
        if let Some(layout) = self
            .layout_manager
            .layout_for_window(window)
            .and_then(|id| self.layout_manager.get(id))
        {
            format!("{} [{}]", version::APP_NAME, layout.id.name)
        } else if let Some(id) = self.layout_manager.active_layout_id() {
            format!("{} [{}]", version::APP_NAME, id.name)
        } else {
            version::APP_NAME.to_string()
        }
    }

    fn scale_factor(&self, _window: window::Id) -> f32 {
        self.ui_scale_factor.into()
    }

    fn subscription(&self) -> Subscription<Message> {
        let window_events = window::events().map(Message::WindowEvent);
        let sidebar = self.sidebar.subscription().map(Message::Sidebar);

        // One merged, order-stable stream set keeps the subscription identity steady and
        // avoids duplicate connections when layouts share a stream.
        let mut streams = UniqueStreams::default();
        for id in self.layout_manager.visible_ids() {
            if let Some(layout) = self.layout_manager.get(id) {
                streams.merge(&layout.dashboard.streams);
            }
        }
        let exchange_streams =
            Dashboard::market_subscriptions(&streams, &self.data_sources.exchange)
                .map(Message::MarketWsEvent);
        let trading = self.trading.subscription().map(Message::Trading);

        let tick = iced::window::frames().map(Message::Tick);

        let hotkeys = keyboard::listen().filter_map(|event| {
            let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
                return None;
            };
            let hotkey = match key.as_ref() {
                keyboard::Key::Named(keyboard::key::Named::Escape) => Hotkey::Escape,
                keyboard::Key::Named(keyboard::key::Named::Tab) if modifiers.control() => {
                    Hotkey::CycleLayout {
                        forward: !modifiers.shift(),
                    }
                }
                keyboard::Key::Character(c) if modifiers.command() => match c {
                    "t" => Hotkey::NewLayout,
                    "w" => Hotkey::CloseLayout,
                    "k" => Hotkey::TickerSearch,
                    "s" => Hotkey::Save,
                    digit => digit
                        .parse::<usize>()
                        .ok()
                        .filter(|n| (1..=9).contains(n))
                        .map(|n| Hotkey::SelectLayout(n - 1))?,
                },
                _ => return None,
            };
            Some(Message::Hotkey(hotkey))
        });

        // The release can land outside the grip, so follow the cursor only while dragging.
        let tab_drag = if self.tab_drag.is_some() {
            iced::event::listen_with(|event, _status, window_id| match event {
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                    Some(Message::TabDragMoved(window_id, position))
                }
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                )) => Some(Message::TabDragReleased),
                _ => None,
            })
        } else {
            Subscription::none()
        };

        // Redraws a LIVE confirmation so its button turns on after the delay.
        let now = std::time::Instant::now();
        let confirm_delay = if self.order_confirm.as_ref().is_some_and(|c| c.waiting(now))
            || self.bulk_confirm.as_ref().is_some_and(|c| c.waiting(now))
        {
            iced::time::every(std::time::Duration::from_millis(100))
                .map(|_| Message::OrderConfirm(modal::order_confirm::Message::DelayElapsed))
        } else {
            Subscription::none()
        };

        let auto_poll =
            iced::time::every(std::time::Duration::from_secs(1)).map(|_| Message::AutoPoll);

        Subscription::batch(vec![
            confirm_delay,
            auto_poll,
            tab_drag,
            exchange_streams,
            trading,
            sidebar,
            window_events,
            tick,
            hotkeys,
        ])
    }

    /// Insert a layout built from a serialized dashboard, named after `name` but unique.
    fn add_layout_from_config(&mut self, name: &str, ser_dashboard: data::Dashboard) -> uuid::Uuid {
        let manager = &mut self.layout_manager;
        let new_uid = uuid::Uuid::new_v4();
        let new_layout = LayoutId {
            unique: new_uid,
            name: manager.ensure_unique_name(name, new_uid),
        };

        let popout_windows = ser_dashboard
            .popout
            .iter()
            .map(|(pane, window_spec)| (configuration(pane.clone()), *window_spec))
            .collect();

        let dashboard = Dashboard::from_config(
            configuration(ser_dashboard.pane),
            popout_windows,
            ser_dashboard.chart_settings,
        );

        manager.insert_layout(new_layout, dashboard);
        new_uid
    }

    fn handle_hotkey(&mut self, hotkey: Hotkey) -> Task<Message> {
        use modal::layout_manager::Message as Layouts;

        // While a confirmation is open only Escape (which cancels it) does anything.
        if self.confirm_open() {
            return match hotkey {
                Hotkey::Escape => Task::done(Message::GoBack),
                _ => Task::none(),
            };
        }
        // While the indicator editor is open, layout and search shortcuts would act on the
        // dashboard hidden behind it; only Escape and save apply there.
        if self.sidebar.active_menu() == Some(sidebar::Menu::IndicatorEditor) {
            return match hotkey {
                Hotkey::Escape => Task::done(Message::GoBack),
                Hotkey::Save => {
                    Task::done(Message::IndicatorEditor(indicator_editor::Message::Save))
                }
                _ => Task::none(),
            };
        }
        let message = match hotkey {
            Hotkey::Escape => Message::GoBack,
            Hotkey::CycleLayout { forward } => Message::Layouts(Layouts::Cycle { forward }),
            Hotkey::NewLayout => Message::Layouts(Layouts::AddLayout),
            Hotkey::CloseLayout => Message::Layouts(Layouts::RequestCloseActive),
            Hotkey::SelectLayout(index) => Message::Layouts(Layouts::SelectIndex(index)),
            Hotkey::TickerSearch => Message::Sidebar(screen::sidebar::Message::TickersTable(
                screen::sidebar::tickers_table::Message::ToggleTable,
            )),
            Hotkey::Save => return Task::none(),
        };
        Task::done(message)
    }

    /// Rebuild the charts that use `indicator` after the catalog was reloaded, in the active
    /// layout and the detached ones. Other layouts are rebuilt from the new catalog when loaded.
    fn reload_indicator(&mut self, indicator: &data::chart::kline::IndicatorId) -> Task<Message> {
        let data_sources = Arc::clone(&self.data_sources);
        let mut tasks = vec![];

        for layout_id in self.layout_manager.visible_ids() {
            let window_id = self.layout_window_id(layout_id);
            let Some(dashboard) = self.layout_manager.mut_dashboard(layout_id) else {
                continue;
            };
            let task = dashboard.reload_indicator(window_id, indicator, &data_sources);
            tasks.push(self.map_dashboard_task(task, Some(layout_id)));
        }

        Task::batch(tasks)
    }

    /// The window a layout is shown in: its own when detached, otherwise the main window.
    fn layout_window_id(&self, layout_id: uuid::Uuid) -> window::Id {
        self.layout_manager
            .detached_window(layout_id)
            .unwrap_or(self.main_window.id)
    }

    fn active_dashboard(&self) -> &Dashboard {
        let active_layout = self
            .layout_manager
            .active_layout_id()
            .expect("No active layout");
        self.layout_manager
            .get(active_layout.unique)
            .map(|layout| &layout.dashboard)
            .expect("No active dashboard")
    }

    fn active_dashboard_mut(&mut self) -> &mut Dashboard {
        let active_layout = self
            .layout_manager
            .active_layout_id()
            .expect("No active layout");
        self.layout_manager
            .get_mut(active_layout.unique)
            .map(|layout| &mut layout.dashboard)
            .expect("No active dashboard")
    }

    fn update_shared_chart_resources(&mut self, message: manifest::Message) -> Task<Message> {
        self.layout_manager
            .update_chart_resources(message, &self.data_sources.catalog_http)
            .map(Message::SharedChartResources)
    }

    fn map_dashboard_task(
        &self,
        task: Task<dashboard::Message>,
        layout_id: Option<uuid::Uuid>,
    ) -> Task<Message> {
        let Some(layout_id) = layout_id.or_else(|| {
            self.layout_manager
                .active_layout_id()
                .map(|layout| layout.unique)
        }) else {
            return Task::none();
        };

        task.map(move |event| Message::Dashboard {
            layout_id: Some(layout_id),
            event,
        })
    }

    fn open_url(&mut self, url: &str) {
        if let Err(err) = data::open_url(url) {
            self.notifications
                .push(Toast::error(format!("Failed to open link: {err}")));
        }
    }

    fn replace_theme(&mut self, theme: iced_core::Theme) {
        if !self.theme.replace(theme.clone()) {
            return;
        }

        for layout_id in self.layout_manager.visible_ids() {
            let window_id = self.layout_window_id(layout_id);
            if let Some(dashboard) = self.layout_manager.mut_dashboard(layout_id) {
                dashboard.theme_updated(window_id, &theme);
            }
        }
    }

    fn handle_window_event(&mut self, event: window::Event) -> Task<Message> {
        match event {
            window::Event::Opened(window_id, position, size) => {
                if window_id == self.main_window.id {
                    self.main_window.position = position;
                }

                let mut tasks = vec![window::scale_factor(window_id, |window, scale_factor| {
                    Message::WindowScaleFactorUpdated {
                        window,
                        scale_factor,
                    }
                })];

                if self.layout_manager.layout_for_window(window_id).is_some() {
                    self.layout_manager
                        .record_window(window_id, position, Some(size));
                    tasks.push(Self::probe_monitor(window_id));
                }
                Task::batch(tasks)
            }
            window::Event::Moved(window_id, position) => {
                if window_id == self.main_window.id {
                    self.main_window.position = Some(position);
                } else if self.layout_manager.layout_for_window(window_id).is_some() {
                    self.layout_manager
                        .record_window(window_id, Some(position), None);
                    return Self::probe_monitor(window_id);
                }
                Task::none()
            }
            window::Event::Resized(window_id, size) => {
                self.layout_manager
                    .record_window(window_id, None, Some(size));
                Task::none()
            }
            window::Event::Rescaled(window_id, scale_factor) => {
                self.window_scale_factors.set(window_id, scale_factor);
                Task::none()
            }
            window::Event::Closed(window_id) => {
                // The window is really gone, whichever path closed it (a layout switch and a
                // popout merge close windows without a close request). Safe to repeat.
                if window_id != self.main_window.id {
                    self.forget_window(window_id);
                }
                Task::none()
            }
            window::Event::CloseRequested(window_id) => {
                let main_window = self.main_window.id;

                if window_id != main_window {
                    self.forget_window(window_id);
                    self.restoring_windows.remove(&window_id);

                    // Closing a detached window returns its layout to the tab bar.
                    if self.layout_manager.layout_for_window(window_id).is_some() {
                        self.layout_manager.sync_drawings(main_window);
                        if let Some(layout_id) = self.layout_manager.attach(window_id) {
                            if let Some(dashboard) = self.layout_manager.mut_dashboard(layout_id) {
                                dashboard.clear_focus();
                            }
                            if let Some(active) = self.layout_manager.active_layout_id() {
                                let active = active.unique;
                                self.layout_manager
                                    .park_inactive_layouts(active, main_window);
                            }
                        }
                        return window::close(window_id);
                    }

                    self.active_dashboard_mut().popout.remove(&window_id);
                    return window::close(window_id);
                }

                // A resting limit entry that still owes its protection could fill while the
                // app is closed: say so once and let a second request close (condition 81).
                let now = std::time::Instant::now();
                if protect_attach::hold_close(
                    self.trading.has_resting_limit_attach(),
                    self.close_warned_at,
                    now,
                ) {
                    self.close_warned_at = Some(now);
                    self.notifications
                        .push(Toast::warn(protect_attach::CLOSE_WARNING.to_string()));
                    return Task::none();
                }

                let dashboard = self.active_dashboard_mut();

                let mut active_windows = dashboard
                    .popout
                    .keys()
                    .copied()
                    .collect::<Vec<window::Id>>();
                active_windows.push(main_window);
                active_windows.extend(self.layout_manager.detached_windows());

                window::collect_window_specs(active_windows, Message::ExitRequested)
            }
        }
    }

    /// Asks for the monitor size of a detached window, to store with its position.
    fn probe_monitor(window_id: window::Id) -> Task<Message> {
        iced::window::monitor_size(window_id)
            .map(move |monitor| Message::DetachedMonitorChecked(window_id, monitor))
    }

    /// Records the monitor of a detached window. A restored window that ended up on a monitor of
    /// a different size than the saved one is moved next to the main window, so it cannot stay
    /// off-screen after a monitor was unplugged.
    fn check_detached_monitor(
        &mut self,
        window_id: window::Id,
        monitor: Option<iced::Size>,
    ) -> Task<Message> {
        let saved_monitor = self
            .layout_manager
            .saved_window(window_id)
            .and_then(|saved| saved.monitor);
        let first_check = self.restoring_windows.remove(&window_id);

        if first_check
            && let (Some(current), Some((saved_w, saved_h))) = (monitor, saved_monitor)
            && ((current.width - saved_w).abs() > 1.0 || (current.height - saved_h).abs() > 1.0)
            && let Some(main_position) = self.main_window.position
        {
            self.layout_manager.record_monitor(window_id, monitor);
            return iced::window::move_to(window_id, main_position + iced::Vector::new(40.0, 40.0));
        }

        if first_check || self.layout_manager.layout_for_window(window_id).is_some() {
            self.layout_manager.record_monitor(window_id, monitor);
        }
        Task::none()
    }

    /// Moves a layout out of the tab bar into a window of its own.
    fn detach_layout(&mut self, id: uuid::Uuid, position: Option<iced::Point>) -> Task<Message> {
        if !self.layout_manager.can_detach(id) {
            self.notifications.push(Toast::info(
                "At least one layout must stay in the main window".to_string(),
            ));
            return Task::none();
        }

        let main_window = self.main_window;
        let was_active = self
            .layout_manager
            .active_layout_id()
            .is_some_and(|layout| layout.unique == id);

        self.layout_manager.sync_drawings(main_window.id);

        // Popped-out panes go back to the layout's own grid; the layout then owns one window.
        let (merged_windows, merge_task) = match self.layout_manager.mut_dashboard(id) {
            Some(dashboard) => dashboard.merge_all_popouts(&main_window),
            None => (vec![], Task::none()),
        };
        let mut tasks = vec![self.map_dashboard_task(merge_task, Some(id))];
        for popout in merged_windows {
            self.forget_window(popout);
            if was_active {
                tasks.push(window::close::<window::Id>(popout).discard());
            }
        }

        let (window_id, open_task) = window::open(window::Settings {
            position: position.map_or(window::Position::Default, window::Position::Specific),
            size: window::default_size(),
            exit_on_close_request: false,
            min_size: Some(iced::Size::new(400.0, 300.0)),
            ..window::settings()
        });
        tasks.push(open_task.discard());

        let Some(next_active) = self.layout_manager.detach(id, window_id) else {
            return Task::batch(tasks);
        };

        if let Some(next) = next_active {
            tasks.push(self.load_layout(next, main_window.id));
        }

        // The detached layout stays live: refresh its stream set for its own window.
        if let Some(dashboard) = self.layout_manager.mut_dashboard(id) {
            dashboard.clear_focus();
            let task = dashboard.load_layout(window_id);
            tasks.push(self.map_dashboard_task(task, Some(id)));
        }

        Task::batch(tasks)
    }

    /// Opens the detached windows read from the saved state.
    fn restore_detached_windows(&mut self) -> Task<Message> {
        let mut tasks = vec![];

        for (layout_id, saved) in self.layout_manager.take_restorable() {
            // Without a known monitor the saved coordinates cannot be trusted.
            let position = if saved.monitor.is_some() {
                window::Position::Specific(saved.spec.position())
            } else {
                window::Position::Centered
            };
            let (window_id, open_task) = window::open(window::Settings {
                position,
                size: saved.spec.size(),
                exit_on_close_request: false,
                min_size: Some(iced::Size::new(400.0, 300.0)),
                ..window::settings()
            });
            tasks.push(open_task.discard());

            self.layout_manager
                .restore_detached(layout_id, window_id, saved);
            self.restoring_windows.insert(window_id);

            let own_window = window::Window::new(window_id);
            if let Some(dashboard) = self.layout_manager.mut_dashboard(layout_id) {
                // Layouts are saved with their popouts merged; fold in any stragglers.
                let (_, merge_task) = dashboard.merge_all_popouts(&own_window);
                let load_task = dashboard.load_layout(window_id);
                tasks.push(self.map_dashboard_task(merge_task, Some(layout_id)));
                tasks.push(self.map_dashboard_task(load_task, Some(layout_id)));
            }
        }

        Task::batch(tasks)
    }

    fn load_layout(&mut self, layout_uid: uuid::Uuid, main_window: window::Id) -> Task<Message> {
        // Publish the outgoing layout's edits before its charts are parked.
        self.layout_manager.sync_drawings(main_window);

        if let Err(err) = self.layout_manager.set_active_layout(layout_uid) {
            log::error!("Failed to set active layout: {}", err);
            return Task::none();
        }

        self.layout_manager
            .park_inactive_layouts(layout_uid, main_window);

        let task = self
            .layout_manager
            .get_mut(layout_uid)
            .map(|layout| layout.dashboard.load_layout(main_window));

        task.map(|task| self.map_dashboard_task(task, Some(layout_uid)))
            .unwrap_or_else(|| {
                log::error!("Active layout missing after selection: {}", layout_uid);
                Task::none()
            })
    }

    fn view_with_modal<'a>(
        &'a self,
        base: Element<'a, Message>,
        dashboard: &'a Dashboard,
        menu: sidebar::Menu,
    ) -> Element<'a, Message> {
        let sidebar_pos = self.sidebar.position();

        match menu {
            sidebar::Menu::Settings => {
                let settings_modal = {
                    let theme_picklist = {
                        let mut themes: Vec<iced::Theme> = iced_core::Theme::ALL.to_vec();

                        let default_theme = iced_core::Theme::Custom(default_theme().into());
                        themes.push(default_theme);

                        if let Some(custom_theme) = &self.theme_editor.custom_theme {
                            themes.push(custom_theme.clone());
                        }

                        pick_list(themes, Some(self.theme.current().clone()), |theme| {
                            Message::ThemeSelected(theme)
                        })
                    };

                    let toggle_theme_editor = button(text("Theme editor")).on_press(
                        Message::Sidebar(screen::sidebar::Message::ToggleSidebarMenu(Some(
                            sidebar::Menu::ThemeEditor,
                        ))),
                    );

                    let toggle_network_editor = button(text("Network")).on_press(Message::Sidebar(
                        screen::sidebar::Message::ToggleSidebarMenu(Some(sidebar::Menu::Network)),
                    ));

                    let toggle_exchange_accounts = button(text("Exchange accounts")).on_press(
                        Message::Sidebar(screen::sidebar::Message::ToggleSidebarMenu(Some(
                            sidebar::Menu::ExchangeAccounts,
                        ))),
                    );

                    let toggle_order_ticket = button(text("Order ticket")).on_press(
                        Message::Sidebar(screen::sidebar::Message::ToggleSidebarMenu(Some(
                            sidebar::Menu::OrderTicket,
                        ))),
                    );

                    let timezone_picklist = pick_list(
                        [data::UserTimezone::Utc, data::UserTimezone::Local],
                        Some(self.timezone),
                        Message::SetTimezone,
                    );

                    let restart_required_marker = || {
                        let btn = button(text("!").style(|theme: &iced::Theme| {
                            iced::widget::text::Style {
                                color: Some(theme.extended_palette().warning.base.color),
                            }
                        }))
                        .style(move |theme, status| style::button::modifier(theme, status, true));
                        tooltip(btn, Some("Restart required"), TooltipPosition::Top)
                    };

                    let size_in_quote_ccy_checkbox = {
                        let is_active = match self.volume_size_unit {
                            exchange::SizeUnit::Quote => true,
                            exchange::SizeUnit::Base => false,
                        };
                        let restart_required =
                            self.volume_size_unit != self.applied_volume_size_unit;

                        let checkbox = iced::widget::checkbox(is_active)
                            .label("Size in quote currency")
                            .on_toggle(|checked| {
                                Message::ApplyVolumeSizeUnit(if checked {
                                    exchange::SizeUnit::Quote
                                } else {
                                    exchange::SizeUnit::Base
                                })
                            });

                        let checkbox = tooltip(
                            checkbox,
                            Some(
                                "Display sizes/volumes in quote currency (USD).\n- Has no effect on inverse perps or open interest\n- Restart required to apply.",
                            ),
                            TooltipPosition::Top,
                        );

                        let mut row = row![checkbox]
                            .align_y(iced::Alignment::Center)
                            .spacing(8)
                            .height(24);
                        if restart_required {
                            row = row.push(restart_required_marker());
                        }
                        row
                    };

                    let cache_metadata_checkbox = {
                        let cache_enabled = self.sidebar.cache_enabled();

                        let checkbox = tooltip(
                            iced::widget::checkbox(cache_enabled)
                                .label("Cache ticker metadata")
                                .on_toggle(Message::SetMetadataCaching),
                            Some("Cache ticker metadata for faster startup."),
                            TooltipPosition::Top,
                        );

                        if !cache_enabled {
                            column![row![checkbox].height(24).align_y(iced::Alignment::Center)]
                        } else {
                            let loading = self.sidebar.is_metadata_loading();

                            let refresh_btn = {
                                let btn = button(crate::style::icon_text(
                                    crate::style::Icon::Refresh,
                                    12,
                                ))
                                .style(move |theme, status| {
                                    style::button::modifier(theme, status, loading)
                                });
                                let btn = if loading {
                                    btn
                                } else {
                                    btn.on_press(Message::RefreshMetadata)
                                };

                                tooltip(
                                    btn,
                                    if loading { None } else { Some("Refresh now") },
                                    TooltipPosition::Top,
                                )
                            };

                            let last_updated = {
                                let last_update = self.sidebar.last_metadata_update();

                                let label = if loading {
                                    "Refreshing metadata…".to_string()
                                } else {
                                    match last_update {
                                        Some(at) => {
                                            format!(
                                                "Metadata last update: {}",
                                                data::util::relative_time_label(at)
                                            )
                                        }
                                        None => "Metadata not fetched yet".to_string(),
                                    }
                                };

                                text(label)
                                    .size(crate::style::text_size::SMALL)
                                    .style(style::secondary_text)
                            };

                            column![
                                row![checkbox, refresh_btn]
                                    .align_y(iced::Alignment::Center)
                                    .spacing(8),
                                last_updated,
                            ]
                            .spacing(4)
                        }
                    };

                    let sidebar_pos_picklist = pick_list(
                        [sidebar::Position::Left, sidebar::Position::Right],
                        Some(sidebar_pos),
                        |pos| Message::Sidebar(screen::sidebar::Message::SetSidebarPosition(pos)),
                    );

                    let scale_factor = {
                        let current_value: f32 = self.ui_scale_factor.into();

                        let decrease_btn = if current_value > data::config::MIN_SCALE {
                            button(text("-"))
                                .on_press(Message::ScaleFactorChanged((current_value - 0.1).into()))
                        } else {
                            button(text("-"))
                        };

                        let increase_btn = if current_value < data::config::MAX_SCALE {
                            button(text("+"))
                                .on_press(Message::ScaleFactorChanged((current_value + 0.1).into()))
                        } else {
                            button(text("+"))
                        };

                        container(
                            row![
                                decrease_btn,
                                text(format!("{:.0}%", current_value * 100.0))
                                    .size(crate::style::text_size::SECTION),
                                increase_btn,
                            ]
                            .align_y(Alignment::Center)
                            .spacing(8)
                            .padding(4),
                        )
                        .style(style::modal_container)
                    };

                    let antialiasing_checkbox = {
                        let restart_required = self.antialiasing != self.applied_antialiasing;
                        let checkbox = iced::widget::checkbox(self.antialiasing)
                            .label("Antialiasing")
                            .on_toggle(Message::ApplyAntialiasing);

                        let checkbox = tooltip(
                            checkbox,
                            Some(
                                "Disabling may increase performance, but expect rough edges\n- Restart required to apply.",
                            ),
                            TooltipPosition::Top,
                        );

                        let mut row = row![checkbox]
                            .align_y(iced::Alignment::Center)
                            .spacing(8)
                            .height(24);
                        if restart_required {
                            row = row.push(restart_required_marker());
                        }
                        row
                    };

                    let open_data_folder = {
                        let button =
                            button(text("Open data folder")).on_press(Message::DataFolderRequested);

                        tooltip(
                            button,
                            Some("Open the folder where the data & config is stored"),
                            TooltipPosition::Top,
                        )
                    };

                    let version_info = {
                        let (version_label, commit_label) = version::app_build_version_parts();

                        let github_link_button =
                            button(text(version_label).size(crate::style::text_size::EMPHASIS))
                                .padding(0)
                                .style(style::button::text_link)
                                .on_press(Message::OpenUrlRequested(Cow::Borrowed(
                                    version::FORK_REPOSITORY_URL,
                                )));

                        let github_button: Element<'_, Message> = iced::widget::tooltip(
                            github_link_button,
                            container(
                                row![
                                    text("GitHub"),
                                    style::icon_text(style::Icon::ExternalLink, 12),
                                ]
                                .spacing(4)
                                .align_y(Alignment::Center),
                            )
                            .style(style::tooltip)
                            .padding(8),
                            TooltipPosition::Top,
                        )
                        .into();

                        if let (Some(commit_label), Some(commit_url)) =
                            (commit_label, version::build_commit_url())
                        {
                            let commit_button =
                                button(text(commit_label).size(crate::style::text_size::SMALL))
                                    .padding(0)
                                    .style(style::button::text_link_secondary)
                                    .on_press(Message::OpenUrlRequested(Cow::Owned(commit_url)));

                            column![github_button, commit_button]
                                .spacing(2)
                                .align_x(Alignment::End)
                                .into()
                        } else {
                            github_button
                        }
                    };

                    let footer = column![
                        container(version_info)
                            .width(iced::Length::Fill)
                            .align_x(Alignment::End),
                    ]
                    .spacing(8);

                    let column_content = split_column![
                        column![open_data_folder,].spacing(8),
                        column![text("Sidebar position").size(crate::style::text_size::SECTION), sidebar_pos_picklist,].spacing(12),
                        column![text("Time zone").size(crate::style::text_size::SECTION), timezone_picklist,].spacing(12),
                        column![text("Market data").size(crate::style::text_size::SECTION), column![size_in_quote_ccy_checkbox, cache_metadata_checkbox].spacing(4)].spacing(12),
                        column![text("Theme").size(crate::style::text_size::SECTION), theme_picklist,].spacing(12),
                        column![text("Interface scale").size(crate::style::text_size::SECTION), scale_factor,].spacing(12),
                        column![text("Rendering").size(crate::style::text_size::SECTION), antialiasing_checkbox,].spacing(12),
                        column![
                            text("Experimental").size(crate::style::text_size::SECTION),
                            column![toggle_theme_editor, toggle_network_editor, toggle_exchange_accounts, toggle_order_ticket].spacing(8),
                        ]
                        .spacing(12),
                        footer,
                        ; spacing = 16, align_x = Alignment::Start
                    ];

                    let content = scrollable::Scrollable::with_direction(
                        column_content,
                        scrollable::Direction::Vertical(
                            scrollable::Scrollbar::new().width(8).scroller_width(6),
                        ),
                    );

                    container(content)
                        .align_x(Alignment::Start)
                        .max_width(240)
                        .padding(24)
                        .style(style::dashboard_modal)
                };

                let (align_x, padding) = match sidebar_pos {
                    sidebar::Position::Left => (Alignment::Start, padding::left(44).bottom(4)),
                    sidebar::Position::Right => (Alignment::End, padding::right(44).bottom(4)),
                };

                dashboard_modal(
                    base,
                    settings_modal,
                    Message::Sidebar(screen::sidebar::Message::ToggleSidebarMenu(None)),
                    padding,
                    Alignment::End,
                    align_x,
                )
            }
            sidebar::Menu::Layout => {
                let main_window = self.main_window.id;

                let manage_pane = if let Some((window_id, pane_id)) = dashboard.focus {
                    let selected_pane_str =
                        if let Some(state) = dashboard.get_pane(main_window, window_id, pane_id) {
                            let link_group_name: String =
                                state.link_group.as_ref().map_or_else(String::new, |g| {
                                    " - Group ".to_string() + &g.to_string()
                                });

                            state.content.to_string() + &link_group_name
                        } else {
                            "".to_string()
                        };

                    let is_main_window = window_id == main_window;

                    let reset_pane_button = {
                        let btn = button(text("Reset").align_x(Alignment::Center))
                            .width(iced::Length::Fill);
                        if is_main_window {
                            let dashboard_msg = Message::Dashboard {
                                layout_id: None,
                                event: dashboard::Message::Pane(
                                    main_window,
                                    dashboard::pane::Message::ReplacePane(pane_id),
                                ),
                            };

                            btn.on_press(dashboard_msg)
                        } else {
                            btn
                        }
                    };
                    let split_pane_button = {
                        let btn = button(text("Split").align_x(Alignment::Center))
                            .width(iced::Length::Fill);
                        if is_main_window {
                            let dashboard_msg = Message::Dashboard {
                                layout_id: None,
                                event: dashboard::Message::Pane(
                                    main_window,
                                    dashboard::pane::Message::SplitPane(
                                        pane_grid::Axis::Horizontal,
                                        pane_id,
                                    ),
                                ),
                            };
                            btn.on_press(dashboard_msg)
                        } else {
                            btn
                        }
                    };

                    column![
                        text(selected_pane_str),
                        row![
                            tooltip(
                                reset_pane_button,
                                if is_main_window {
                                    Some("Reset selected pane")
                                } else {
                                    None
                                },
                                TooltipPosition::Top,
                            ),
                            tooltip(
                                split_pane_button,
                                if is_main_window {
                                    Some("Split selected pane horizontally")
                                } else {
                                    None
                                },
                                TooltipPosition::Top,
                            ),
                        ]
                        .spacing(8)
                    ]
                    .spacing(8)
                } else {
                    let reset_pane_button =
                        button(text("Reset").align_x(Alignment::Center)).width(iced::Length::Fill);
                    let split_pane_button =
                        button(text("Split").align_x(Alignment::Center)).width(iced::Length::Fill);

                    column![
                        text("No pane selected"),
                        row![
                            tooltip(reset_pane_button, None, TooltipPosition::Top),
                            tooltip(split_pane_button, None, TooltipPosition::Top),
                        ]
                        .spacing(8)
                    ]
                    .spacing(8)
                };

                let manage_layout_modal = {
                    let col = column![
                        manage_pane,
                        rule::horizontal(1.0).style(style::split_ruler),
                        self.layout_manager.view().map(Message::Layouts)
                    ]
                    .spacing(20);

                    container(col.align_x(Alignment::Center))
                        .width(260)
                        .padding(24)
                        .style(style::dashboard_modal)
                };

                let (align_x, padding) = match sidebar_pos {
                    sidebar::Position::Left => (Alignment::Start, padding::left(44).top(40)),
                    sidebar::Position::Right => (Alignment::End, padding::right(44).top(40)),
                };

                dashboard_modal(
                    base,
                    manage_layout_modal,
                    Message::Sidebar(screen::sidebar::Message::ToggleSidebarMenu(None)),
                    padding,
                    Alignment::Start,
                    align_x,
                )
            }
            sidebar::Menu::Audio => {
                let (align_x, padding) = match sidebar_pos {
                    sidebar::Position::Left => (Alignment::Start, padding::left(44).top(76)),
                    sidebar::Position::Right => (Alignment::End, padding::right(44).top(76)),
                };

                let trade_streams_list = dashboard.streams.trade_streams(None);

                dashboard_modal(
                    base,
                    self.audio_stream
                        .view(trade_streams_list)
                        .map(Message::AudioStream),
                    Message::Sidebar(screen::sidebar::Message::ToggleSidebarMenu(None)),
                    padding,
                    Alignment::Start,
                    align_x,
                )
            }
            sidebar::Menu::ThemeEditor => {
                let (align_x, padding) = match sidebar_pos {
                    sidebar::Position::Left => (Alignment::Start, padding::left(44).bottom(4)),
                    sidebar::Position::Right => (Alignment::End, padding::right(44).bottom(4)),
                };

                dashboard_modal(
                    base,
                    self.theme_editor
                        .view(self.theme.current())
                        .map(Message::ThemeEditor),
                    Message::Sidebar(screen::sidebar::Message::ToggleSidebarMenu(None)),
                    padding,
                    Alignment::End,
                    align_x,
                )
            }
            sidebar::Menu::IndicatorEditor => {
                let (align_x, padding) = match sidebar_pos {
                    sidebar::Position::Left => (Alignment::Start, padding::left(44).top(40)),
                    sidebar::Position::Right => (Alignment::End, padding::right(44).top(40)),
                };

                dashboard_modal(
                    base,
                    self.indicator_editor.view().map(Message::IndicatorEditor),
                    Message::Sidebar(screen::sidebar::Message::ToggleSidebarMenu(None)),
                    padding,
                    Alignment::Start,
                    align_x,
                )
            }
            sidebar::Menu::ExchangeAccounts => {
                let (align_x, padding) = match sidebar_pos {
                    sidebar::Position::Left => (Alignment::Start, padding::left(44).bottom(4)),
                    sidebar::Position::Right => (Alignment::End, padding::right(44).bottom(4)),
                };

                dashboard_modal(
                    base,
                    self.exchange_accounts
                        .view(&self.trading)
                        .map(Message::ExchangeAccounts),
                    Message::Sidebar(screen::sidebar::Message::ToggleSidebarMenu(None)),
                    padding,
                    Alignment::End,
                    align_x,
                )
            }
            sidebar::Menu::OrderTicket => {
                let (align_x, padding) = match sidebar_pos {
                    sidebar::Position::Left => (Alignment::Start, padding::left(44).bottom(4)),
                    sidebar::Position::Right => (Alignment::End, padding::right(44).bottom(4)),
                };

                let accounts = self.exchange_accounts.persisted().accounts;
                let signed_ok = self.proxy_snapshot_ok().is_ok();
                let ticket = self
                    .order_ticket
                    .view(&accounts, &self.trading, &self.trade_session, signed_ok)
                    .map(Message::OrderTicket);
                let picked = self
                    .order_ticket
                    .account()
                    .and_then(|id| accounts.iter().find(|m| m.id == id));
                let content: Element<'_, Message> = match picked {
                    Some(meta) => {
                        let gate = modal::order_ticket::gate_for(
                            Some(meta),
                            None,
                            "",
                            &self.trading,
                            &self.trade_session,
                            signed_ok,
                            self.order_ticket.is_preparing()
                                || self.account_panel.is_bulk_preparing(),
                        );
                        let blocked = modal::account_panel::bulk_blocked_reason(
                            &gate,
                            self.exchange_accounts.is_deleting(meta.id),
                            self.trading.bulk_running(meta.id),
                            self.trading.replace_busy(meta.id),
                            self.confirm_open(),
                        );
                        let panel = self
                            .account_panel
                            .view(meta, &accounts, &self.trading, blocked)
                            .map(Message::AccountPanel);
                        column![
                            ticket,
                            container(panel)
                                .max_width(360)
                                .padding(24)
                                .style(style::dashboard_modal)
                        ]
                        .spacing(4)
                        .into()
                    }
                    None => ticket,
                };
                dashboard_modal(
                    base,
                    content,
                    Message::Sidebar(screen::sidebar::Message::ToggleSidebarMenu(None)),
                    padding,
                    Alignment::End,
                    align_x,
                )
            }
            sidebar::Menu::Network => {
                let (align_x, padding) = match sidebar_pos {
                    sidebar::Position::Left => (Alignment::Start, padding::left(44).bottom(4)),
                    sidebar::Position::Right => (Alignment::End, padding::right(44).bottom(4)),
                };

                dashboard_modal(
                    base,
                    self.network_editor
                        .view(&self.network_config)
                        .map(Message::NetworkEditor),
                    Message::Sidebar(screen::sidebar::Message::ToggleSidebarMenu(None)),
                    padding,
                    Alignment::End,
                    align_x,
                )
            }
        }
    }

    fn save_state_to_disk(&mut self, windows: &HashMap<window::Id, WindowSpec>) {
        self.active_dashboard_mut()
            .popout
            .iter_mut()
            .for_each(|(id, (_, window_spec))| {
                if let Some(new_window_spec) = windows.get(id) {
                    *window_spec = *new_window_spec;
                }
            });

        for (id, spec) in windows {
            self.layout_manager
                .record_window(*id, Some(spec.position()), Some(spec.size()));
        }

        self.sidebar.sync_tickers_table_settings();
        self.layout_manager.sync_drawings(self.main_window.id);

        let mut ser_layouts = vec![];
        for layout in &self.layout_manager.layouts {
            if let Some(layout) = self.layout_manager.get(layout.id.unique) {
                let serialized_dashboard = data::Dashboard::from(&layout.dashboard);
                ser_layouts.push(data::Layout {
                    name: layout.id.name.clone(),
                    dashboard: serialized_dashboard,
                    detached: self.layout_manager.saved_placement(layout.id.unique),
                });
            }
        }

        let layouts = data::Layouts {
            chart_presets: self.layout_manager.presets().entries().to_vec(),
            drawings: self.layout_manager.chart_resources().drawings().to_config(),
            layouts: ser_layouts,
            active_layout: self
                .layout_manager
                .active_layout_id()
                .map(|layout| layout.name.to_string())
                .clone(),
        };

        let main_window_spec = windows
            .iter()
            .find(|(id, _)| **id == self.main_window.id)
            .map(|(_, spec)| *spec);

        let audio_cfg = data::AudioStream::from(&self.audio_stream);

        let state = data::State::from_parts(
            layouts,
            self.theme.data_theme(),
            self.theme_editor.custom_theme.clone().map(data::Theme),
            main_window_spec,
            self.timezone,
            self.sidebar.state.clone(),
            self.ui_scale_factor,
            audio_cfg,
            self.network_config.for_persistence(),
            self.exchange_accounts.persisted(),
            self.volume_size_unit,
            self.sidebar.cache_enabled(),
            self.antialiasing,
        );

        match serde_json::to_string(&state) {
            Ok(layout_str) => {
                let file_name = data::SAVED_STATE_PATH;
                if let Err(e) = data::write_json_to_file(&layout_str, file_name) {
                    log::error!("Failed to write layout state to file: {}", e);
                } else {
                    log::info!("Persisted state to {file_name}");
                }
            }
            Err(e) => log::error!("Failed to serialize layout: {}", e),
        }

        self.sidebar.persist_metadata_cache();
    }
}
