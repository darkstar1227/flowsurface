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
    FromPine,
    PineEdit(text_editor::Action),
    PineConvert,
    PineCancel,
    Save,
    Saved(SaveStamp, Result<SavedSource, Vec<SourceDiagnostic>>),
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

/// The editor state a save started from, to tell whether the text changed while it was running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SaveStamp {
    /// Bumped whenever other source text is loaded.
    loaded: u64,
    /// Bumped on every text change, including loads.
    edited: u64,
}

impl SaveStamp {
    /// The saved text still belongs to the open source (same load).
    fn same_source(self, current: Self) -> bool {
        self.loaded == current.loaded
    }

    /// The editor text is still exactly what was saved.
    fn unchanged(self, current: Self) -> bool {
        self == current
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
    /// Pine source being typed; `Some` while the Pine input replaces the source editor.
    pine: Option<Content>,
    /// Converting would replace unsaved text; converting again confirms.
    pine_discard: bool,
    stamp: SaveStamp,
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
            pine: None,
            pine_discard: false,
            stamp: SaveStamp::default(),
        };
        editor.start_new();
        editor
    }

    /// Re-read the catalog listing and leave Pine input mode; call when the editor is shown.
    pub fn refresh(&mut self) {
        self.entries = editor::sources();
        self.pine = None;
        self.pine_discard = false;
    }

    pub fn update(&mut self, message: Message) -> (Task<Message>, Option<Action>) {
        if !matches!(message, Message::New | Message::Open(_)) {
            self.discard_for = None;
        }
        if !matches!(message, Message::PineConvert) {
            self.pine_discard = false;
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
                    self.stamp.edited += 1;
                }
                self.content.perform(action);
            }
            // The source editor is hidden in Pine mode; Save and Check are ignored there.
            Message::Check | Message::Save if self.pine.is_some() => {}
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
            Message::FromPine => {
                self.pine.get_or_insert_with(Content::new);
                self.status = Some(Status::Info(
                    "Paste Pine Script v5/v6, then press Convert.".to_owned(),
                ));
            }
            Message::PineEdit(action) => {
                self.pine_discard = false;
                if let Some(pine) = &mut self.pine {
                    pine.perform(action);
                }
            }
            Message::PineCancel => {
                self.pine = None;
                self.status = None;
            }
            Message::PineConvert => {
                let Some(pine) = &self.pine else {
                    return (Task::none(), None);
                };
                if self.dirty && !self.pine_discard {
                    self.pine_discard = true;
                    self.status = Some(Status::Info(
                        "Unsaved changes. Press Convert again to replace them.".to_owned(),
                    ));
                    return (Task::none(), None);
                }
                let transpiled = pine::transpile(&pine.text(), &pine::Options::default());
                let mut diagnostics = pine_diagnostics(&transpiled);
                self.pine = None;
                self.load(Origin::New, &transpiled.source);
                self.dirty = true;
                match editor::check(&transpiled.source) {
                    Ok(id) => {
                        let converted = format!("Converted from Pine as {}", id.id());
                        self.diagnostics = diagnostics;
                        self.status = Some(if self.diagnostics.iter().any(|d| !d.is_warning) {
                            Status::Error(format!("{converted}, but some Pine lines were skipped"))
                        } else {
                            Status::Success(format!("{converted}. Press Save to keep it."))
                        });
                    }
                    Err(errors) => {
                        diagnostics.extend(errors);
                        self.show_errors(diagnostics);
                    }
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
                let stamp = self.stamp;
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
                        move |result| Message::Saved(stamp, result),
                    ),
                    None,
                );
            }
            Message::Saved(stamp, result) => {
                self.saving = false;
                self.entries = editor::sources();
                let same_source = stamp.same_source(self.stamp);
                match result {
                    Ok(saved) => {
                        let replaced_other = same_source
                            && self
                                .origin
                                .installed_id()
                                .is_some_and(|opened| *opened != saved.id);
                        if same_source {
                            self.diagnostics.clear();
                        }
                        if stamp.unchanged(self.stamp) {
                            self.dirty = false;
                        }
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
                        if same_source {
                            self.origin = Origin::Installed(saved.id.clone());
                        }
                        return (Task::none(), Some(Action::Reloaded(saved.id)));
                    }
                    Err(diagnostics) if same_source => self.show_errors(diagnostics),
                    // Positions in these diagnostics refer to text that is no longer open.
                    Err(_) => {
                        self.status = Some(Status::Error(
                            "The previous save failed; nothing was saved.".to_owned(),
                        ));
                    }
                }
            }
            Message::JumpTo { line, column } => {
                let line = line.saturating_sub(1);
                // Diagnostics count characters; the editor cursor is a byte index into the line.
                let column = self
                    .content
                    .line(line)
                    .map_or(0, |text| byte_column(&text.text, column.saturating_sub(1)));
                self.content.move_to(Cursor {
                    position: Position { line, column },
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
        self.pine = None;
        self.stamp.loaded += 1;
        self.stamp.edited += 1;
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
        let cursor_column = self
            .content
            .line(cursor.line)
            .map_or(cursor.column, |line| char_column(&line.text, cursor.column));
        let header = row![
            text(format!("{title}{}", if self.dirty { " •" } else { "" }))
                .size(style::text_size::SECTION),
            space::horizontal(),
            text(format!("Ln {}, Col {}", cursor.line + 1, cursor_column + 1)).size(11),
            button(text("From Pine"))
                .on_press_maybe((self.pine.is_none()).then_some(Message::FromPine)),
            button(text("Check")).on_press_maybe(self.pine.is_none().then_some(Message::Check)),
            button(text(if self.saving {
                "Saving…"
            } else {
                "Save (⌘S)"
            }))
            .on_press_maybe((!self.saving && self.pine.is_none()).then_some(Message::Save))
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

        let mut body = column![header].spacing(8);
        if let Some(pine) = &self.pine {
            body = body
                .push(
                    row![
                        text("Pine Script input").size(12),
                        space::horizontal(),
                        button(text("Cancel")).on_press(Message::PineCancel),
                        button(text("Convert"))
                            .on_press(Message::PineConvert)
                            .style(button::primary),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .push(
                    iced::widget::text_editor(pine)
                        .on_action(Message::PineEdit)
                        .font(style::AZERET_MONO)
                        .size(12)
                        .height(Length::Fill),
                );
        } else {
            body = body.push(source);
        }
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

/// Pine translation problems as diagnostics. They carry no editor position because their line
/// numbers refer to the Pine source, not the converted `.ind` text.
fn pine_diagnostics(transpiled: &pine::Transpiled) -> Vec<SourceDiagnostic> {
    let make = |item: &pine::Unsupported, is_warning: bool| SourceDiagnostic {
        line: None,
        column: None,
        message: format!("Pine line {}: {}", item.line, item.reason),
        is_warning,
    };
    transpiled
        .unsupported
        .iter()
        .map(|item| make(item, false))
        .chain(transpiled.notes.iter().map(|item| make(item, true)))
        .collect()
}

fn padding_right() -> iced::Padding {
    iced::Padding::ZERO.right(10)
}

/// Byte offset of the `chars`-th character of `line`, clamped to the line end.
fn byte_column(line: &str, chars: usize) -> usize {
    line.char_indices()
        .nth(chars)
        .map_or(line.len(), |(index, _)| index)
}

/// Number of characters before byte offset `bytes` of `line`.
fn char_column(line: &str, bytes: usize) -> usize {
    line.char_indices()
        .take_while(|(index, _)| *index < bytes)
        .count()
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

#[cfg(test)]
mod tests {
    use super::{SaveStamp, byte_column, char_column, pine_diagnostics};

    #[test]
    fn columns_convert_between_chars_and_bytes() {
        let line = "// 高低 x";
        assert_eq!(byte_column(line, 6), line.find('x').unwrap());
        assert_eq!(char_column(line, line.find('x').unwrap()), 6);
        assert_eq!(byte_column(line, 100), line.len());
        assert_eq!(char_column(line, 0), 0);
    }

    #[test]
    fn save_stamp_detects_changes_made_while_saving() {
        let sent = SaveStamp::default();
        assert!(sent.same_source(sent) && sent.unchanged(sent));

        let edited = SaveStamp { edited: 1, ..sent };
        assert!(sent.same_source(edited));
        assert!(!sent.unchanged(edited));

        let loaded = SaveStamp {
            loaded: 1,
            edited: 1,
        };
        assert!(!sent.same_source(loaded));
        assert!(!sent.unchanged(loaded));
    }

    #[test]
    fn pine_problems_become_unpositioned_diagnostics() {
        let transpiled = pine::Transpiled {
            source: String::new(),
            id: "pine.x".to_owned(),
            unsupported: vec![pine::Unsupported {
                line: 12,
                reason: "request.security".to_owned(),
            }],
            notes: vec![pine::Unsupported {
                line: 3,
                reason: "hline became a plot".to_owned(),
            }],
        };
        let diagnostics = pine_diagnostics(&transpiled);
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].message, "Pine line 12: request.security");
        assert!(!diagnostics[0].is_warning);
        assert_eq!(diagnostics[1].message, "Pine line 3: hline became a plot");
        assert!(diagnostics[1].is_warning);
        assert!(
            diagnostics
                .iter()
                .all(|d| d.line.is_none() && d.column.is_none())
        );
    }
}
