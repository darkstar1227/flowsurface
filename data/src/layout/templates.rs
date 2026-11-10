//! Named layout templates: a dashboard's panes, timeframes and indicators saved to a JSON file
//! in `<data dir>/templates/`, so templates can be shared by copying files.
//!
//! Drawings are not part of a template; main-panel drawings are shared per ticker and
//! indicator-panel drawings are dropped.

use super::{Dashboard, Pane};
use crate::stream::PersistStreamKind;
use exchange::Ticker;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const DIRECTORY: &str = "templates";
const EXTENSION: &str = "json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Template {
    pub name: String,
    pub dashboard: Dashboard,
}

/// A template file on disk.
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub template: Template,
}

impl Template {
    pub fn new(name: impl Into<String>, mut dashboard: Dashboard) -> Self {
        for pane in panes_mut(&mut dashboard) {
            strip_drawings(pane);
        }
        Self {
            name: name.into(),
            dashboard,
        }
    }

    /// The ticker most panes are bound to; ties go to the first one found.
    pub fn primary_ticker(&self) -> Option<Ticker> {
        primary_ticker(&self.dashboard)
    }

    /// The template's dashboard, with `primary` ticker replaced by `ticker` when given.
    /// Other tickers stay as they are.
    pub fn instantiate(&self, ticker: Option<Ticker>) -> Dashboard {
        match (ticker, self.primary_ticker()) {
            (Some(to), Some(from)) if to != from => retarget(&self.dashboard, from, to),
            _ => self.dashboard.clone(),
        }
    }
}

pub fn primary_ticker(dashboard: &Dashboard) -> Option<Ticker> {
    let mut counts = BTreeMap::<Ticker, (usize, usize)>::new();
    let mut order = 0;
    let mut panes = vec![&dashboard.pane];
    panes.extend(dashboard.popout.iter().map(|(pane, _)| pane));
    while let Some(pane) = panes.pop() {
        if let Pane::Split { a, b, .. } = pane {
            panes.push(b);
            panes.push(a);
            continue;
        }
        if let Some(ticker) = stream_types(pane).first().map(stream_ticker) {
            let entry = counts.entry(ticker).or_insert((0, order));
            entry.0 += 1;
            order += 1;
        }
    }
    counts
        .into_iter()
        .max_by(|(_, (count_a, order_a)), (_, (count_b, order_b))| {
            count_a.cmp(count_b).then(order_b.cmp(order_a))
        })
        .map(|(ticker, _)| ticker)
}

fn stream_types(pane: &Pane) -> &[PersistStreamKind] {
    match pane {
        Pane::ShaderHeatmap { stream_type, .. }
        | Pane::KlineChart { stream_type, .. }
        | Pane::TimeAndSales { stream_type, .. }
        | Pane::Ladder { stream_type, .. } => stream_type,
        Pane::Split { .. } | Pane::Starter { .. } => &[],
    }
}

fn stream_ticker(stream: &PersistStreamKind) -> Ticker {
    match stream {
        PersistStreamKind::Kline { ticker, .. } | PersistStreamKind::Trades { ticker } => *ticker,
        PersistStreamKind::Depth(depth) | PersistStreamKind::DepthAndTrades(depth) => depth.ticker,
    }
}

fn panes_mut(dashboard: &mut Dashboard) -> Vec<&mut Pane> {
    let mut leaves = Vec::new();
    let mut stack = vec![&mut dashboard.pane];
    stack.extend(dashboard.popout.iter_mut().map(|(pane, _)| pane));
    while let Some(pane) = stack.pop() {
        if let Pane::Split { a, b, .. } = pane {
            stack.push(a);
            stack.push(b);
        } else {
            leaves.push(pane);
        }
    }
    leaves
}

fn strip_drawings(pane: &mut Pane) {
    if let Pane::KlineChart {
        layout: Some(layout),
        ..
    } = pane
    {
        layout.drawings.clear();
    }
}

/// Replace every occurrence of `from` with `to`, wherever the dashboard stores a ticker.
fn retarget(dashboard: &Dashboard, from: Ticker, to: Ticker) -> Dashboard {
    let (Ok(serde_json::Value::String(from)), Ok(serde_json::Value::String(to))) =
        (serde_json::to_value(from), serde_json::to_value(to))
    else {
        return dashboard.clone();
    };
    let Ok(mut value) = serde_json::to_value(dashboard) else {
        return dashboard.clone();
    };
    replace_strings(&mut value, &from, &to);
    serde_json::from_value(value).unwrap_or_else(|error| {
        log::warn!("Could not retarget template to {to}: {error}");
        dashboard.clone()
    })
}

fn replace_strings(value: &mut serde_json::Value, from: &str, to: &str) {
    match value {
        serde_json::Value::String(text) if text == from => *text = to.to_owned(),
        serde_json::Value::Array(items) => {
            for item in items {
                replace_strings(item, from, to);
            }
        }
        serde_json::Value::Object(map) => {
            let keys = map
                .keys()
                .filter(|key| *key == from)
                .cloned()
                .collect::<Vec<_>>();
            for key in keys {
                if let Some(item) = map.remove(&key) {
                    map.insert(to.to_owned(), item);
                }
            }
            for item in map.values_mut() {
                replace_strings(item, from, to);
            }
        }
        _ => {}
    }
}

pub fn default_directory() -> PathBuf {
    crate::data_path(Some(DIRECTORY))
}

/// Create the templates directory if needed and show it in the file manager.
pub fn open_directory(directory: &Path) -> Result<(), String> {
    fs::create_dir_all(directory)
        .and_then(|()| open::that(directory))
        .map_err(|error| format!("{}: {error}", directory.display()))
}

/// Templates in `directory`, sorted by name. Unreadable files are skipped and logged.
pub fn load_all(directory: &Path) -> Vec<Entry> {
    let Ok(read) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut entries = read
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().and_then(|extension| extension.to_str()) == Some(EXTENSION)
                && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
        })
        .filter_map(|path| {
            let parsed = fs::read_to_string(&path)
                .map_err(|error| error.to_string())
                .and_then(|text| {
                    serde_json::from_str::<Template>(&text).map_err(|error| error.to_string())
                });
            match parsed {
                Ok(template) => Some(Entry { path, template }),
                Err(error) => {
                    log::warn!("Skipping layout template {}: {error}", path.display());
                    None
                }
            }
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left.template
            .name
            .to_lowercase()
            .cmp(&right.template.name.to_lowercase())
            .then_with(|| left.path.cmp(&right.path))
    });
    entries
}

/// Write `template` to a new file named after it, never replacing an existing file.
pub fn save(directory: &Path, template: &Template) -> Result<PathBuf, String> {
    fs::create_dir_all(directory).map_err(|error| format!("{}: {error}", directory.display()))?;
    let json = serde_json::to_string_pretty(template).map_err(|error| error.to_string())?;
    let stem = file_stem(&template.name);
    for attempt in 1..1000 {
        let file_name = if attempt == 1 {
            format!("{stem}.{EXTENSION}")
        } else {
            format!("{stem}-{attempt}.{EXTENSION}")
        };
        let path = directory.join(file_name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                return file
                    .write_all(json.as_bytes())
                    .and_then(|()| file.sync_all())
                    .map(|()| path.clone())
                    .map_err(|error| {
                        let _ = fs::remove_file(&path);
                        format!("{}: {error}", path.display())
                    });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    }
    Err(format!("Too many templates named {stem}"))
}

/// Delete a template file, only if it is a template inside `directory`.
pub fn delete(directory: &Path, path: &Path) -> Result<(), String> {
    if path.parent() != Some(directory)
        || path.extension().and_then(|extension| extension.to_str()) != Some(EXTENSION)
    {
        return Err(format!("{} is not a layout template", path.display()));
    }
    fs::remove_file(path).map_err(|error| format!("{}: {error}", path.display()))
}

/// A file name derived from a template name: letters, digits, `-` and `_` only.
fn file_stem(name: &str) -> String {
    let stem = name
        .trim()
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned();
    if stem.is_empty() {
        "template".to_owned()
    } else {
        stem
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::adapter::Exchange;

    fn ticker(symbol: &str) -> Ticker {
        Ticker::new(symbol, Exchange::BinanceLinear)
    }

    fn kline(symbol: &str) -> Pane {
        Pane::KlineChart {
            layout: Some(Box::default()),
            stream_type: vec![PersistStreamKind::Kline {
                ticker: ticker(symbol),
                timeframe: exchange::Timeframe::M5,
            }],
            settings: Default::default(),
            link_group: None,
        }
    }

    fn split(a: Pane, b: Pane) -> Pane {
        Pane::Split {
            axis: super::super::pane::Axis::Vertical,
            ratio: 0.5,
            a: Box::new(a),
            b: Box::new(b),
        }
    }

    fn dashboard(pane: Pane) -> Dashboard {
        Dashboard {
            pane,
            ..Default::default()
        }
    }

    fn tickers(dashboard: &Dashboard) -> Vec<Ticker> {
        let mut found = Vec::new();
        let mut stack = vec![&dashboard.pane];
        while let Some(pane) = stack.pop() {
            if let Pane::Split { a, b, .. } = pane {
                stack.push(b);
                stack.push(a);
            } else {
                found.extend(stream_types(pane).iter().map(stream_ticker));
            }
        }
        found
    }

    #[test]
    fn primary_ticker_is_the_most_used_one() {
        let template = Template::new(
            "t",
            dashboard(split(
                kline("ETHUSDT"),
                split(kline("BTCUSDT"), kline("BTCUSDT")),
            )),
        );
        assert_eq!(template.primary_ticker(), Some(ticker("BTCUSDT")));
    }

    #[test]
    fn instantiate_replaces_only_the_primary_ticker() {
        let template = Template::new(
            "t",
            dashboard(split(
                kline("BTCUSDT"),
                split(kline("BTCUSDT"), kline("ETHUSDT")),
            )),
        );
        let applied = template.instantiate(Some(ticker("SOLUSDT")));
        assert_eq!(
            tickers(&applied),
            vec![ticker("SOLUSDT"), ticker("SOLUSDT"), ticker("ETHUSDT")]
        );
        assert_eq!(
            tickers(&template.instantiate(None)),
            vec![ticker("BTCUSDT"), ticker("BTCUSDT"), ticker("ETHUSDT")]
        );
    }

    #[test]
    fn save_load_and_delete_round_trip() {
        let directory = std::env::temp_dir().join(format!(
            "flowsurface-templates-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let template = Template::new("Scalp / 1m", dashboard(kline("BTCUSDT")));
        let first = save(&directory, &template).unwrap();
        let second = save(&directory, &template).unwrap();
        assert_eq!(first.file_name().unwrap(), "Scalp---1m.json");
        assert_ne!(first, second, "a second save must not replace the first");

        let loaded = load_all(&directory);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].template.name, "Scalp / 1m");
        assert_eq!(loaded[0].template.primary_ticker(), Some(ticker("BTCUSDT")));

        fs::write(directory.join("broken.json"), "{").unwrap();
        assert_eq!(load_all(&directory).len(), 2, "broken files are skipped");

        assert!(delete(&directory, Path::new("/etc/hosts")).is_err());
        delete(&directory, &first).unwrap();
        assert_eq!(load_all(&directory).len(), 1);
        fs::remove_dir_all(directory).unwrap();
    }
}
