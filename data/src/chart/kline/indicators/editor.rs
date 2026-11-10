//! Source editing for installed indicators: check a source the way the catalog loader would,
//! save it into the indicators directory, and reload the catalog.

use super::language::PackageOrigin;
use super::{IndicatorId, catalog, loader};
use std::fs;
use std::path::{Path, PathBuf};

/// A problem found while checking a source. Line and column are 1-based and present only when
/// the compiler reported a location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDiagnostic {
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub message: String,
    pub is_warning: bool,
}

impl SourceDiagnostic {
    fn general(message: impl Into<String>) -> Self {
        Self {
            line: None,
            column: None,
            message: message.into(),
            is_warning: false,
        }
    }
}

impl std::fmt::Display for SourceDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.line, self.column) {
            (Some(line), Some(column)) => write!(formatter, "{line}:{column}: {}", self.message),
            _ => formatter.write_str(&self.message),
        }
    }
}

/// An indicator the editor can open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEntry {
    pub id: IndicatorId,
    pub title: String,
    /// Bundled sources open read-only; saving them requires a new ID.
    pub editable: bool,
}

/// Where a successful save wrote the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedSource {
    pub id: IndicatorId,
    pub path: PathBuf,
    /// True when the save created a new file rather than replacing an existing one.
    pub created: bool,
}

/// Indicators in the loaded catalog, installed ones first, each group sorted by ID.
pub fn sources() -> Vec<SourceEntry> {
    let mut entries = catalog()
        .registrations()
        .map(|registration| SourceEntry {
            id: registration.id.clone(),
            title: registration.definition.display_name.clone(),
            editable: registration.origin == PackageOrigin::Installed,
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        right
            .editable
            .cmp(&left.editable)
            .then_with(|| left.id.cmp(&right.id))
    });
    entries
}

/// Read the source text of a loaded indicator.
pub fn read_source(id: &IndicatorId) -> Result<String, String> {
    let registration = catalog()
        .get(id)
        .ok_or_else(|| format!("Indicator '{}' is not loaded", id.id()))?;
    match registration.origin {
        PackageOrigin::Bundled => super::bundled::source_text(id)
            .map(str::to_owned)
            .ok_or_else(|| format!("No embedded source for bundled indicator '{}'", id.id())),
        PackageOrigin::Installed => {
            let directory = loader::installed_directory()?;
            let path = installed_path(&directory, id)?.ok_or_else(|| {
                format!(
                    "No source file for '{}' in {}",
                    id.id(),
                    directory.display()
                )
            })?;
            fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))
        }
    }
}

/// A minimal source for a new indicator, matching `flowsurface-indicator new`.
pub fn template(id: &str, title: &str) -> String {
    format!(
        "language(1)\nindicator({title:?}, id = {id:?})\nformat(precision = \"base_ticker_min_tick\")\nperiod = input.int(14, \"Period\", min = 1, max = 200)\nvalue = ta.sma(chart.close, period)\nplot(value, id = \"value\", title = \"Value\")\n"
    )
}

/// Compile and register a source without writing it. Returns the ID it declares.
pub fn check(source: &str) -> Result<IndicatorId, Vec<SourceDiagnostic>> {
    let compiled =
        indicator_engine::compile(source, Path::new("editor.ind")).map_err(|errors| {
            errors
                .iter()
                .map(|diagnostic| locate(source, diagnostic))
                .collect::<Vec<_>>()
        })?;
    let id = IndicatorId::parse(compiled.descriptor.id.as_str())
        .map_err(|error| vec![SourceDiagnostic::general(format!("Invalid ID: {error}"))])?;
    let registration =
        super::scripting::registration(compiled, PackageOrigin::Installed).map_err(|errors| {
            errors
                .into_iter()
                .map(SourceDiagnostic::general)
                .collect::<Vec<_>>()
        })?;
    registration
        .validate_config(&registration.default_config())
        .map_err(|error| {
            vec![SourceDiagnostic::general(format!(
                "Invalid default configuration: {error}"
            ))]
        })?;
    Ok(id)
}

/// Check a source, write it into the indicators directory and reload the catalog.
///
/// `opened` is the indicator the text was loaded from. Saving under the same ID replaces that
/// file; a different ID creates a new file and leaves the original untouched, and is refused when
/// another installed source already uses the new ID.
pub fn save(
    source: &str,
    opened: Option<&IndicatorId>,
) -> Result<SavedSource, Vec<SourceDiagnostic>> {
    let directory =
        loader::installed_directory().map_err(|error| vec![SourceDiagnostic::general(error)])?;
    let is_bundled = |id: &IndicatorId| {
        catalog()
            .get(id)
            .is_some_and(|registration| registration.origin == PackageOrigin::Bundled)
    };
    let saved = save_in(&directory, source, opened, is_bundled)?;

    loader::reload_indicator_catalog().map_err(|error| {
        vec![SourceDiagnostic::general(format!(
            "Saved to {}, but reloading indicators failed: {error}",
            saved.path.display()
        ))]
    })?;
    if catalog()
        .get(&saved.id)
        .is_none_or(|registration| registration.origin != PackageOrigin::Installed)
    {
        let mut diagnostics = catalog()
            .diagnostics()
            .iter()
            .filter(|diagnostic| diagnostic.contains(saved.id.id()))
            .map(|diagnostic| SourceDiagnostic::general(diagnostic.clone()))
            .collect::<Vec<_>>();
        diagnostics.insert(
            0,
            SourceDiagnostic::general(format!(
                "Saved to {}, but the catalog did not load it",
                saved.path.display()
            )),
        );
        return Err(diagnostics);
    }
    Ok(saved)
}

fn save_in(
    directory: &Path,
    source: &str,
    opened: Option<&IndicatorId>,
    is_bundled: impl Fn(&IndicatorId) -> bool,
) -> Result<SavedSource, Vec<SourceDiagnostic>> {
    let general = |message: String| vec![SourceDiagnostic::general(message)];
    let id = check(source)?;
    if is_bundled(&id) {
        return Err(general(format!(
            "'{}' is a bundled indicator. Change the id in indicator(...) to save a copy, e.g. \"user.{}\"",
            id.id(),
            id.id().rsplit_once('.').map_or(id.id(), |(_, name)| name)
        )));
    }

    let existing = installed_path(directory, &id).map_err(general)?;
    if existing.is_some() && opened != Some(&id) {
        return Err(general(format!(
            "An installed indicator with ID '{}' already exists. Open it to edit, or choose another ID",
            id.id()
        )));
    }

    let created = existing.is_none();
    let destination = match existing {
        Some(path) => path,
        None => {
            let (namespace, _) = id
                .id()
                .rsplit_once('.')
                .expect("validated indicator IDs have a namespace");
            directory.join(namespace).join(
                indicator_engine::IndicatorId::new(id.id())
                    .map_err(general)?
                    .source_file_name(),
            )
        }
    };
    write_atomically(&destination, source.as_bytes(), !created).map_err(general)?;
    Ok(SavedSource {
        id,
        path: destination,
        created,
    })
}

fn installed_path(directory: &Path, id: &IndicatorId) -> Result<Option<PathBuf>, String> {
    if !directory.exists() {
        return Ok(None);
    }
    Ok(loader::installed_sources(directory)?
        .into_iter()
        .find(|loaded| loaded.id == id.id())
        .map(|loaded| loaded.path))
}

/// Write `contents` through a temporary file. With `replace`, an existing destination is
/// replaced; otherwise the write fails if anything already exists there, including a source
/// that could not be loaded and so was not found as an installed indicator.
fn write_atomically(destination: &Path, contents: &[u8], replace: bool) -> Result<(), String> {
    use std::io::Write;

    let parent = destination
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", destination.display()))?;
    // Source discovery skips symlinked directories; writing through one would leave the
    // indicators directory.
    if fs::symlink_metadata(parent).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(format!(
            "{} is a symbolic link; indicators are only saved into real directories",
            parent.display()
        ));
    }
    if !replace && fs::symlink_metadata(destination).is_ok() {
        return Err(format!(
            "{} already exists but is not a loadable indicator with this ID; move or fix it first",
            destination.display()
        ));
    }
    fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    // A non-source extension keeps an interrupted write out of source discovery.
    let temporary = parent.join(format!(".edit-{}.part", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        if replace {
            fs::rename(&temporary, destination)
        } else {
            // A hard link publishes the file atomically and refuses to replace an existing path.
            fs::hard_link(&temporary, destination)
        }
    })();
    if result.is_err() || !replace {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| format!("{}: {error}", destination.display()))
}

/// Convert a compiler diagnostic's byte span into a 1-based line and column of `source`.
fn locate(source: &str, diagnostic: &indicator_engine::Diagnostic) -> SourceDiagnostic {
    let mut offset = diagnostic.span.start.min(source.len());
    while !source.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &source[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let column = source[line_start..offset].chars().count() + 1;
    SourceDiagnostic {
        line: Some(line),
        column: Some(column),
        message: diagnostic.message.clone(),
        is_warning: diagnostic.severity == indicator_engine::Severity::Warning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "flowsurface-editor-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn never_bundled(_: &IndicatorId) -> bool {
        false
    }

    #[test]
    fn template_compiles() {
        let id = check(&template("user.example", "Example")).unwrap();
        assert_eq!(id.id(), "user.example");
    }

    #[test]
    fn errors_report_line_and_column() {
        let source = "language(1)\nindicator(\"Broken\", id = \"user.broken\")\nvalue = ta.sma(chart.close, )\nplot(value)\n";
        let diagnostics = check(source).unwrap_err();
        let located = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.line.is_some())
            .expect("a located diagnostic");
        assert_eq!(located.line, Some(3), "{diagnostics:?}");
        assert!(located.column.is_some_and(|column| column >= 1));
    }

    #[test]
    fn locate_counts_characters_not_bytes() {
        let source = "a\n\u{00e9}\u{00e9}x";
        let offset = source.find('x').unwrap();
        let diagnostic = indicator_engine::Diagnostic::error(
            "editor.ind",
            indicator_engine::Span::new(offset, offset + 1),
            "bad",
        );
        let located = locate(source, &diagnostic);
        assert_eq!((located.line, located.column), (Some(2), Some(3)));
    }

    #[test]
    fn save_creates_then_replaces_in_place() {
        let directory = temp_dir("save");
        let source = template("user.example", "Example");
        let saved = save_in(&directory, &source, None, never_bundled).unwrap();
        assert!(saved.created);
        assert_eq!(saved.path, directory.join("user").join("example.ind"));
        assert_eq!(fs::read_to_string(&saved.path).unwrap(), source);

        let edited = source.replace("Example", "Edited");
        let id = saved.id.clone();
        let replaced = save_in(&directory, &edited, Some(&id), never_bundled).unwrap();
        assert!(!replaced.created);
        assert_eq!(replaced.path, saved.path);
        assert_eq!(fs::read_to_string(&replaced.path).unwrap(), edited);
        let leftovers = fs::read_dir(directory.join("user"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".part"))
            .count();
        assert_eq!(leftovers, 0);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn save_refuses_to_overwrite_another_installed_indicator() {
        let directory = temp_dir("clobber");
        save_in(
            &directory,
            &template("user.first", "First"),
            None,
            never_bundled,
        )
        .unwrap();
        let other = IndicatorId::parse("user.other").unwrap();
        let error = save_in(
            &directory,
            &template("user.first", "Copy"),
            Some(&other),
            never_bundled,
        )
        .unwrap_err();
        assert!(error[0].message.contains("already exists"), "{error:?}");
        let error = save_in(
            &directory,
            &template("user.first", "New"),
            None,
            never_bundled,
        )
        .unwrap_err();
        assert!(error[0].message.contains("already exists"), "{error:?}");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn save_refuses_bundled_ids_and_invalid_sources() {
        let directory = temp_dir("bundled");
        let error = save_in(
            &directory,
            &template("core.example", "Example"),
            None,
            |id: &IndicatorId| id.id().starts_with("core."),
        )
        .unwrap_err();
        assert!(error[0].message.contains("bundled"), "{error:?}");
        assert!(save_in(&directory, "language(1)\nindicator(", None, never_bundled).is_err());
        assert!(
            fs::read_dir(&directory).unwrap().next().is_none(),
            "nothing should be written"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn new_source_never_replaces_an_unloadable_or_mismatched_file() {
        let directory = temp_dir("unloadable");
        let broken = directory.join("user").join("foo.ind");
        fs::create_dir_all(broken.parent().unwrap()).unwrap();
        fs::write(&broken, "language(1)\nindicator(").unwrap();
        let error = save_in(
            &directory,
            &template("user.foo", "Foo"),
            None,
            never_bundled,
        )
        .unwrap_err();
        assert!(error[0].message.contains("already exists"), "{error:?}");
        assert_eq!(
            fs::read_to_string(&broken).unwrap(),
            "language(1)\nindicator("
        );

        // A file named foo.ind that declares another namespace still loads under that ID.
        let other = directory.join("user").join("bar.ind");
        fs::write(&other, template("other.bar", "Other")).unwrap();
        let error = save_in(
            &directory,
            &template("user.bar", "Bar"),
            None,
            never_bundled,
        )
        .unwrap_err();
        assert!(error[0].message.contains("already exists"), "{error:?}");
        assert_eq!(
            fs::read_to_string(&other).unwrap(),
            template("other.bar", "Other")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn save_refuses_a_symlinked_namespace_directory() {
        let directory = temp_dir("symlink");
        let outside = temp_dir("outside");
        std::os::unix::fs::symlink(&outside, directory.join("user")).unwrap();
        let error = save_in(
            &directory,
            &template("user.foo", "Foo"),
            None,
            never_bundled,
        )
        .unwrap_err();
        assert!(error[0].message.contains("symbolic link"), "{error:?}");
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        fs::remove_dir_all(directory).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn edited_file_is_found_where_it_already_lives() {
        let directory = temp_dir("in-place");
        let legacy = directory.join("example.ind");
        fs::write(&legacy, template("user.example", "Example")).unwrap();
        let id = IndicatorId::parse("user.example").unwrap();
        let saved = save_in(
            &directory,
            &template("user.example", "Edited"),
            Some(&id),
            never_bundled,
        )
        .unwrap();
        assert_eq!(saved.path, legacy);
        assert!(!directory.join("user").exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
