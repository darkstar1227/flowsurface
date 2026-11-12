pub mod pane;

use super::DashboardError;
use crate::layout::{ChartResourcesMessage, SharedChartResources};
use crate::{
    connector::{
        client::DataSources,
        fetcher::{self, FetchSpec, FetchedData},
        stream::ResolvedStream,
    },
    screen::sidebar::tickers_table::TickersTable,
    style,
    theme::ThemeState,
    widget::toast::Toast,
    window::{self, PixelRatio, Window},
};
use data::{
    UserTimezone,
    chart::kline::{ChartSettings, indicators::manifest},
    layout::{WindowSpec, pane::ContentKind, presets},
    stream::PersistStreamKind,
};
use exchange::{
    Kline, PushFrequency, StreamPairKind, TickerInfo, Trade, UnixMs,
    adapter::{
        AdapterHandles, MAX_KLINE_STREAMS_PER_STREAM, MAX_TRADE_TICKERS_PER_STREAM, StreamConfig,
        StreamKind, StreamTicksize, UniqueStreams,
    },
    depth::Depth,
};

use iced::{
    Element, Length, Subscription, Task, Vector,
    widget::{
        PaneGrid, center, container,
        pane_grid::{self, Configuration},
    },
};
use std::{collections::HashMap, time::Instant, vec};

#[derive(Debug, Clone)]
pub enum Message {
    Pane(window::Id, pane::Message),
    SavePopoutSpecs(HashMap<window::Id, WindowSpec>),
    ErrorOccurred(Option<uuid::Uuid>, DashboardError),
    Notification(Toast),
    DistributeFetchedData {
        pane_id: uuid::Uuid,
        stream: StreamKind,
        data: FetchedData,
    },
    ResolveStreams(uuid::Uuid, Vec<PersistStreamKind>),
    RequestPalette,
}

pub struct Dashboard {
    pub panes: pane_grid::State<pane::State>,
    pub focus: Option<(window::Id, pane_grid::Pane)>,
    pub popout: HashMap<window::Id, (pane_grid::State<pane::State>, WindowSpec)>,
    pub streams: UniqueStreams,
    pub chart_settings: ChartSettings,
}

impl Default for Dashboard {
    fn default() -> Self {
        Self {
            panes: pane_grid::State::with_configuration(Self::default_pane_config()),
            focus: None,
            streams: UniqueStreams::default(),
            popout: HashMap::new(),
            chart_settings: ChartSettings::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    OpenUrlRequested(String),
    OpenNetworkSettings,
    Notification(Toast),
    ResolveStreams {
        pane_id: uuid::Uuid,
        streams: Vec<PersistStreamKind>,
    },
    RequestPalette,
    PresetCommand {
        window: window::Id,
        pane: pane_grid::Pane,
        command: presets::Command,
    },
    IndicatorCatalog(manifest::Message),
    /// A pane asked for an order. `window` is where its confirmation opens: the pane's own
    /// window (main, popout or detached layout).
    TradeRequest {
        window: window::Id,
        order: crate::order_flow::PaneOrder,
    },
}

/// What the panes draw of the account's orders and positions (a view model, rebuilt when the
/// app state changes). It is never saved and never serialized. For now it holds what the ladder
/// title bar says about the account a command click would order with.
#[derive(Debug, Clone, Default)]
pub struct TradeOverlay {
    /// The saved accounts, with their alias (the alias is for display only).
    pub accounts: Vec<(crate::order_flow::AccountChoice, String)>,
    /// The account the order ticket shows now.
    pub ticket_account: Option<uuid::Uuid>,
    pub live_open: bool,
    /// The order and position lines of each account (read-only; see `order_lines`).
    pub lines: Vec<AccountLines>,
}

/// The lines one account has on its symbols.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountLines {
    pub account: uuid::Uuid,
    pub lines: Vec<crate::order_lines::SymbolLine>,
}

/// What the title bar says about a pane's trading account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountTag {
    pub text: String,
    /// A Live account, or a reason there is none: drawn in the warning colour.
    pub alert: bool,
}

impl TradeOverlay {
    /// The lines a pane of this market and symbol draws: those of the account a click in it
    /// would order with. Nothing when there is no such account (never a guess among several).
    pub fn lines_for(
        &self,
        market: exchange::adapter::Exchange,
        symbol: &str,
    ) -> Vec<crate::order_lines::OrderLine> {
        let choices: Vec<_> = self.accounts.iter().map(|(c, _)| *c).collect();
        let Ok(pick) = crate::order_flow::resolve_pane_account(
            market,
            self.ticket_account,
            &choices,
            self.live_open,
        ) else {
            return Vec::new();
        };
        self.lines
            .iter()
            .filter(|a| a.account == pick.id)
            .flat_map(|a| a.lines.iter())
            .filter(|l| l.symbol == symbol)
            .map(|l| l.line.clone())
            .collect()
    }

    /// The account a click in a pane of this market would order with, by the same rule the click
    /// uses (`resolve_pane_account`), or the short reason there is none. `None` when no account
    /// is saved or the market cannot be traded at all (nothing to say there).
    pub fn account_tag_of(&self, market: exchange::adapter::Exchange) -> Option<AccountTag> {
        use crate::order_flow::{AccountRefusal as R, resolve_pane_account};
        use exchange::account::AccountEnv;
        if self.accounts.is_empty() {
            return None;
        }
        let choices: Vec<_> = self.accounts.iter().map(|(c, _)| *c).collect();
        Some(
            match resolve_pane_account(market, self.ticket_account, &choices, self.live_open) {
                Ok(pick) => {
                    let alias = self
                        .accounts
                        .iter()
                        .find(|(c, _)| c.id == pick.id)
                        .map(|(_, a)| a.as_str())
                        .unwrap_or_default();
                    let live = pick.env == AccountEnv::Live;
                    AccountTag {
                        text: format!("{alias} · {}", if live { "LIVE" } else { "TESTNET" }),
                        alert: live,
                    }
                }
                Err(R::NotTradable) => return None,
                Err(R::NoAccount) => AccountTag {
                    text: "No account".to_string(),
                    alert: false,
                },
                Err(R::PickInTicket) => AccountTag {
                    text: "Pick account in ticket".to_string(),
                    alert: true,
                },
                Err(R::NotTested) => AccountTag {
                    text: "Account not tested".to_string(),
                    alert: true,
                },
                Err(R::LiveClosed) => AccountTag {
                    text: "LIVE closed".to_string(),
                    alert: true,
                },
            },
        )
    }

    /// The title-bar label for a pane of this market.
    pub fn account_tag<'a, M: 'a>(
        &self,
        market: exchange::adapter::Exchange,
    ) -> Option<Element<'a, M>> {
        let tag = self.account_tag_of(market)?;
        let label = iced::widget::text(tag.text).size(style::text_size::SMALL);
        Some(if tag.alert {
            label.style(iced::widget::text::danger).into()
        } else {
            label.into()
        })
    }
}

impl Dashboard {
    pub fn from_config(
        panes: Configuration<pane::State>,
        popout_windows: Vec<(Configuration<pane::State>, WindowSpec)>,
        chart_settings: ChartSettings,
    ) -> Self {
        let panes = pane_grid::State::with_configuration(panes);

        let mut popout = HashMap::new();

        if window::MULTI_WINDOW_SUPPORTED {
            for (pane, specs) in popout_windows {
                popout.insert(
                    window::Id::unique(),
                    (pane_grid::State::with_configuration(pane), specs),
                );
            }
        }

        Self {
            panes,
            focus: None,
            streams: UniqueStreams::default(),
            popout,
            chart_settings,
        }
    }

    pub fn load_layout(&mut self, main_window: window::Id) -> Task<Message> {
        debug_assert!(window::MULTI_WINDOW_SUPPORTED || self.popout.is_empty());

        let mut open_popouts_tasks: Vec<Task<Message>> = vec![];
        let mut new_popout = Vec::new();
        let mut keys_to_remove = Vec::new();

        for (old_window_id, (_, specs)) in &self.popout {
            keys_to_remove.push((*old_window_id, *specs));
        }

        // remove keys and open new windows
        for (old_window_id, window_spec) in keys_to_remove {
            let (window, task) = window::open(window::Settings {
                position: window::Position::Specific(window_spec.position()),
                size: window_spec.size(),
                exit_on_close_request: false,
                ..window::settings()
            });

            open_popouts_tasks.push(task.then(|_| Task::none()));

            if let Some((removed_pane, specs)) = self.popout.remove(&old_window_id) {
                new_popout.push((window, (removed_pane, specs)));
            }
        }

        // assign new windows to old panes
        for (window, (pane, specs)) in new_popout {
            self.popout.insert(window, (pane, specs));
        }

        Task::batch(open_popouts_tasks).chain(self.refresh_streams(main_window))
    }

    pub fn update(
        &mut self,
        message: Message,
        main_window: &Window,
        data_sources: &DataSources,
        chart_resources: &SharedChartResources,
    ) -> (Task<Message>, Option<Event>) {
        match message {
            Message::SavePopoutSpecs(specs) => {
                for (window_id, new_spec) in specs {
                    if let Some((_, spec)) = self.popout.get_mut(&window_id) {
                        *spec = new_spec;
                    }
                }
            }
            Message::ErrorOccurred(pane_id, err) => match pane_id {
                Some(id) => {
                    if let Some(state) = self.get_mut_pane_state_by_uuid(main_window.id, id) {
                        if let DashboardError::Fetch(msg, Some(req_id)) = &err {
                            if !state.content.apply_fetch_failure(*req_id) {
                                return (Task::none(), None);
                            }
                            log::error!("Fetch error: {msg}");
                        }
                        state.notifications.push(Toast::error(err.to_string()));
                    }
                }
                _ => {
                    return (
                        Task::none(),
                        Some(Event::Notification(Toast::error(err.to_string()))),
                    );
                }
            },
            Message::Pane(window, message) => match message {
                pane::Message::PaneClicked(pane) => {
                    self.set_focus(window, pane);
                }
                pane::Message::PaneResized(pane_grid::ResizeEvent { split, ratio }) => {
                    self.panes.resize(split, ratio);
                }
                pane::Message::PaneDragged(event) => {
                    if let pane_grid::DragEvent::Dropped { pane, target } = event {
                        self.panes.drop(pane, target);
                    }
                }
                pane::Message::SplitPane(axis, pane) => {
                    let focus_pane = if let Some((new_pane, _)) =
                        self.panes.split(axis, pane, pane::State::new())
                    {
                        Some(new_pane)
                    } else {
                        None
                    };

                    if let Some(focus_pane) = focus_pane {
                        self.set_focus(window, focus_pane);
                    }
                }
                pane::Message::ClosePane(pane) => {
                    if let Some((_, sibling)) = self.panes.close(pane) {
                        self.set_focus(window, sibling);
                    }
                }
                pane::Message::MaximizePane(pane) => {
                    self.panes.maximize(pane);
                }
                pane::Message::Restore => {
                    self.panes.restore();
                }
                pane::Message::ReplacePane(pane) => {
                    if let Some(pane) = self.panes.get_mut(pane) {
                        *pane = pane::State::new();
                    }

                    return (self.refresh_streams(main_window.id), None);
                }
                pane::Message::VisualConfigChanged(pane, cfg, to_sync) => {
                    if to_sync {
                        let snapshot =
                            self.get_pane(main_window.id, window, pane)
                                .and_then(|state| {
                                    state.settings_snapshot(data::PaneSettingsTransfer::Visual(
                                        Some(cfg.clone()),
                                    ))
                                });
                        if let Some(snapshot) = snapshot {
                            self.iter_all_panes_mut(main_window.id)
                                .for_each(|(_, _, state)| {
                                    let _ = state.apply_settings_snapshot(&snapshot);
                                });
                        }
                    } else if let Some(state) = self.get_mut_pane(main_window.id, window, pane) {
                        state.settings.visual_config = Some(cfg.clone());
                        state.content.change_visual_config(cfg);
                    }
                }
                pane::Message::SwitchLinkGroup(pane, group) => {
                    if group.is_none() {
                        if let Some(state) = self.get_mut_pane(main_window.id, window, pane) {
                            state.link_group = None;
                        }
                        return (Task::none(), None);
                    }

                    let maybe_ticker_info = self
                        .iter_all_panes(main_window.id)
                        .filter(|(w, p, _)| !(*w == window && *p == pane))
                        .find_map(|(_, _, other_state)| {
                            if other_state.link_group == group {
                                other_state.stream_pair()
                            } else {
                                None
                            }
                        });

                    let linked_content =
                        self.get_mut_pane(main_window.id, window, pane)
                            .and_then(|state| {
                                state.link_group = group;
                                state.modal = None;

                                if let Some(ticker_info) = maybe_ticker_info
                                    && state.stream_pair() != Some(ticker_info)
                                {
                                    Some((ticker_info, state.content.kind()))
                                } else {
                                    None
                                }
                            });

                    if let Some((ticker_info, content_kind)) = linked_content {
                        let task = self.init_pane(
                            data_sources,
                            main_window.id,
                            window,
                            pane,
                            ticker_info,
                            content_kind,
                        );
                        return (task, None);
                    }
                }
                pane::Message::Popout => {
                    return (self.popout_pane(main_window), None);
                }
                pane::Message::Merge => {
                    return (self.merge_pane(main_window), None);
                }
                pane::Message::ToggleInspector(pane, page) => {
                    self.set_focus(window, pane);
                    let task = self
                        .get_mut_pane(main_window.id, window, pane)
                        .and_then(|state| state.toggle_inspector(page))
                        .map(iced::widget::operation::focus)
                        .unwrap_or_else(Task::none);
                    return (task, None);
                }
                pane::Message::SyncKlineSettings(pane) => {
                    return (
                        self.sync_kline_settings(main_window.id, window, pane, data_sources),
                        None,
                    );
                }
                pane::Message::PaneEvent(pane, event) => {
                    let mut chart_settings = self.chart_settings;
                    let effect = self
                        .get_mut_pane(main_window.id, window, pane)
                        .map(|state| state.update(event, &mut chart_settings, chart_resources));
                    self.chart_settings = chart_settings;
                    return self.dispatch_pane_effect(
                        effect.flatten(),
                        main_window.id,
                        window,
                        pane,
                        data_sources,
                    );
                }
            },
            Message::RequestPalette => {
                return (Task::none(), Some(Event::RequestPalette));
            }
            Message::DistributeFetchedData {
                pane_id,
                data,
                stream,
            } => {
                return (
                    self.distribute_fetched_data(main_window.id, pane_id, data, stream),
                    None,
                );
            }
            Message::ResolveStreams(pane_id, streams) => {
                return (
                    Task::none(),
                    Some(Event::ResolveStreams { pane_id, streams }),
                );
            }
            Message::Notification(toast) => {
                return (Task::none(), Some(Event::Notification(toast)));
            }
        }

        (Task::none(), None)
    }

    fn new_pane(
        &mut self,
        axis: pane_grid::Axis,
        main_window: &Window,
        pane_state: Option<pane::State>,
    ) -> Task<Message> {
        if self
            .focus
            .filter(|(window, _)| *window == main_window.id)
            .is_some()
        {
            // If there is any focused pane on main window, split it
            return self.split_pane(axis, main_window);
        } else {
            // If there is no focused pane, split the last pane or create a new empty grid
            let pane = self.panes.iter().last().map(|(pane, _)| pane).copied();

            if let Some(pane) = pane {
                let result = self.panes.split(axis, pane, pane_state.unwrap_or_default());

                if let Some((pane, _)) = result {
                    return self.focus_pane(main_window.id, pane);
                }
            } else {
                let (state, pane) = pane_grid::State::new(pane_state.unwrap_or_default());
                self.panes = state;

                return self.focus_pane(main_window.id, pane);
            }
        }

        Task::none()
    }

    pub fn focused_pane(&self, main_window: window::Id) -> Option<&pane::State> {
        let (window, pane) = self.focus?;
        self.get_pane(main_window, window, pane)
    }

    fn set_focus(&mut self, window: window::Id, pane: pane_grid::Pane) {
        self.focus = Some((window, pane));
    }

    pub fn clear_focus(&mut self) {
        self.focus = None;
    }

    fn default_pane_config() -> Configuration<pane::State> {
        Configuration::Split {
            axis: pane_grid::Axis::Vertical,
            ratio: 0.8,
            a: Box::new(Configuration::Split {
                axis: pane_grid::Axis::Horizontal,
                ratio: 0.4,
                a: Box::new(Configuration::Split {
                    axis: pane_grid::Axis::Vertical,
                    ratio: 0.5,
                    a: Box::new(Configuration::Pane(pane::State::default())),
                    b: Box::new(Configuration::Pane(pane::State::default())),
                }),
                b: Box::new(Configuration::Split {
                    axis: pane_grid::Axis::Vertical,
                    ratio: 0.5,
                    a: Box::new(Configuration::Pane(pane::State::default())),
                    b: Box::new(Configuration::Pane(pane::State::default())),
                }),
            }),
            b: Box::new(Configuration::Pane(pane::State::default())),
        }
    }

    fn focus_pane(&mut self, window: window::Id, pane: pane_grid::Pane) -> Task<Message> {
        if self.focus != Some((window, pane)) {
            self.set_focus(window, pane);
        }

        Task::none()
    }

    fn split_pane(&mut self, axis: pane_grid::Axis, main_window: &Window) -> Task<Message> {
        if let Some((window, pane)) = self.focus
            && window == main_window.id
        {
            let result = self.panes.split(axis, pane, pane::State::new());

            if let Some((pane, _)) = result {
                return self.focus_pane(main_window.id, pane);
            }
        }

        Task::none()
    }

    fn popout_pane(&mut self, main_window: &Window) -> Task<Message> {
        if window::MULTI_WINDOW_SUPPORTED
            && let Some((_, id)) = self.focus.take()
            && let Some((pane, _)) = self.panes.close(id)
        {
            let (window, task) = window::open(window::Settings {
                position: main_window
                    .position
                    .map(|point| window::Position::Specific(point + Vector::new(20.0, 20.0)))
                    .unwrap_or_default(),
                exit_on_close_request: false,
                min_size: Some(iced::Size::new(400.0, 300.0)),
                ..window::settings()
            });

            let (state, id) = pane_grid::State::new(pane);
            self.popout.insert(window, (state, WindowSpec::default()));

            return task.then(move |window| {
                Task::done(Message::Pane(window, pane::Message::PaneClicked(id)))
            });
        }

        Task::none()
    }

    fn merge_pane(&mut self, main_window: &Window) -> Task<Message> {
        if let Some((window, pane)) = self.focus.take()
            && let Some(pane_state) = self
                .popout
                .remove(&window)
                .and_then(|(mut panes, _)| panes.panes.remove(&pane))
        {
            let task = self.new_pane(pane_grid::Axis::Horizontal, main_window, Some(pane_state));

            return Task::batch(vec![window::close(window), task]);
        }

        Task::none()
    }

    /// Moves every popped-out pane back into the main grid.
    ///
    /// Returns the windows the panes were in, which the caller closes if they are still open.
    pub fn merge_all_popouts(&mut self, main_window: &Window) -> (Vec<window::Id>, Task<Message>) {
        let windows = self.popout.keys().copied().collect::<Vec<_>>();
        let mut tasks = vec![];

        for window in &windows {
            let Some((state, _)) = self.popout.remove(window) else {
                continue;
            };
            for (_, pane_state) in state.panes {
                // A focused main-grid pane would be split into a blank pane instead.
                self.focus = None;
                tasks.push(self.new_pane(
                    pane_grid::Axis::Horizontal,
                    main_window,
                    Some(pane_state),
                ));
            }
        }
        self.focus = None;

        (windows, Task::batch(tasks))
    }

    pub fn get_pane(
        &self,
        main_window: window::Id,
        window: window::Id,
        pane: pane_grid::Pane,
    ) -> Option<&pane::State> {
        if main_window == window {
            self.panes.get(pane)
        } else {
            self.popout
                .get(&window)
                .and_then(|(panes, _)| panes.get(pane))
        }
    }

    fn get_mut_pane(
        &mut self,
        main_window: window::Id,
        window: window::Id,
        pane: pane_grid::Pane,
    ) -> Option<&mut pane::State> {
        if main_window == window {
            self.panes.get_mut(pane)
        } else {
            self.popout
                .get_mut(&window)
                .and_then(|(panes, _)| panes.get_mut(pane))
        }
    }

    fn get_mut_pane_state_by_uuid(
        &mut self,
        main_window: window::Id,
        uuid: uuid::Uuid,
    ) -> Option<&mut pane::State> {
        self.iter_all_panes_mut(main_window)
            .find(|(_, _, state)| state.unique_id() == uuid)
            .map(|(_, _, state)| state)
    }

    fn iter_all_panes(
        &self,
        main_window: window::Id,
    ) -> impl Iterator<Item = (window::Id, pane_grid::Pane, &pane::State)> {
        self.panes
            .iter()
            .map(move |(pane, state)| (main_window, *pane, state))
            .chain(self.popout.iter().flat_map(|(window_id, (panes, _))| {
                panes.iter().map(|(pane, state)| (*window_id, *pane, state))
            }))
    }

    /// Gives every chart and order book of this dashboard its order lines. Cheap when nothing
    /// changed: a pane compares and redraws only for a change.
    pub fn apply_trade_overlay(&mut self, main_window: window::Id, overlay: &TradeOverlay) {
        for (_, _, state) in self.iter_all_panes_mut(main_window) {
            state.content.set_order_lines(overlay);
        }
    }

    fn iter_all_panes_mut(
        &mut self,
        main_window: window::Id,
    ) -> impl Iterator<Item = (window::Id, pane_grid::Pane, &mut pane::State)> {
        self.panes
            .iter_mut()
            .map(move |(pane, state)| (main_window, *pane, state))
            .chain(self.popout.iter_mut().flat_map(|(window_id, (panes, _))| {
                panes
                    .iter_mut()
                    .map(|(pane, state)| (*window_id, *pane, state))
            }))
    }

    pub fn view<'a>(
        &'a self,
        main_window: &'a Window,
        tickers_table: &'a TickersTable,
        timezone: UserTimezone,
        pixel_ratio: PixelRatio,
        theme: &'a ThemeState,
        chart_resources: &'a SharedChartResources,
        overlay: &'a TradeOverlay,
    ) -> Element<'a, Message> {
        let pane_grid: Element<_> = PaneGrid::new(&self.panes, |id, pane, maximized| {
            let is_focused = self.focus == Some((main_window.id, id));
            pane.view(
                id,
                self.panes.len(),
                is_focused,
                maximized,
                main_window.id,
                main_window,
                timezone,
                pixel_ratio,
                tickers_table,
                theme,
                chart_resources,
                overlay,
            )
        })
        .min_size(240)
        .on_click(pane::Message::PaneClicked)
        .on_drag(pane::Message::PaneDragged)
        .on_resize(8, pane::Message::PaneResized)
        .spacing(6)
        .style(style::pane_grid)
        .into();

        pane_grid.map(move |message| Message::Pane(main_window.id, message))
    }

    pub fn view_window<'a>(
        &'a self,
        window: window::Id,
        main_window: &'a Window,
        tickers_table: &'a TickersTable,
        timezone: UserTimezone,
        pixel_ratio: PixelRatio,
        theme: &'a ThemeState,
        chart_resources: &'a SharedChartResources,
        overlay: &'a TradeOverlay,
    ) -> Element<'a, Message> {
        if let Some((state, _)) = self.popout.get(&window) {
            let content = container(
                PaneGrid::new(state, |id, pane, _maximized| {
                    let is_focused = self.focus == Some((window, id));
                    pane.view(
                        id,
                        state.len(),
                        is_focused,
                        false,
                        window,
                        main_window,
                        timezone,
                        pixel_ratio,
                        tickers_table,
                        theme,
                        chart_resources,
                        overlay,
                    )
                })
                .on_click(pane::Message::PaneClicked),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(8);

            Element::new(content).map(move |message| Message::Pane(window, message))
        } else {
            Element::new(center("No pane found for window"))
                .map(move |message| Message::Pane(window, message))
        }
    }

    pub fn go_back(&mut self, main_window: window::Id) -> bool {
        let Some((window, pane)) = self.focus else {
            return false;
        };

        let Some(state) = self.get_mut_pane(main_window, window, pane) else {
            return false;
        };

        if state.modal.is_some() {
            state.modal = None;
            return true;
        }

        state.go_back()
    }

    fn handle_error(
        &mut self,
        pane_id: Option<uuid::Uuid>,
        err: &DashboardError,
        main_window: window::Id,
    ) -> Task<Message> {
        match pane_id {
            Some(id) => {
                if let Some(state) = self.get_mut_pane_state_by_uuid(main_window, id) {
                    state.notifications.push(Toast::error(err.to_string()));
                }
                Task::none()
            }
            _ => Task::done(Message::Notification(Toast::error(err.to_string()))),
        }
    }

    /// Borrow visual settings from an existing pane of the same content kind.
    ///
    /// `exclude` is the pane that is being initialized, it is skipped so that
    /// a freshly-set placeholder (from `ContentSelected`) doesn't shadow an
    /// already-configured pane of the same kind.
    fn borrow_synced_settings(
        &self,
        main_window: window::Id,
        content_kind: ContentKind,
        exclude: Option<(window::Id, pane_grid::Pane)>,
    ) -> Option<data::PaneSettingsSnapshot> {
        self.iter_all_panes(main_window)
            .find_map(|(win, pane, state)| {
                if state.content.kind() == content_kind && (exclude != Some((win, pane))) {
                    state.settings_snapshot(data::PaneSettingsTransfer::Visual(
                        state.settings.visual_config.clone(),
                    ))
                } else {
                    None
                }
            })
    }

    fn init_pane(
        &mut self,
        data_sources: &DataSources,
        main_window: window::Id,
        window: window::Id,
        selected_pane: pane_grid::Pane,
        ticker_info: TickerInfo,
        content_kind: ContentKind,
    ) -> Task<Message> {
        let synced =
            self.borrow_synced_settings(main_window, content_kind, Some((window, selected_pane)));

        let (streams, task) = {
            let Some(state) = self.get_mut_pane(main_window, window, selected_pane) else {
                return Task::none();
            };

            if let Some(cfg) = synced
                .as_ref()
                .and_then(data::PaneSettingsSnapshot::visual_config)
                && (state.settings.visual_config.is_none() || state.content.kind() != content_kind)
            {
                state.settings.visual_config = Some(cfg);
            } else if state.content.kind() != content_kind {
                state.settings.visual_config = None;
            }

            let streams = state.set_content_and_streams(vec![ticker_info], content_kind);

            if let Some(snapshot) = &synced {
                let _ = state.content.apply_settings_snapshot(snapshot);
            }

            let task = match state.invalidate(Instant::now()) {
                Some(pane::Action::Chart(pane::ChartAction::RequestFetch(requests))) => state
                    .request_fetch(data_sources, requests)
                    .map(Message::from),
                _ => Task::none(),
            };

            (streams, task)
        };

        self.streams.extend(streams.iter());
        task
    }

    pub fn init_focused_pane(
        &mut self,
        data_sources: &DataSources,
        main_window: window::Id,
        ticker_info: TickerInfo,
        content_kind: ContentKind,
    ) -> Task<Message> {
        let initial_pane = if self.focus.is_none() && self.panes.len() == 1 {
            self.panes.iter().next().map(|(pane_id, _)| *pane_id)
        } else {
            None
        };
        if let Some(pane_id) = initial_pane {
            self.set_focus(main_window, pane_id);
        }

        let Some((window, selected_pane)) = self.focus else {
            return Task::done(Message::Notification(Toast::warn(
                "No focused pane found".to_string(),
            )));
        };

        if let Some(state) = self.get_mut_pane(main_window, window, selected_pane) {
            let previous_ticker = state.stream_pair();
            if previous_ticker.is_some() && previous_ticker != Some(ticker_info) {
                state.link_group = None;
            }
        } else {
            return Task::done(Message::Notification(Toast::warn(
                "No focused pane found".to_string(),
            )));
        }

        self.init_pane(
            data_sources,
            main_window,
            window,
            selected_pane,
            ticker_info,
            content_kind,
        )
    }

    pub fn switch_tickers_in_group(
        &mut self,
        data_sources: &DataSources,
        main_window: window::Id,
        ticker_info: TickerInfo,
    ) -> Task<Message> {
        let initial_pane = if self.focus.is_none() && self.panes.len() == 1 {
            self.panes.iter().next().map(|(pane_id, _)| *pane_id)
        } else {
            None
        };
        if let Some(pane_id) = initial_pane {
            self.set_focus(main_window, pane_id);
        }

        let link_group = self.focus.and_then(|(window, pane)| {
            self.get_pane(main_window, window, pane)
                .and_then(|state| state.link_group)
        });

        if let Some(group) = link_group {
            let pane_infos: Vec<(window::Id, pane_grid::Pane, ContentKind)> = self
                .iter_all_panes_mut(main_window)
                .filter_map(|(window, pane, state)| {
                    if state.link_group == Some(group) {
                        Some((window, pane, state.content.kind()))
                    } else {
                        None
                    }
                })
                .collect();

            let tasks: Vec<Task<Message>> = pane_infos
                .iter()
                .map(|(window, pane, content_kind)| {
                    self.init_pane(
                        data_sources,
                        main_window,
                        *window,
                        *pane,
                        ticker_info,
                        *content_kind,
                    )
                })
                .collect();

            Task::batch(tasks)
        } else if let Some((window, pane)) = self.focus {
            if let Some(state) = self.get_mut_pane(main_window, window, pane) {
                let content_kind = state.content.kind();
                self.init_focused_pane(data_sources, main_window, ticker_info, content_kind)
            } else {
                Task::done(Message::Notification(Toast::warn(
                    "Couldn't get focused pane's content".to_string(),
                )))
            }
        } else {
            Task::done(Message::Notification(Toast::warn(
                "No link group or focused pane found".to_string(),
            )))
        }
    }

    pub fn distribute_fetched_data(
        &mut self,
        main_window: window::Id,
        pane_id: uuid::Uuid,
        data: FetchedData,
        stream_type: StreamKind,
    ) -> Task<Message> {
        match data {
            FetchedData::Trades {
                batch,
                req_id,
                until_time,
                is_batches_done,
                has_data,
            } => {
                if batch.is_empty() {
                    if is_batches_done && has_data {
                        if let Err(reason) = self.insert_fetched_trades(
                            main_window,
                            pane_id,
                            stream_type.ticker_info(),
                            &batch,
                            true,
                            req_id,
                        ) {
                            return self.handle_error(Some(pane_id), &reason, main_window);
                        }
                    } else if let Some(pane_state) =
                        self.get_mut_pane_state_by_uuid(main_window, pane_id)
                        && let Some(req_id) = req_id
                    {
                        pane_state
                            .content
                            .apply_trade_fetch_no_data(stream_type.ticker_info(), req_id);
                    }
                } else {
                    let filtered_batch = batch
                        .into_iter()
                        .filter(|trade| trade.time <= until_time)
                        .collect::<Vec<_>>();

                    if let Err(reason) = self.insert_fetched_trades(
                        main_window,
                        pane_id,
                        stream_type.ticker_info(),
                        &filtered_batch,
                        is_batches_done,
                        req_id,
                    ) {
                        return self.handle_error(Some(pane_id), &reason, main_window);
                    }
                }
            }
            FetchedData::Klines {
                data,
                req_id,
                is_batches_done,
                has_data,
            } => {
                if let Some(pane_state) = self.get_mut_pane_state_by_uuid(main_window, pane_id)
                    && let StreamKind::Kline {
                        timeframe,
                        ticker_info,
                    } = stream_type
                {
                    pane_state.insert_hist_klines(
                        req_id,
                        timeframe,
                        ticker_info,
                        &data,
                        is_batches_done,
                        has_data,
                    );
                }
            }
            FetchedData::OpenInterest { series, req_id } => {
                if let Some(pane_state) = self.get_mut_pane_state_by_uuid(main_window, pane_id)
                    && let StreamKind::Kline { ticker_info, .. } = stream_type
                {
                    pane_state.insert_hist_open_interest(req_id, ticker_info, &series);
                }
            }
        }

        Task::none()
    }

    fn insert_fetched_trades(
        &mut self,
        main_window: window::Id,
        pane_id: uuid::Uuid,
        ticker_info: TickerInfo,
        trades: &[Trade],
        is_batches_done: bool,
        req_id: Option<uuid::Uuid>,
    ) -> Result<(), DashboardError> {
        let Some(pane_state) = self.get_mut_pane_state_by_uuid(main_window, pane_id) else {
            return if req_id.is_some() {
                Ok(())
            } else {
                Err(DashboardError::Unknown(
                    "No matching pane state found for fetched trades".to_string(),
                ))
            };
        };

        pane_state
            .content
            .insert_historical_trades(ticker_info, trades, is_batches_done, req_id)
            .map_err(|reason| DashboardError::Unknown(reason.to_string()))
    }

    pub fn ingest_live_kline(
        &mut self,
        stream: &StreamKind,
        kline: &Kline,
        main_window: window::Id,
    ) -> Task<Message> {
        let mut found_match = false;

        self.iter_all_panes_mut(main_window)
            .for_each(|(_, _, pane_state)| {
                if pane_state.matches_stream(stream) {
                    pane_state.content.ingest_live_kline(stream, kline);
                    found_match = true;
                }
            });

        if found_match {
            Task::none()
        } else {
            log::debug!("{stream:?} stream had no matching panes - dropping");
            self.refresh_streams(main_window)
        }
    }

    pub fn ingest_live_depth(
        &mut self,
        stream: &StreamKind,
        update_t: UnixMs,
        depth: std::sync::Arc<Depth>,
        main_window: window::Id,
    ) -> Task<Message> {
        let mut found_match = false;

        self.iter_all_panes_mut(main_window)
            .for_each(|(_, _, pane_state)| {
                if pane_state.matches_stream(stream) {
                    pane_state
                        .content
                        .ingest_live_depth(stream, &depth, update_t);
                    found_match = true;
                }
            });

        if found_match {
            Task::none()
        } else {
            self.refresh_streams(main_window)
        }
    }

    pub fn ingest_live_trades(
        &mut self,
        stream: &StreamKind,
        buffer: &[Trade],
        update_t: UnixMs,
        main_window: window::Id,
    ) -> Task<Message> {
        let mut found_match = false;

        self.iter_all_panes_mut(main_window)
            .for_each(|(_, _, pane_state)| {
                if pane_state.matches_stream(stream) {
                    pane_state
                        .content
                        .ingest_live_trades(stream, buffer, update_t);
                    found_match = true;
                }
            });

        if found_match {
            Task::none()
        } else {
            self.refresh_streams(main_window)
        }
    }

    pub fn sync_drawings(
        &mut self,
        main_window: window::Id,
        library: &mut data::layout::drawings::Library,
    ) {
        for (_, _, state) in self.iter_all_panes_mut(main_window) {
            state.sync_drawings(library);
        }
    }

    /// Rebuild every chart that uses `indicator` after the indicator catalog was reloaded.
    pub fn reload_indicator(
        &mut self,
        main_window: window::Id,
        indicator: &data::chart::kline::IndicatorId,
        data_sources: &DataSources,
    ) -> Task<Message> {
        let now = Instant::now();
        let tasks = self
            .iter_all_panes_mut(main_window)
            .filter_map(|(_, _, state)| state.reload_indicator(indicator, data_sources, now))
            .map(|task| task.map(Message::from))
            .collect::<Vec<_>>();
        Task::batch(tasks)
    }

    pub fn invalidate_all_panes(&mut self, main_window: window::Id) {
        self.iter_all_panes_mut(main_window)
            .for_each(|(_, _, state)| {
                let _ = state.invalidate(Instant::now());
            });
    }

    pub fn park_for_inactive_layout(&mut self, main_window: window::Id) {
        self.iter_all_panes_mut(main_window)
            .for_each(|(_, _, state)| state.park_for_inactive_layout());
    }

    pub fn tick(
        &mut self,
        now: Instant,
        data_sources: &DataSources,
        main_window: window::Id,
    ) -> Task<Message> {
        let mut tasks = vec![];

        let mut tick_state =
            |window: window::Id, pane_id: pane_grid::Pane, state: &mut pane::State| {
                let (action, indicator_task) = state.tick(now, pane_id);
                match action {
                    Some(pane::Action::Chart(action)) => match action {
                        pane::ChartAction::RequestFetch(reqs) => {
                            tasks.push(state.request_fetch(data_sources, reqs).map(Message::from));
                        }
                        pane::ChartAction::RequestPalette => {
                            tasks.push(Task::done(Message::RequestPalette));
                        }
                    },
                    Some(pane::Action::ResolveStreams(streams)) => {
                        tasks.push(Task::done(Message::ResolveStreams(
                            state.unique_id(),
                            streams,
                        )));
                    }
                    Some(pane::Action::ResolveContent) => match state.stream_pair_kind() {
                        Some(StreamPairKind::MultiSource(tickers)) => {
                            state.set_content_and_streams(tickers, state.content.kind());
                        }
                        Some(StreamPairKind::SingleSource(ticker)) => {
                            state.set_content_and_streams(vec![ticker], state.content.kind());
                        }
                        None => {}
                    },
                    None => {}
                }

                tasks.push(indicator_task.map(move |message| Message::Pane(window, message)));
            };

        // tick only the maximized pane if there is any, otherwise tick all panes
        let maximized_pane = self.panes.maximized();
        for (pane_id, state) in self.panes.iter_mut() {
            if maximized_pane.is_some_and(|maximized| *pane_id != maximized) {
                continue;
            }

            tick_state(main_window, *pane_id, state);
        }

        for (window, (popout_state, _)) in self.popout.iter_mut() {
            for (pane_id, state) in popout_state.iter_mut() {
                tick_state(*window, *pane_id, state);
            }
        }

        Task::batch(tasks)
    }

    pub fn resolve_streams(
        &mut self,
        main_window: window::Id,
        pane_id: uuid::Uuid,
        streams: Vec<StreamKind>,
    ) -> Task<Message> {
        if let Some(state) = self.get_mut_pane_state_by_uuid(main_window, pane_id) {
            state.streams = ResolvedStream::Ready(streams.clone());
            if matches!(&state.content, pane::Content::Kline { .. }) {
                state.streams.canonicalize_kline_trade_streams();
            }
        }
        self.refresh_streams(main_window)
    }

    pub fn block_streams(&mut self, main_window: window::Id, pane_id: uuid::Uuid, reason: String) {
        if let Some(state) = self.get_mut_pane_state_by_uuid(main_window, pane_id) {
            match &mut state.streams {
                ResolvedStream::Waiting { streams, .. } => {
                    state.streams = ResolvedStream::Blocked {
                        streams: streams.clone(),
                        reason,
                        last_attempt: None,
                    };
                }
                ResolvedStream::Blocked {
                    reason: old_reason, ..
                } => {
                    *old_reason = reason;
                }
                _ => {}
            }
        }
    }

    pub fn market_subscriptions(
        streams: &UniqueStreams,
        handles: &AdapterHandles,
    ) -> Subscription<exchange::Event> {
        let unique_streams = streams
            .combined_used()
            .flat_map(|(exchange, specs)| {
                let mut subs = vec![];

                if !specs.depth.is_empty() {
                    let depth_subs = specs
                        .depth
                        .iter()
                        .map(|(ticker, aggr, push_freq)| {
                            let tick_mltp = match aggr {
                                StreamTicksize::Client => None,
                                StreamTicksize::ServerSide(tick_mltp) => Some(*tick_mltp),
                            };

                            let config = StreamConfig::new(
                                *ticker,
                                ticker.exchange(),
                                tick_mltp,
                                *push_freq,
                            );

                            let data = (handles.clone(), config);
                            Subscription::run_with(data, |data| data.0.depth_stream(&data.1))
                        })
                        .collect::<Vec<_>>();

                    if !depth_subs.is_empty() {
                        subs.push(Subscription::batch(depth_subs));
                    }
                }

                if !specs.trade.is_empty() {
                    let trade_subs = specs
                        .trade
                        .chunks(MAX_TRADE_TICKERS_PER_STREAM)
                        .map(|tickers| {
                            let config = StreamConfig::new(
                                tickers.to_vec(),
                                exchange,
                                None,
                                PushFrequency::ServerDefault,
                            );

                            let data = (handles.clone(), config);
                            Subscription::run_with(data, |data| data.0.trade_stream(&data.1))
                        })
                        .collect::<Vec<_>>();

                    if !trade_subs.is_empty() {
                        subs.push(Subscription::batch(trade_subs));
                    }
                }

                if !specs.kline.is_empty() {
                    let kline_subs = specs
                        .kline
                        .chunks(MAX_KLINE_STREAMS_PER_STREAM)
                        .map(|streams| {
                            let config = StreamConfig::new(
                                streams.to_vec(),
                                exchange,
                                None,
                                PushFrequency::ServerDefault,
                            );

                            let data = (handles.clone(), config);
                            Subscription::run_with(data, |data| data.0.kline_stream(&data.1))
                        })
                        .collect::<Vec<_>>();

                    if !kline_subs.is_empty() {
                        subs.push(Subscription::batch(kline_subs));
                    }
                }

                subs
            })
            .collect::<Vec<Subscription<exchange::Event>>>();

        Subscription::batch(unique_streams)
    }

    pub fn theme_updated(&mut self, main_window: window::Id, theme: &iced_core::Theme) {
        self.iter_all_panes_mut(main_window)
            .for_each(|(_, _, state)| {
                state.content.update_theme(theme);
            });
    }

    fn apply_settings_snapshot_to_pane(
        &mut self,
        main_window: window::Id,
        window: window::Id,
        pane: pane_grid::Pane,
        snapshot: &data::PaneSettingsSnapshot,
        data_sources: &DataSources,
    ) -> Task<Message> {
        let Some(state) = self.get_mut_pane(main_window, window, pane) else {
            return Task::none();
        };

        match state.apply_settings_snapshot(snapshot) {
            Some(requests) => self.request_fetch(main_window, window, pane, requests, data_sources),
            None => self.refresh_streams(main_window),
        }
    }

    fn request_fetch(
        &mut self,
        main_window: window::Id,
        window: window::Id,
        pane: pane_grid::Pane,
        requests: Vec<FetchSpec>,
        data_sources: &DataSources,
    ) -> Task<Message> {
        let Some(state) = self.get_mut_pane(main_window, window, pane) else {
            return Task::none();
        };

        state
            .request_fetch(data_sources, requests)
            .map(Message::from)
            .chain(self.refresh_streams(main_window))
    }

    pub fn reconcile_selected_presets(
        &mut self,
        main_window: window::Id,
        presets: &presets::Library,
    ) {
        for (_, _, state) in self.iter_all_panes_mut(main_window) {
            state.clear_missing_preset(presets);
        }
    }

    pub fn preset_command_completed(
        &mut self,
        main_window: window::Id,
        window: window::Id,
        pane: pane_grid::Pane,
        response: Result<Option<uuid::Uuid>, String>,
    ) {
        if let Some(state) = self.get_mut_pane(main_window, window, pane) {
            state.preset_command_completed(response);
        }
    }

    fn dispatch_pane_effect(
        &mut self,
        effect: Option<pane::Effect>,
        main_window: window::Id,
        window: window::Id,
        pane: pane_grid::Pane,
        data_sources: &DataSources,
    ) -> (Task<Message>, Option<Event>) {
        match effect {
            Some(pane::Effect::RefreshStreams) => (self.refresh_streams(main_window), None),
            Some(pane::Effect::RequestFetch(requests)) => (
                self.request_fetch(main_window, window, pane, requests, data_sources),
                None,
            ),
            Some(pane::Effect::SwitchTickersInGroup(ticker_info)) => (
                self.switch_tickers_in_group(data_sources, main_window, ticker_info),
                None,
            ),
            Some(pane::Effect::FocusWidget(id)) => (iced::widget::operation::focus(id), None),
            Some(pane::Effect::OpenNetworkSettings) => {
                (Task::none(), Some(Event::OpenNetworkSettings))
            }
            Some(pane::Effect::OpenUrl(url)) => (Task::none(), Some(Event::OpenUrlRequested(url))),
            Some(pane::Effect::SharedChartResources(message)) => match *message {
                ChartResourcesMessage::Presets(command) => (
                    Task::none(),
                    Some(Event::PresetCommand {
                        window,
                        pane,
                        command,
                    }),
                ),
                ChartResourcesMessage::IndicatorCatalog(message) => {
                    (Task::none(), Some(Event::IndicatorCatalog(message)))
                }
            },
            Some(pane::Effect::TradeRequest(order)) => {
                (Task::none(), Some(Event::TradeRequest { window, order }))
            }
            None => (Task::none(), None),
        }
    }

    fn sync_kline_settings(
        &mut self,
        main_window: window::Id,
        source_window: window::Id,
        source_pane: pane_grid::Pane,
        data_sources: &DataSources,
    ) -> Task<Message> {
        let Some(source) = self.get_pane(main_window, source_window, source_pane) else {
            return Task::none();
        };
        let Some(snapshot) = source.settings_snapshot(data::PaneSettingsTransfer::Kline) else {
            return Task::none();
        };

        let targets = self
            .iter_all_panes(main_window)
            .filter(|(_, _, state)| {
                matches!(state.content, pane::Content::Kline { chart: Some(_), .. })
            })
            .map(|(window, pane, _)| (window, pane))
            .collect::<Vec<_>>();

        Task::batch(targets.into_iter().map(|(window, pane)| {
            self.apply_settings_snapshot_to_pane(main_window, window, pane, &snapshot, data_sources)
        }))
    }

    fn refresh_streams(&mut self, main_window: window::Id) -> Task<Message> {
        let all_pane_streams = self
            .iter_all_panes(main_window)
            .flat_map(|(_, _, pane_state)| pane_state.streams.ready_iter().into_iter().flatten());
        self.streams = UniqueStreams::from(all_pane_streams);

        Task::none()
    }
}

impl From<fetcher::FetchUpdate> for Message {
    fn from(update: fetcher::FetchUpdate) -> Self {
        match update {
            fetcher::FetchUpdate::Data {
                pane_id,
                stream,
                data,
            } => Message::DistributeFetchedData {
                pane_id,
                stream,
                data,
            },
            fetcher::FetchUpdate::Error {
                pane_id,
                error,
                req_id,
            } => Message::ErrorOccurred(Some(pane_id), DashboardError::Fetch(error, req_id)),
        }
    }
}

#[cfg(test)]
mod overlay_tests {
    use super::*;
    use crate::order_flow::AccountChoice;
    use exchange::account::{AccountEnv, AccountProduct, AccountVenue};
    use exchange::adapter::Exchange;

    fn acc(n: u128, env: AccountEnv, tested: bool) -> (AccountChoice, String) {
        (
            AccountChoice {
                id: uuid::Uuid::from_u128(n),
                venue: AccountVenue::Binance,
                product: AccountProduct::UsdM,
                env,
                tested,
            },
            format!("acct{n}"),
        )
    }

    /// The title bar follows the click's own rule: the name and TESTNET / LIVE, or the reason.
    #[test]
    fn the_title_tag_names_the_account_or_says_why_not() {
        let m = Exchange::BinanceLinear;
        let none = TradeOverlay::default();
        assert_eq!(none.account_tag_of(m), None);
        let one = TradeOverlay {
            accounts: vec![acc(1, AccountEnv::Testnet, true)],
            live_open: false,
            ..Default::default()
        };
        let tag = one.account_tag_of(m).unwrap();
        assert_eq!(tag.text, "acct1 · TESTNET");
        assert!(!tag.alert);
        let many = TradeOverlay {
            accounts: vec![
                acc(1, AccountEnv::Testnet, true),
                acc(2, AccountEnv::Testnet, true),
            ],
            ..Default::default()
        };
        assert_eq!(
            many.account_tag_of(m).unwrap().text,
            "Pick account in ticket"
        );
        let picked = TradeOverlay {
            ticket_account: Some(uuid::Uuid::from_u128(2)),
            ..many
        };
        assert_eq!(picked.account_tag_of(m).unwrap().text, "acct2 · TESTNET");
        let live = TradeOverlay {
            accounts: vec![acc(3, AccountEnv::Live, true)],
            live_open: true,
            ..Default::default()
        };
        let tag = live.account_tag_of(m).unwrap();
        assert_eq!(tag.text, "acct3 · LIVE");
        assert!(tag.alert);
        let closed = TradeOverlay {
            live_open: false,
            ..live
        };
        assert_eq!(closed.account_tag_of(m).unwrap().text, "LIVE closed");
        // Another venue's market: nothing to say.
        assert_eq!(one.account_tag_of(Exchange::HyperliquidLinear), None);
    }

    fn line(symbol: &str, price: &str) -> crate::order_lines::SymbolLine {
        use exchange::trade::{Decimal, Side};
        crate::order_lines::SymbolLine {
            symbol: symbol.to_string(),
            line: crate::order_lines::OrderLine {
                price: Decimal::parse(price).unwrap(),
                kind: crate::order_lines::LineKind::Stop,
                side: Side::Sell,
                qty: None,
                whole: true,
                stale: false,
            },
        }
    }

    /// A pane draws the lines of its own symbol and of the account a click would use; with
    /// several accounts and none picked it draws none.
    #[test]
    fn a_pane_draws_only_its_symbol_and_its_account() {
        let m = Exchange::BinanceLinear;
        let lines = vec![
            AccountLines {
                account: uuid::Uuid::from_u128(1),
                lines: vec![line("BTCUSDT", "58000"), line("ETHUSDT", "3000")],
            },
            AccountLines {
                account: uuid::Uuid::from_u128(2),
                lines: vec![line("BTCUSDT", "57000")],
            },
        ];
        let one = TradeOverlay {
            accounts: vec![acc(1, AccountEnv::Testnet, true)],
            lines: lines.clone(),
            ..Default::default()
        };
        let got = one.lines_for(m, "BTCUSDT");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].price.to_plain_string(), "58000");
        assert!(one.lines_for(m, "SOLUSDT").is_empty());
        let many = TradeOverlay {
            accounts: vec![
                acc(1, AccountEnv::Testnet, true),
                acc(2, AccountEnv::Testnet, true),
            ],
            lines,
            ..Default::default()
        };
        assert!(many.lines_for(m, "BTCUSDT").is_empty());
        let picked = TradeOverlay {
            ticket_account: Some(uuid::Uuid::from_u128(2)),
            ..many
        };
        assert_eq!(
            picked.lines_for(m, "BTCUSDT")[0].price.to_plain_string(),
            "57000"
        );
        // An account that was not tested is not drawn either.
        let untested = TradeOverlay {
            accounts: vec![acc(1, AccountEnv::Testnet, false)],
            lines: picked.lines.clone(),
            ..Default::default()
        };
        assert!(untested.lines_for(m, "BTCUSDT").is_empty());
    }
}
