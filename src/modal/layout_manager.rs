use crate::connector::client::DataSources;
use crate::layout::{Layout, LayoutId, SharedChartResources};
use crate::screen::dashboard::{self, Dashboard};
use crate::style::icon_text;
use crate::widget::column_drag::DragEvent;
use crate::widget::{manager_icon_button, manager_list, manager_row};
use crate::{style, tooltip};
use data::chart::kline::indicators::manifest;
use data::layout::presets::{self, Library as PresetLibrary};
use data::layout::templates;
use data::layout::{DetachedWindow, MAX_NAME_LENGTH};

use iced::widget::{
    button, center, column, container, row, scrollable, space, text, text_input,
    tooltip::Position as TooltipPosition,
};
use iced::{Element, padding, window};
use std::path::PathBuf;
use std::vec;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
pub enum Editing {
    ConfirmingDelete(Uuid),
    Renaming(Uuid, String),
    Preview,
    None,
}

#[derive(Debug, Clone)]
pub enum Message {
    SelectActive(Uuid),
    SetLayoutName(Uuid, String),
    Renaming(String),
    AddLayout,
    RemoveLayout(Uuid),
    ToggleEditMode(Editing),
    CloneLayout(Uuid),
    Reorder(DragEvent),
    /// Selects the layout at this position in the tab bar.
    SelectIndex(usize),
    /// Selects the next (or previous) layout, wrapping around.
    Cycle {
        forward: bool,
    },
    /// First request arms the delete confirmation; a second one deletes the layout.
    RequestClose(Uuid),
    RequestCloseActive,
    CancelClose,
    /// Save the active layout as a template file.
    SaveTemplate,
    /// Open a template as a new layout, optionally with its main ticker replaced by the
    /// active layout's main ticker.
    ApplyTemplate {
        path: PathBuf,
        current_ticker: bool,
    },
    /// First request arms the delete confirmation; a second one deletes the template file.
    DeleteTemplate(PathBuf),
    OpenTemplatesFolder,
    /// Move a tab into its own window.
    DetachTab(Uuid),
    /// The grip of a tab was pressed; releasing it drops the tab as a new window.
    TabGrabbed(Uuid),
}

pub enum Action {
    Select(Uuid),
    Clone(Uuid),
    /// Switch to `select`, then delete the previously active layout `remove`.
    Close {
        select: Uuid,
        remove: Uuid,
    },
    /// Add a layout built from a template and switch to it.
    Apply {
        name: String,
        dashboard: Box<data::Dashboard>,
    },
    /// Open the layout in its own window; the caller reports the window with `detach`.
    Detach(Uuid),
    /// Start tracking a tab drag until the mouse button is released.
    TabGrabbed(Uuid),
    /// A detach request that cannot be honoured, e.g. the last remaining tab.
    DetachRefused,
    /// Close the window of a layout that was removed while detached.
    CloseWindow(window::Id),
}

/// A layout shown in its own window instead of the tab bar.
struct Detached {
    layout: Uuid,
    window: window::Id,
    /// The window as the dashboard sees its main window.
    handle: crate::window::Window,
    /// Last known placement, kept current by window events and written to `saved-state.json`.
    saved: Option<DetachedWindow>,
}

pub struct LayoutManager {
    pub layouts: Vec<Layout>,
    chart_resources: SharedChartResources,
    active_layout_id: Option<Uuid>,
    pub edit_mode: Editing,
    /// Layout whose tab-bar delete button is awaiting confirmation.
    closing: Option<Uuid>,
    template_directory: PathBuf,
    templates: Vec<templates::Entry>,
    /// Template whose delete button is awaiting confirmation.
    deleting_template: Option<PathBuf>,
    template_status: Option<Result<String, String>>,
    detached: Vec<Detached>,
    /// Detached windows read from the saved state, waiting for the app to open them.
    restorable: Vec<(Uuid, DetachedWindow)>,
}

impl LayoutManager {
    fn default_layout() -> (LayoutId, Layout) {
        let id = LayoutId {
            unique: Uuid::new_v4(),
            name: "Layout 1".into(),
        };
        let layout = Layout {
            id: id.clone(),
            dashboard: Dashboard::default(),
        };

        (id, layout)
    }

    pub fn new() -> Self {
        let (default_layout, layout) = Self::default_layout();

        Self {
            layouts: vec![layout],
            chart_resources: SharedChartResources::default(),
            active_layout_id: Some(default_layout.unique),
            edit_mode: Editing::None,
            closing: None,
            template_directory: templates::default_directory(),
            templates: templates::load_all(&templates::default_directory()),
            deleting_template: None,
            template_status: None,
            detached: vec![],
            restorable: vec![],
        }
    }

    pub fn from_config(
        layouts: Vec<Layout>,
        active_layout: Option<LayoutId>,
        chart_presets: Vec<data::chart::kline::ChartPreset>,
        drawings: data::layout::drawings::Config,
        detached: Vec<(Uuid, DetachedWindow)>,
    ) -> Self {
        let mut manager = Self {
            layouts,
            chart_resources: SharedChartResources::from_config(chart_presets, drawings),
            active_layout_id: active_layout.map(|l| l.unique),
            edit_mode: Editing::None,
            closing: None,
            template_directory: templates::default_directory(),
            templates: templates::load_all(&templates::default_directory()),
            deleting_template: None,
            template_status: None,
            detached: vec![],
            restorable: detached,
        };
        manager.ensure_layout();
        manager
    }

    /// Detached windows from the saved state that the app should open now.
    ///
    /// Drops entries for unknown layouts and the active layout, and always keeps at least one
    /// layout in the tab bar. Does nothing where extra windows are unsupported.
    pub fn take_restorable(&mut self) -> Vec<(Uuid, DetachedWindow)> {
        let requested = std::mem::take(&mut self.restorable);
        if !crate::window::MULTI_WINDOW_SUPPORTED {
            return vec![];
        }

        let mut accepted: Vec<(Uuid, DetachedWindow)> = vec![];
        for (id, window) in requested {
            let known = self.get(id).is_some();
            let duplicate = accepted.iter().any(|(accepted_id, _)| *accepted_id == id);
            let is_active = Some(id) == self.active_layout_id;
            // The tab bar keeps at least one layout, and the active one stays in it.
            let leaves_a_tab = self.layouts.len() > accepted.len() + 1;
            if known && !duplicate && !is_active && leaves_a_tab {
                accepted.push((id, window));
            }
        }

        let active_is_valid = self
            .active_layout_id
            .is_some_and(|id| self.get(id).is_some());
        if !active_is_valid {
            self.active_layout_id = self
                .layouts
                .iter()
                .map(|layout| layout.id.unique)
                .find(|id| !accepted.iter().any(|(accepted_id, _)| accepted_id == id));
        }

        accepted
    }

    pub fn is_detached(&self, layout: Uuid) -> bool {
        self.detached.iter().any(|d| d.layout == layout)
    }

    pub fn detached_window(&self, layout: Uuid) -> Option<window::Id> {
        self.detached
            .iter()
            .find(|d| d.layout == layout)
            .map(|d| d.window)
    }

    pub fn window_handle(&self, window: window::Id) -> Option<&crate::window::Window> {
        self.detached
            .iter()
            .find(|d| d.window == window)
            .map(|d| &d.handle)
    }

    pub fn layout_for_window(&self, window: window::Id) -> Option<Uuid> {
        self.detached
            .iter()
            .find(|d| d.window == window)
            .map(|d| d.layout)
    }

    pub fn detached_windows(&self) -> impl Iterator<Item = window::Id> + '_ {
        self.detached.iter().map(|d| d.window)
    }

    /// Layouts shown in the tab bar, in order.
    pub fn tab_layouts(&self) -> impl Iterator<Item = &Layout> {
        self.layouts
            .iter()
            .filter(|layout| !self.is_detached(layout.id.unique))
    }

    fn tab_ids(&self) -> Vec<Uuid> {
        self.tab_layouts().map(|layout| layout.id.unique).collect()
    }

    /// The active layout plus every detached one, in tab order.
    pub fn visible_ids(&self) -> Vec<Uuid> {
        self.layouts
            .iter()
            .map(|layout| layout.id.unique)
            .filter(|id| Some(*id) == self.active_layout_id || self.is_detached(*id))
            .collect()
    }

    pub fn can_detach(&self, id: Uuid) -> bool {
        self.get(id).is_some() && !self.is_detached(id) && self.tab_ids().len() > 1
    }

    /// Hides the layout from the tab bar and tracks `window` as its window.
    ///
    /// Returns `None` if the layout cannot be detached. Otherwise the inner value is the tab to
    /// switch to when the detached layout was the active one.
    pub fn detach(&mut self, id: Uuid, window: window::Id) -> Option<Option<Uuid>> {
        if !self.can_detach(id) {
            return None;
        }

        let next_active = (Some(id) == self.active_layout_id).then(|| {
            let tabs = self.tab_ids();
            let index = tabs.iter().position(|tab| *tab == id).unwrap_or_default();
            let neighbor = if index > 0 { index - 1 } else { index + 1 };
            tabs[neighbor]
        });

        self.detached.push(Detached {
            layout: id,
            window,
            handle: crate::window::Window::new(window),
            saved: None,
        });
        Some(next_active)
    }

    /// Registers a window opened for a layout restored from the saved state.
    pub fn restore_detached(&mut self, id: Uuid, window: window::Id, saved: DetachedWindow) {
        if self.get(id).is_some() && !self.is_detached(id) {
            self.detached.push(Detached {
                layout: id,
                window,
                handle: crate::window::Window::new(window),
                saved: Some(saved),
            });
        }
    }

    /// Returns the layout of a closed detached window to the tab bar, at its old position.
    pub fn attach(&mut self, window: window::Id) -> Option<Uuid> {
        let index = self.detached.iter().position(|d| d.window == window)?;
        Some(self.detached.remove(index).layout)
    }

    pub fn saved_window(&self, window: window::Id) -> Option<&DetachedWindow> {
        self.detached
            .iter()
            .find(|d| d.window == window)
            .and_then(|d| d.saved.as_ref())
    }

    pub fn saved_placement(&self, layout: Uuid) -> Option<DetachedWindow> {
        self.detached.iter().find(|d| d.layout == layout).map(|d| {
            d.saved.unwrap_or_else(|| DetachedWindow {
                spec: data::layout::WindowSpec::default(),
                monitor: None,
            })
        })
    }

    /// Records the position and/or size last reported for a detached window.
    pub fn record_window(
        &mut self,
        window: window::Id,
        position: Option<iced::Point>,
        size: Option<iced::Size>,
    ) {
        let Some(detached) = self.detached.iter_mut().find(|d| d.window == window) else {
            return;
        };
        let saved = detached.saved.get_or_insert(DetachedWindow {
            spec: data::layout::WindowSpec::default(),
            monitor: None,
        });
        if let Some(position) = position {
            saved.spec.pos_x = position.x;
            saved.spec.pos_y = position.y;
        }
        if let Some(size) = size {
            saved.spec.width = size.width;
            saved.spec.height = size.height;
        }
    }

    pub fn record_monitor(&mut self, window: window::Id, monitor: Option<iced::Size>) {
        if let Some(detached) = self.detached.iter_mut().find(|d| d.window == window)
            && let Some(saved) = detached.saved.as_mut()
        {
            saved.monitor = monitor.map(|size| (size.width, size.height));
        }
    }

    /// Re-read the templates directory, picking up files copied in from elsewhere.
    pub fn refresh_templates(&mut self) {
        self.templates = templates::load_all(&self.template_directory);
    }

    fn templates_view(&self) -> Element<'_, Message> {
        let header = row![
            text("Templates").size(crate::style::text_size::SECTION),
            space::horizontal(),
            tooltip(
                manager_icon_button(style::Icon::Folder, Some(Message::OpenTemplatesFolder)),
                Some("Open templates folder (copy .json files here to import)"),
                TooltipPosition::Top,
            ),
        ]
        .align_y(iced::Alignment::Center);

        let mut list = column![header].spacing(4);
        for entry in &self.templates {
            let primary = entry
                .template
                .primary_ticker()
                .map(|ticker| ticker.to_string());
            let open = button(
                column![
                    text(entry.template.name.clone()).size(12),
                    text(primary.clone().unwrap_or_else(|| "No ticker".to_owned())).size(10),
                ]
                .spacing(2),
            )
            .width(iced::Length::Fill)
            .style(|theme, status| style::button::transparent(theme, status, false))
            .on_press(Message::ApplyTemplate {
                path: entry.path.clone(),
                current_ticker: false,
            });
            let open = tooltip(
                open,
                Some("Open as a new layout with its saved tickers"),
                TooltipPosition::Top,
            );
            let current = tooltip(
                manager_icon_button(
                    style::Icon::Link,
                    primary.is_some().then(|| Message::ApplyTemplate {
                        path: entry.path.clone(),
                        current_ticker: true,
                    }),
                ),
                Some("Open as a new layout using the active layout's main ticker"),
                TooltipPosition::Top,
            );
            let armed = self.deleting_template.as_ref() == Some(&entry.path);
            let delete = manager_icon_button(
                style::Icon::TrashBin,
                Some(Message::DeleteTemplate(entry.path.clone())),
            )
            .style(move |theme, status| {
                if armed {
                    style::button::cancel(theme, status, true)
                } else {
                    style::button::transparent(theme, status, false)
                }
            });
            let delete = tooltip(
                delete,
                Some(if armed {
                    "Click again to delete"
                } else {
                    "Delete template"
                }),
                TooltipPosition::Top,
            );
            list = list.push(
                row![open, current, delete]
                    .spacing(2)
                    .align_y(iced::Alignment::Center),
            );
        }
        if self.templates.is_empty() {
            list = list.push(text("No templates yet").size(11));
        }
        list = list.push(
            button(text("Save active layout as template"))
                .style(move |t, s| style::button::transparent(t, s, true))
                .width(iced::Length::Fill)
                .on_press(Message::SaveTemplate),
        );
        if let Some(status) = &self.template_status {
            list = list.push(match status {
                Ok(message) => text(message.clone()).size(11).style(text::success),
                Err(message) => text(message.clone()).size(11).style(text::danger),
            });
        }
        list.into()
    }

    pub fn chart_resources(&self) -> &SharedChartResources {
        &self.chart_resources
    }

    pub fn presets(&self) -> &PresetLibrary {
        self.chart_resources.presets()
    }

    pub fn ensure_layout(&mut self) {
        if !self.layouts.is_empty() {
            return;
        }

        let (default_layout, layout) = Self::default_layout();
        self.layouts.push(layout);
        self.active_layout_id = Some(default_layout.unique);
        self.edit_mode = Editing::None;
    }

    pub fn get(&self, unique: Uuid) -> Option<&Layout> {
        self.layouts
            .iter()
            .find(|layout| layout.id.unique == unique)
    }

    pub fn get_mut(&mut self, unique: Uuid) -> Option<&mut Layout> {
        self.layouts
            .iter_mut()
            .find(|layout| layout.id.unique == unique)
    }

    pub fn active_layout_id(&self) -> Option<&LayoutId> {
        self.get(self.active_layout_id?).map(|layout| &layout.id)
    }

    pub fn insert_layout(&mut self, id: LayoutId, dashboard: Dashboard) {
        self.layouts.push(Layout { id, dashboard });
    }

    pub fn generate_unique_layout_name(&self) -> String {
        let mut counter = 1;
        loop {
            let candidate = format!("Layout {counter}");
            if !self
                .layouts
                .iter()
                .any(|layout| layout.id.name == candidate)
            {
                return candidate;
            }
            counter += 1;
        }
    }

    pub fn ensure_unique_name(&self, proposed: &str, current_id: Uuid) -> String {
        data::util::unique_name(
            proposed,
            self.layouts
                .iter()
                .filter(|layout| layout.id.unique != current_id)
                .map(|layout| layout.id.name.as_str()),
            MAX_NAME_LENGTH,
        )
    }

    pub fn mut_dashboard(&mut self, id: Uuid) -> Option<&mut Dashboard> {
        self.get_mut(id).map(|e| &mut e.dashboard)
    }

    pub fn update_dashboard(
        &mut self,
        message: dashboard::Message,
        main_window: &crate::window::Window,
        layout_id: &Uuid,
        data_sources: &DataSources,
    ) -> Option<(iced::Task<dashboard::Message>, Option<dashboard::Event>)> {
        let layout_index = self
            .layouts
            .iter()
            .position(|layout| layout.id.unique == *layout_id)?;
        let dashboard = &mut self.layouts[layout_index].dashboard;

        Some(dashboard.update(message, main_window, data_sources, &self.chart_resources))
    }

    pub fn update_chart_resources(
        &mut self,
        message: manifest::Message,
        catalog_http: &reqwest::Client,
    ) -> iced::Task<manifest::Message> {
        self.chart_resources.update(message, catalog_http)
    }

    pub fn apply_preset_command(
        &mut self,
        main_window: window::Id,
        layout_id: Uuid,
        source_window: window::Id,
        source_pane: iced::widget::pane_grid::Pane,
        command: presets::Command,
    ) {
        let response = self.chart_resources.update_presets(command);

        for layout in &mut self.layouts {
            layout
                .dashboard
                .reconcile_selected_presets(main_window, self.chart_resources.presets());
        }

        if let Some(layout) = self
            .layouts
            .iter_mut()
            .find(|layout| layout.id.unique == layout_id)
        {
            layout.dashboard.preset_command_completed(
                main_window,
                source_window,
                source_pane,
                response,
            );
        }
    }

    /// Exchanges kline drawings between the active layout's charts and the shared library.
    pub fn sync_drawings(&mut self, main_window: window::Id) {
        let visible = self.visible_ids();
        let library = self.chart_resources.drawings_mut();
        for layout in self
            .layouts
            .iter_mut()
            .filter(|layout| visible.contains(&layout.id.unique))
        {
            let window = self
                .detached
                .iter()
                .find(|d| d.layout == layout.id.unique)
                .map_or(main_window, |d| d.window);
            layout.dashboard.sync_drawings(window, library);
        }
    }

    /// Parks every layout that is neither the active one nor open in its own window.
    pub fn park_inactive_layouts(&mut self, active_layout_id: Uuid, main_window: window::Id) {
        let detached = &self.detached;
        self.layouts
            .iter_mut()
            .filter(|layout| {
                layout.id.unique != active_layout_id
                    && !detached.iter().any(|d| d.layout == layout.id.unique)
            })
            .for_each(|layout| layout.dashboard.park_for_inactive_layout(main_window));
    }

    pub fn set_active_layout(&mut self, layout_id: Uuid) -> Result<&mut Layout, String> {
        self.active_layout_id = Some(layout_id);

        self.get_mut(layout_id)
            .ok_or_else(|| "Layout not found".into())
    }

    pub fn closing(&self) -> Option<Uuid> {
        self.closing
    }

    fn close(&mut self, id: Uuid) -> Option<Action> {
        self.closing = None;
        let tabs = self.tab_ids();
        if tabs.len() <= 1 || !tabs.contains(&id) {
            return None;
        }
        if Some(id) != self.active_layout_id {
            self.layouts.retain(|layout| layout.id.unique != id);
            return None;
        }
        let index = tabs.iter().position(|tab| *tab == id)?;
        let neighbor = if index > 0 { index - 1 } else { index + 1 };
        let select = *tabs.get(neighbor)?;
        Some(Action::Close { select, remove: id })
    }

    pub fn update(&mut self, message: Message) -> Option<Action> {
        if !matches!(
            message,
            Message::RequestClose(_) | Message::RequestCloseActive
        ) {
            self.closing = None;
        }

        if !matches!(message, Message::DeleteTemplate(_)) {
            self.deleting_template = None;
        }
        match message {
            Message::SaveTemplate => {
                let layout = self.active_layout_id.and_then(|id| self.get(id))?;
                let template = templates::Template::new(
                    layout.id.name.clone(),
                    data::Dashboard::from(&layout.dashboard),
                );
                self.template_status = Some(
                    templates::save(&self.template_directory, &template)
                        .map(|path| format!("Saved {}", path.display())),
                );
                self.templates = templates::load_all(&self.template_directory);
            }
            Message::ApplyTemplate {
                path,
                current_ticker,
            } => {
                let entry = self.templates.iter().find(|entry| entry.path == path)?;
                let ticker = if current_ticker {
                    let layout = self.active_layout_id.and_then(|id| self.get(id))?;
                    templates::primary_ticker(&data::Dashboard::from(&layout.dashboard))
                } else {
                    None
                };
                return Some(Action::Apply {
                    name: entry.template.name.clone(),
                    dashboard: Box::new(entry.template.instantiate(ticker)),
                });
            }
            Message::DeleteTemplate(path) => {
                if self.deleting_template.as_ref() != Some(&path) {
                    self.deleting_template = Some(path);
                    return None;
                }
                self.deleting_template = None;
                self.template_status = Some(
                    templates::delete(&self.template_directory, &path)
                        .map(|()| "Template deleted".to_owned()),
                );
                self.templates = templates::load_all(&self.template_directory);
            }
            Message::OpenTemplatesFolder => {
                self.template_status = templates::open_directory(&self.template_directory)
                    .err()
                    .map(Err);
                self.refresh_templates();
            }
            Message::SelectIndex(index) => {
                let id = *self.tab_ids().get(index)?;
                if Some(id) != self.active_layout_id {
                    return self.update(Message::SelectActive(id));
                }
            }
            Message::Cycle { forward } => {
                let tabs = self.tab_ids();
                let len = tabs.len();
                let index = tabs
                    .iter()
                    .position(|id| Some(*id) == self.active_layout_id)?;
                let next = if forward {
                    (index + 1) % len
                } else {
                    (index + len - 1) % len
                };
                if next != index {
                    return self.update(Message::SelectIndex(next));
                }
            }
            Message::RequestClose(id) => {
                if self.closing == Some(id) {
                    return self.close(id);
                }
                if self.tab_ids().len() > 1 {
                    self.closing = Some(id);
                }
            }
            Message::RequestCloseActive => {
                let id = self.active_layout_id?;
                return self.update(Message::RequestClose(id));
            }
            Message::CancelClose => {}
            Message::SelectActive(id) => {
                if self.is_detached(id) {
                    return None;
                }
                self.active_layout_id = Some(id);
                return Some(Action::Select(id));
            }
            Message::ToggleEditMode(new_mode) => match (&new_mode, &self.edit_mode) {
                (Editing::Preview, Editing::Preview) => {
                    self.edit_mode = Editing::None;
                }
                (Editing::Renaming(id, _), Editing::Renaming(renaming_id, _))
                    if id == renaming_id =>
                {
                    self.edit_mode = Editing::None;
                }
                _ => {
                    self.edit_mode = new_mode;
                }
            },
            Message::AddLayout => {
                let new_layout = LayoutId {
                    unique: Uuid::new_v4(),
                    name: self.generate_unique_layout_name(),
                };

                self.insert_layout(new_layout.clone(), Dashboard::default());

                return Some(Action::Select(new_layout.unique));
            }
            Message::RemoveLayout(id) => {
                if Some(id) == self.active_layout_id {
                    return None;
                }
                let window = self.detached_window(id);
                self.detached.retain(|d| d.layout != id);
                self.layouts.retain(|layout| layout.id.unique != id);
                self.edit_mode = Editing::Preview;
                return window.map(Action::CloseWindow);
            }
            Message::TabGrabbed(id) => {
                return Some(Action::TabGrabbed(id));
            }
            Message::DetachTab(id) => {
                if !crate::window::MULTI_WINDOW_SUPPORTED || !self.can_detach(id) {
                    return Some(Action::DetachRefused);
                }
                return Some(Action::Detach(id));
            }
            Message::SetLayoutName(id, new_name) => {
                let unique_name = self.ensure_unique_name(&new_name, id);

                if let Some(layout) = self.get_mut(id) {
                    layout.id.name = unique_name;
                }

                self.edit_mode = Editing::Preview;
            }
            Message::Renaming(name) => {
                self.edit_mode = match self.edit_mode {
                    Editing::Renaming(id, _) => {
                        let truncated = name.chars().take(MAX_NAME_LENGTH).collect();
                        Editing::Renaming(id, truncated)
                    }
                    _ => Editing::None,
                };
            }
            Message::CloneLayout(id) => {
                return Some(Action::Clone(id));
            }
            Message::Reorder(DragEvent::Dropped {
                index,
                target_index,
            }) => {
                let _ = data::util::reorder_vec(&mut self.layouts, index, target_index);
            }
            Message::Reorder(_) => {}
        }

        None
    }

    pub fn view(&self) -> Element<'_, Message> {
        let mut content = column![].spacing(8);

        let is_edit_mode = self.edit_mode != Editing::None;

        let edit_btn = if is_edit_mode {
            button(icon_text(style::Icon::Return, 12))
                .on_press(Message::ToggleEditMode(Editing::Preview))
        } else {
            button(text("Edit")).on_press(Message::ToggleEditMode(Editing::Preview))
        };

        content = content.push(row![
            space::horizontal(),
            if is_edit_mode {
                row![edit_btn]
            } else {
                row![
                    tooltip(
                        button("i").style(style::button::info),
                        Some("Layouts won't be saved if app exits abruptly"),
                        TooltipPosition::Top,
                    ),
                    edit_btn,
                ]
                .spacing(4)
            }
        ]);

        let mut layout_widgets: Vec<Element<'_, Message>> = vec![];

        for layout in &self.layouts {
            let layout_id = &layout.id;

            let mut layout_row = row![];

            let is_active = self.active_layout_id == Some(layout_id.unique);
            match &self.edit_mode {
                Editing::ConfirmingDelete(delete_id) => {
                    if *delete_id == layout_id.unique {
                        let (confirm_btn, cancel_btn) = create_confirm_delete_buttons(layout_id);

                        layout_row = layout_row
                            .push(center(
                                text(format!("Delete {}?", layout.id.name))
                                    .size(crate::style::text_size::BODY),
                            ))
                            .push(confirm_btn)
                            .push(cancel_btn);
                    } else {
                        layout_row = layout_row.push(create_layout_button(layout_id, None));
                    }
                }
                Editing::Renaming(renaming_id, name) => {
                    if *renaming_id == layout_id.unique {
                        let input_box = text_input("New layout name", name)
                            .on_input(|new_name| Message::Renaming(new_name.clone()))
                            .on_submit(Message::SetLayoutName(*renaming_id, name.clone()));

                        let (_, cancel_btn) = create_confirm_delete_buttons(layout_id);

                        layout_row = layout_row
                            .push(center(input_box).padding(padding::left(4)))
                            .push(cancel_btn);
                    } else {
                        layout_row = layout_row.push(create_layout_button(layout_id, None));
                    }
                }
                Editing::Preview => {
                    layout_row = layout_row
                        .push(create_layout_button(layout_id, None))
                        .push(create_clone_button(layout_id))
                        .push(create_rename_button(layout_id));

                    if !is_active {
                        layout_row = layout_row.push(create_delete_button(layout_id));
                    }
                }
                Editing::None => {
                    layout_row = layout_row.push(create_layout_button(
                        layout_id,
                        if is_active || self.is_detached(layout_id.unique) {
                            None
                        } else {
                            Some(Message::SelectActive(layout_id.unique))
                        },
                    ));
                }
            }

            if is_active && !is_edit_mode {
                layout_row = layout_row.push(
                    container(icon_text(style::Icon::Checkmark, 12)).padding(padding::right(16)),
                );
            }

            layout_widgets.push(manager_row(layout_row, is_active, is_edit_mode));
        }

        content = content.push(manager_list(
            layout_widgets,
            is_edit_mode.then_some(Message::Reorder),
        ));

        if self.edit_mode != Editing::None {
            content = content.push(
                button(text("Add layout"))
                    .style(move |t, s| style::button::transparent(t, s, true))
                    .width(iced::Length::Fill)
                    .on_press(Message::AddLayout),
            );
        };

        content = content.push(self.templates_view());

        scrollable::Scrollable::with_direction(
            content,
            scrollable::Direction::Vertical(
                scrollable::Scrollbar::new().width(8).scroller_width(6),
            ),
        )
        .into()
    }
}

fn create_delete_button<'a>(layout: &LayoutId) -> Element<'a, Message> {
    manager_icon_button(
        style::Icon::TrashBin,
        Some(Message::ToggleEditMode(Editing::ConfirmingDelete(
            layout.unique,
        ))),
    )
    .into()
}

fn create_rename_button<'a>(layout: &LayoutId) -> button::Button<'a, Message> {
    manager_icon_button(
        style::Icon::Edit,
        Some(Message::ToggleEditMode(Editing::Renaming(
            layout.unique,
            layout.name.clone(),
        ))),
    )
}

fn create_clone_button<'a>(layout: &LayoutId) -> Element<'a, Message> {
    tooltip(
        manager_icon_button(
            style::Icon::Clone,
            Some(Message::CloneLayout(layout.unique)),
        ),
        Some("Duplicate layout"),
        TooltipPosition::Top,
    )
}

fn create_confirm_delete_buttons<'a>(
    layout: &LayoutId,
) -> (button::Button<'a, Message>, button::Button<'a, Message>) {
    let confirm = manager_icon_button(
        style::Icon::Checkmark,
        Some(Message::RemoveLayout(layout.unique)),
    )
    .style(|theme, status| style::button::confirm(theme, status, true));

    let cancel = manager_icon_button(
        style::Icon::Close,
        Some(Message::ToggleEditMode(Editing::Preview)),
    )
    .style(|theme, status| style::button::cancel(theme, status, true));

    (confirm, cancel)
}

fn create_layout_button<'a>(
    layout: &'a LayoutId,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    button(
        text(&layout.name)
            .align_y(iced::Alignment::Center)
            .wrapping(iced::widget::text::Wrapping::None),
    )
    .width(iced::Length::Fill)
    .clip(true)
    .style(style::button::manager_entry)
    .on_press_maybe(on_press)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager_with(count: usize) -> LayoutManager {
        let mut manager = LayoutManager::new();
        for _ in 1..count {
            let _ = manager.update(Message::AddLayout);
        }
        manager
    }

    fn id_at(manager: &LayoutManager, index: usize) -> Uuid {
        manager.layouts[index].id.unique
    }

    fn active(manager: &LayoutManager) -> Uuid {
        manager.active_layout_id().unwrap().unique
    }

    #[test]
    fn select_index_switches_to_that_tab() {
        let mut manager = manager_with(3);
        let target = id_at(&manager, 2);
        assert!(matches!(
            manager.update(Message::SelectIndex(2)),
            Some(Action::Select(id)) if id == target
        ));
        assert!(manager.update(Message::SelectIndex(2)).is_none());
        assert!(manager.update(Message::SelectIndex(7)).is_none());
    }

    #[test]
    fn cycle_wraps_around() {
        let mut manager = manager_with(3);
        let first = id_at(&manager, 0);
        let last = id_at(&manager, 2);
        assert_eq!(active(&manager), first);
        assert!(matches!(
            manager.update(Message::Cycle { forward: false }),
            Some(Action::Select(id)) if id == last
        ));
        assert!(matches!(
            manager.update(Message::Cycle { forward: true }),
            Some(Action::Select(id)) if id == first
        ));
    }

    #[test]
    fn closing_needs_a_second_request() {
        let mut manager = manager_with(2);
        let other = id_at(&manager, 1);

        assert!(manager.update(Message::RequestClose(other)).is_none());
        assert_eq!(manager.closing(), Some(other));
        assert_eq!(manager.layouts.len(), 2);

        assert!(manager.update(Message::RequestClose(other)).is_none());
        assert_eq!(manager.layouts.len(), 1);
        assert_eq!(manager.closing(), None);
    }

    #[test]
    fn other_messages_cancel_a_pending_close() {
        let mut manager = manager_with(2);
        let other = id_at(&manager, 1);
        let _ = manager.update(Message::RequestClose(other));
        let _ = manager.update(Message::CancelClose);
        assert_eq!(manager.closing(), None);
        let _ = manager.update(Message::RequestClose(other));
        assert_eq!(manager.layouts.len(), 2);
    }

    #[test]
    fn closing_the_active_tab_selects_a_neighbor() {
        let mut manager = manager_with(2);
        let first = id_at(&manager, 0);
        let second = id_at(&manager, 1);

        let _ = manager.update(Message::RequestCloseActive);
        assert!(matches!(
            manager.update(Message::RequestCloseActive),
            Some(Action::Close { select, remove }) if select == second && remove == first
        ));
    }

    fn window() -> window::Id {
        window::Id::unique()
    }

    fn saved_window(x: f32) -> DetachedWindow {
        DetachedWindow {
            spec: data::layout::WindowSpec {
                width: 640.0,
                height: 480.0,
                pos_x: x,
                pos_y: 10.0,
            },
            monitor: Some((1920.0, 1080.0)),
        }
    }

    #[test]
    fn the_last_tab_cannot_be_detached() {
        let mut manager = manager_with(1);
        let only = active(&manager);
        assert!(!manager.can_detach(only));
        assert!(manager.detach(only, window()).is_none());
        assert!(manager.detached_windows().next().is_none());

        let mut manager = manager_with(2);
        let first = id_at(&manager, 0);
        let second = id_at(&manager, 1);
        assert!(manager.detach(second, window()).is_some());
        assert!(manager.detach(first, window()).is_none());
        assert!(manager.detach(second, window()).is_none());
    }

    #[test]
    fn detaching_the_active_tab_selects_a_neighbor() {
        let mut manager = manager_with(3);
        let first = id_at(&manager, 0);
        let second = id_at(&manager, 1);
        let third = id_at(&manager, 2);

        assert_eq!(manager.detach(first, window()), Some(Some(second)));
        assert!(manager.is_detached(first));
        assert_eq!(
            manager
                .tab_layouts()
                .map(|l| l.id.unique)
                .collect::<Vec<_>>(),
            vec![second, third]
        );

        // A tab that is not active leaves the selection alone.
        assert_eq!(manager.detach(third, window()), Some(None));
    }

    #[test]
    fn detached_tabs_are_skipped_by_index_and_cycle() {
        let mut manager = manager_with(3);
        let first = id_at(&manager, 0);
        let second = id_at(&manager, 1);
        let third = id_at(&manager, 2);
        manager.detach(second, window());

        assert!(matches!(
            manager.update(Message::SelectIndex(1)),
            Some(Action::Select(id)) if id == third
        ));
        assert!(manager.update(Message::SelectIndex(2)).is_none());
        assert!(matches!(
            manager.update(Message::Cycle { forward: true }),
            Some(Action::Select(id)) if id == first
        ));
        assert!(manager.update(Message::SelectActive(second)).is_none());
        assert_eq!(active(&manager), first);
    }

    #[test]
    fn closing_picks_a_tab_neighbor_not_a_detached_one() {
        let mut manager = manager_with(3);
        let first = id_at(&manager, 0);
        let second = id_at(&manager, 1);
        let third = id_at(&manager, 2);
        manager.detach(second, window());

        let _ = manager.update(Message::RequestCloseActive);
        assert!(matches!(
            manager.update(Message::RequestCloseActive),
            Some(Action::Close { select, remove }) if select == third && remove == first
        ));
    }

    #[test]
    fn attaching_returns_the_layout_to_its_old_position() {
        let mut manager = manager_with(3);
        let first = id_at(&manager, 0);
        let second = id_at(&manager, 1);
        let handle = window();
        manager.detach(second, handle);

        assert_eq!(manager.layout_for_window(handle), Some(second));
        assert_eq!(manager.attach(handle), Some(second));
        assert!(!manager.is_detached(second));
        assert_eq!(id_at(&manager, 1), second);
        assert_eq!(
            manager.tab_layouts().map(|l| l.id.unique).nth(1),
            Some(second)
        );
        assert_eq!(active(&manager), first);
        assert!(manager.attach(handle).is_none());
    }

    #[test]
    fn visible_layouts_are_the_active_one_and_the_detached_ones() {
        let mut manager = manager_with(3);
        let first = id_at(&manager, 0);
        let third = id_at(&manager, 2);
        manager.detach(third, window());
        assert_eq!(manager.visible_ids(), vec![first, third]);
    }

    #[test]
    fn removing_a_detached_layout_closes_its_window() {
        let mut manager = manager_with(2);
        let second = id_at(&manager, 1);
        let handle = window();
        manager.detach(second, handle);

        assert!(matches!(
            manager.update(Message::RemoveLayout(second)),
            Some(Action::CloseWindow(closed)) if closed == handle
        ));
        assert_eq!(manager.layouts.len(), 1);
        assert!(!manager.is_detached(second));
    }

    #[test]
    fn restoring_keeps_a_tab_and_keeps_the_active_layout_attached() {
        let mut manager = manager_with(3);
        let first = id_at(&manager, 0);
        let second = id_at(&manager, 1);
        let third = id_at(&manager, 2);
        manager.restorable = vec![
            (first, saved_window(1.0)),
            (second, saved_window(2.0)),
            (second, saved_window(3.0)),
            (third, saved_window(4.0)),
            (Uuid::new_v4(), saved_window(5.0)),
        ];

        let restored = manager.take_restorable();
        let restored_ids = restored.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        assert_eq!(restored_ids, vec![second, third]);
        assert!(!restored_ids.contains(&first));
        assert_eq!(active(&manager), first);
    }

    #[test]
    fn restoring_replaces_an_active_layout_that_was_detached() {
        let mut manager = manager_with(2);
        let first = id_at(&manager, 0);
        let second = id_at(&manager, 1);
        manager.active_layout_id = None;
        manager.restorable = vec![(first, saved_window(1.0))];

        let restored = manager.take_restorable();
        assert_eq!(restored.len(), 1);
        assert_eq!(active(&manager), second);
    }

    #[test]
    fn window_events_update_the_saved_placement() {
        let mut manager = manager_with(2);
        let second = id_at(&manager, 1);
        let handle = window();
        manager.detach(second, handle);

        manager.record_window(handle, Some(iced::Point::new(12.0, 34.0)), None);
        manager.record_window(handle, None, Some(iced::Size::new(500.0, 400.0)));
        manager.record_monitor(handle, Some(iced::Size::new(2560.0, 1440.0)));

        let saved = manager.saved_placement(second).unwrap();
        assert_eq!(saved.spec.pos_x, 12.0);
        assert_eq!(saved.spec.pos_y, 34.0);
        assert_eq!(saved.spec.width, 500.0);
        assert_eq!(saved.spec.height, 400.0);
        assert_eq!(saved.monitor, Some((2560.0, 1440.0)));
    }

    #[test]
    fn the_last_layout_cannot_be_closed() {
        let mut manager = manager_with(1);
        let only = active(&manager);
        assert!(manager.update(Message::RequestClose(only)).is_none());
        assert_eq!(manager.closing(), None);
        assert!(manager.update(Message::RequestClose(only)).is_none());
        assert_eq!(manager.layouts.len(), 1);
    }
}
