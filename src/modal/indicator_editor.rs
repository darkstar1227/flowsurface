use crate::style::{self, Icon, icon_text};
use data::chart::kline::IndicatorId;
use data::chart::kline::indicators::editor::{self, SavedSource, SourceDiagnostic, SourceEntry};
use iced::{
    Alignment, Element, Length, Task, keyboard,
    widget::{
        button, column, container, row, rule, scrollable, space, text,
        text_editor::{self, Binding, Content, Cursor, KeyPress, Position},
    },
};
use std::collections::BTreeSet;
use std::ops::Range;

#[derive(Debug, Clone)]
pub enum Message {
    New,
    Open(IndicatorId),
    Edit(text_editor::Action),
    Check,
    Save,
    Saved(Result<SavedSource, Vec<SourceDiagnostic>>),
    JumpTo { line: usize, column: usize },
}

pub enum Action {
    /// The catalog was reloaded with this indicator's new source; rebuild charts that use it.
    Reloaded(IndicatorId),
}

/// Which indicator the editor text came from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Origin {
    New,
    Installed(IndicatorId),
    /// Bundled sources open read-only; saving needs a new ID.
    Bundled(IndicatorId),
}

impl Origin {
    fn installed_id(&self) -> Option<&IndicatorId> {
        match self {
            Self::Installed(id) => Some(id),
            Self::New | Self::Bundled(_) => None,
        }
    }
}

enum Status {
    Info(String),
    Success(String),
    Error(String),
}

pub struct IndicatorEditor {
    content: Content,
    origin: Origin,
    entries: Vec<SourceEntry>,
    diagnostics: Vec<SourceDiagnostic>,
    status: Option<Status>,
    dirty: bool,
    saving: bool,
    /// A target the user asked to open while the text had unsaved changes; asking again opens it.
    discard_for: Option<Option<IndicatorId>>,
}

impl Default for IndicatorEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl IndicatorEditor {
    pub fn new() -> Self {
        let mut editor = Self {
            content: Content::new(),
            origin: Origin::New,
            entries: Vec::new(),
            diagnostics: Vec::new(),
            status: None,
            dirty: false,
            saving: false,
            discard_for: None,
        };
        editor.start_new();
        editor
    }

    /// Re-read the catalog listing; call when the editor is shown.
    pub fn refresh(&mut self) {
        self.entries = editor::sources();
    }

    pub fn update(&mut self, message: Message) -> (Task<Message>, Option<Action>) {
        if !matches!(message, Message::New | Message::Open(_)) {
            self.discard_for = None;
        }
        match message {
            Message::New => {
                if self.confirm_discard(None) {
                    self.start_new();
                }
            }
            Message::Open(id) => {
                if self.confirm_discard(Some(id.clone())) {
                    self.open(id);
                }
            }
            Message::Edit(action) => {
                if action.is_edit() {
                    self.dirty = true;
                }
                self.content.perform(action);
            }
            Message::Check => {
                let source = self.content.text();
                match editor::check(&source) {
                    Ok(id) => {
                        self.diagnostics.clear();
                        self.status = Some(Status::Success(format!("{} compiles", id.id())));
                    }
                    Err(diagnostics) => self.show_errors(diagnostics),
                }
            }
            Message::Save => {
                if self.saving {
                    return (Task::none(), None);
                }
                self.saving = true;
                self.status = Some(Status::Info("Saving…".to_owned()));
                let source = self.content.text();
                let opened = self.origin.installed_id().cloned();
                return (
                    Task::perform(
                        async move {
                            tokio::task::spawn_blocking(move || {
                                editor::save(&source, opened.as_ref())
                            })
                            .await
                            .unwrap_or_else(|error| {
                                Err(vec![SourceDiagnostic {
                                    line: None,
                                    column: None,
                                    message: format!("Save task failed: {error}"),
                                    is_warning: false,
                                }])
                            })
                        },
                        Message::Saved,
                    ),
                    None,
                );
            }
            Message::Saved(result) => {
                self.saving = false;
                self.refresh();
                match result {
                    Ok(saved) => {
                        let replaced_other = self
                            .origin
                            .installed_id()
                            .is_some_and(|opened| *opened != saved.id);
                        self.diagnostics.clear();
                        self.dirty = false;
                        self.status = Some(Status::Success(if replaced_other || saved.created {
                            format!(
                                "Created {} and reloaded indicators{}",
                                saved.path.display(),
                                if replaced_other {
                                    "; the original source is unchanged"
                                } else {
                                    ""
                                }
                            )
                        } else {
                            format!("Saved {} and reloaded indicators", saved.path.display())
                        }));
                        self.origin = Origin::Installed(saved.id.clone());
                        return (Task::none(), Some(Action::Reloaded(saved.id)));
                    }
                    Err(diagnostics) => self.show_errors(diagnostics),
                }
            }
            Message::JumpTo { line, column } => {
                self.content.move_to(Cursor {
                    position: Position {
                        line: line.saturating_sub(1),
                        column: column.saturating_sub(1),
                    },
                    selection: None,
                });
                return (iced::widget::operation::focus(EDITOR_ID), None);
            }
        }
        (Task::none(), None)
    }

    fn confirm_discard(&mut self, target: Option<IndicatorId>) -> bool {
        if !self.dirty || self.discard_for.as_ref() == Some(&target) {
            self.discard_for = None;
            return true;
        }
        self.discard_for = Some(target);
        self.status = Some(Status::Info(
            "Unsaved changes. Click again to discard them.".to_owned(),
        ));
        false
    }

    fn start_new(&mut self) {
        let taken = self
            .entries
            .iter()
            .map(|entry| entry.id.id().to_owned())
            .chain(
                data::chart::kline::indicators::all_indicators()
                    .iter()
                    .map(|id| id.id().to_owned()),
            )
            .collect::<BTreeSet<_>>();
        let id = (1..)
            .map(|n| format!("user.indicator-{n}"))
            .find(|id| !taken.contains(id))
            .expect("an unused ID exists");
        self.load(Origin::New, &editor::template(&id, "My Indicator"));
        self.status = Some(Status::Info(format!(
            "New indicator {id}. Edit the id in indicator(...) before saving if you like."
        )));
    }

    fn open(&mut self, id: IndicatorId) {
        let editable = self
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .is_some_and(|entry| entry.editable);
        match editor::read_source(&id) {
            Ok(source) => {
                let origin = if editable {
                    Origin::Installed(id.clone())
                } else {
                    Origin::Bundled(id.clone())
                };
                self.load(origin, &source);
                if !editable {
                    self.status = Some(Status::Info(format!(
                        "{} is bundled. Change its id to save an editable copy.",
                        id.id()
                    )));
                }
            }
            Err(error) => self.status = Some(Status::Error(error)),
        }
    }

    fn load(&mut self, origin: Origin, source: &str) {
        self.content = Content::with_text(source);
        self.origin = origin;
        self.diagnostics.clear();
        self.status = None;
        self.dirty = false;
    }

    fn show_errors(&mut self, diagnostics: Vec<SourceDiagnostic>) {
        let count = diagnostics.len();
        self.diagnostics = diagnostics;
        self.status = Some(Status::Error(format!(
            "{count} problem{} found; nothing was saved unless stated below",
            if count == 1 { "" } else { "s" }
        )));
    }

    pub fn view(&self) -> Element<'_, Message> {
        let list = self.source_list();

        let title = match &self.origin {
            Origin::New => "New indicator".to_owned(),
            Origin::Installed(id) => id.id().to_owned(),
            Origin::Bundled(id) => format!("{} (bundled, read-only)", id.id()),
        };
        let cursor = self.content.cursor().position;
        let header = row![
            text(format!("{title}{}", if self.dirty { " •" } else { "" }))
                .size(style::text_size::SECTION),
            space::horizontal(),
            text(format!("Ln {}, Col {}", cursor.line + 1, cursor.column + 1)).size(11),
            button(text("Check")).on_press(Message::Check),
            button(text(if self.saving {
                "Saving…"
            } else {
                "Save (⌘S)"
            }))
            .on_press_maybe((!self.saving).then_some(Message::Save))
            .style(button::primary),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let error_lines = self
            .diagnostics
            .iter()
            .filter_map(|diagnostic| diagnostic.line.map(|line| line - 1))
            .collect::<Vec<_>>();
        let source = iced::widget::text_editor(&self.content)
            .id(EDITOR_ID)
            .on_action(Message::Edit)
            .font(style::AZERET_MONO)
            .size(12)
            .height(Length::Fill)
            .key_binding(key_binding)
            .highlight_with::<ErrorLines>(error_lines, |_, theme| text_editor_format(theme));

        let mut body = column![header, source].spacing(8);
        if let Some(status) = &self.status {
            let line = match status {
                Status::Info(message) => text(message.clone()),
                Status::Success(message) => text(message.clone()).style(text::success),
                Status::Error(message) => text(message.clone()).style(text::danger),
            };
            body = body.push(line.size(12));
        }
        if !self.diagnostics.is_empty() {
            let items = self.diagnostics.iter().map(|diagnostic| {
                let label = text(diagnostic.to_string())
                    .size(12)
                    .style(if diagnostic.is_warning {
                        text::warning
                    } else {
                        text::danger
                    });
                match (diagnostic.line, diagnostic.column) {
                    (Some(line), Some(column)) => button(label)
                        .style(|theme, status| style::button::transparent(theme, status, false))
                        .on_press(Message::JumpTo { line, column })
                        .into(),
                    _ => container(label).padding([2, 4]).into(),
                }
            });
            body = body.push(
                scrollable(column(items).spacing(2))
                    .height(Length::Shrink)
                    .width(Length::Fill),
            );
        }

        container(
            row![
                list,
                rule::vertical(1.0).style(style::split_ruler),
                body.width(Length::Fill)
            ]
            .spacing(12),
        )
        .width(900)
        .height(620)
        .padding(16)
        .style(style::dashboard_modal)
        .into()
    }

    fn source_list(&self) -> Element<'_, Message> {
        let selected = match &self.origin {
            Origin::Installed(id) | Origin::Bundled(id) => Some(id),
            Origin::New => None,
        };
        let entry = |entry: &SourceEntry| {
            let is_selected = selected == Some(&entry.id);
            button(
                column![
                    text(entry.title.clone()).size(12),
                    text(entry.id.id().to_owned()).size(10),
                ]
                .spacing(2),
            )
            .width(Length::Fill)
            .style(move |theme, status| style::button::transparent(theme, status, is_selected))
            .on_press(Message::Open(entry.id.clone()))
            .into()
        };
        let installed = self
            .entries
            .iter()
            .filter(|entry| entry.editable)
            .map(entry)
            .collect::<Vec<Element<'_, Message>>>();
        let bundled = self
            .entries
            .iter()
            .filter(|entry| !entry.editable)
            .map(entry)
            .collect::<Vec<Element<'_, Message>>>();

        let mut list = column![
            button(
                row![icon_text(Icon::Edit, 12), text("New indicator")]
                    .spacing(6)
                    .align_y(Alignment::Center)
            )
            .width(Length::Fill)
            .on_press(Message::New),
            text("Installed").size(11),
        ]
        .spacing(4);
        if installed.is_empty() {
            list = list.push(text("None yet").size(11));
        } else {
            list = list.push(column(installed).spacing(2));
        }
        list = list
            .push(text("Bundled (read-only)").size(11))
            .push(column(bundled).spacing(2));

        scrollable(list.padding(padding_right()))
            .width(220)
            .height(Length::Fill)
            .into()
    }
}

fn padding_right() -> iced::Padding {
    iced::Padding::ZERO.right(10)
}

const EDITOR_ID: &str = "indicator-editor-source";

fn key_binding(event: KeyPress) -> Option<Binding<Message>> {
    match event.key.as_ref() {
        keyboard::Key::Character("s") if event.modifiers.command() => {
            Some(Binding::Custom(Message::Save))
        }
        keyboard::Key::Named(keyboard::key::Named::Tab)
            if !event.modifiers.command() && !event.modifiers.control() =>
        {
            Some(Binding::Sequence(vec![Binding::Insert(' '); 4]))
        }
        // Leave other shortcuts to the app (they are ignored while the editor is open) instead of
        // inserting their character.
        keyboard::Key::Character(c)
            if event.modifiers.command() && !matches!(c, "c" | "x" | "v" | "a") =>
        {
            None
        }
        _ => Binding::from_key_press(event),
    }
}

fn text_editor_format(
    theme: &iced::Theme,
) -> iced::advanced::text::highlighter::Format<iced::Font> {
    iced::advanced::text::highlighter::Format {
        color: Some(theme.extended_palette().danger.base.color),
        font: None,
    }
}

/// Colors the lines that have diagnostics.
struct ErrorLines {
    lines: Vec<usize>,
    current: usize,
}

impl iced::advanced::text::Highlighter for ErrorLines {
    type Settings = Vec<usize>;
    type Highlight = ();
    type Iterator<'a> = std::option::IntoIter<(Range<usize>, ())>;

    fn new(settings: &Self::Settings) -> Self {
        Self {
            lines: settings.clone(),
            current: 0,
        }
    }

    fn update(&mut self, new_settings: &Self::Settings) {
        self.lines.clone_from(new_settings);
        self.current = 0;
    }

    fn change_line(&mut self, line: usize) {
        self.current = self.current.min(line);
    }

    fn highlight_line(&mut self, line: &str) -> Self::Iterator<'_> {
        let index = self.current;
        self.current += 1;
        self.lines
            .contains(&index)
            .then_some((0..line.len(), ()))
            .into_iter()
    }

    fn current_line(&self) -> usize {
        self.current
    }
}
