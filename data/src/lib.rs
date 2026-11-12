pub mod aggr;
pub mod audio;
pub mod chart;
pub mod config;
pub mod layout;
pub mod log;
pub mod metadata;
pub mod panel;
pub mod stream;
pub mod tickers_table;
pub mod util;

use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;

pub use audio::AudioStream;
pub use chart::kline::indicators::{
    IndicatorCatalogPaths, initialize_indicator_catalog, initialize_indicator_catalog_with_paths,
};
pub use config::ScaleFactor;
pub use config::exchange_accounts::{AccountMeta, ExchangeAccounts};
pub use config::network::{Network, TradeFetchMode};
pub use config::sidebar::{self, Sidebar};
pub use config::state::{Layouts, State};
pub use config::theme::Theme;
pub use config::timezone::UserTimezone;

#[doc(hidden)]
pub use ::exchange;
use ::log::{error, info, warn};
#[doc(hidden)]
pub use ::serde;
pub use layout::{Dashboard, Layout, Pane};
pub use metadata::{MarketMetadata, MetadataSource};

use crate::layout::pane::VisualConfig;

pub const SAVED_STATE_PATH: &str = "saved-state.json";

#[derive(Debug, Clone)]
pub enum PaneSettingsSnapshot {
    Heatmap(chart::heatmap::SettingsSnapshot),
    Kline(chart::kline::SettingsSnapshot),
    TimeAndSales(Option<panel::timesales::Config>),
    Ladder(Option<panel::ladder::Config>),
}

impl PaneSettingsSnapshot {
    pub fn visual_config(&self) -> Option<VisualConfig> {
        match self {
            Self::Heatmap(snapshot) => snapshot.config.map(VisualConfig::Heatmap),
            Self::TimeAndSales(config) => config.map(VisualConfig::TimeAndSales),
            Self::Ladder(config) => config.map(VisualConfig::Ladder),
            Self::Kline(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PaneSettingsTransfer {
    Visual(Option<VisualConfig>),
    Kline,
}

#[derive(thiserror::Error, Debug, Clone)]
pub enum InternalError {
    #[error("Fetch error: {0}")]
    Fetch(String),
    #[error("Layout error: {0}")]
    Layout(String),
}

pub fn write_json_to_file(json: &str, file_name: &str) -> std::io::Result<()> {
    let path = data_path(Some(file_name));

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid state file path")
    })?;

    if !parent.exists() {
        std::fs::create_dir_all(parent)?;
    }

    // Write to a temp file and rename so the previous good state survives a
    // crash or partial write.
    let tmp_path = path.with_extension("tmp");

    let write_result = File::create(&tmp_path).and_then(|mut file| {
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp_path, &path)
    });

    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    Ok(())
}

pub fn read_from_file(file_name: &str) -> Result<State, Box<dyn std::error::Error>> {
    let path = data_path(Some(file_name));

    let file_open_result = File::open(&path);
    let mut file = match file_open_result {
        Ok(file) => file,
        Err(e) => return Err(Box::new(e)),
    };

    let mut contents = String::new();
    if let Err(e) = file.read_to_string(&mut contents) {
        return Err(Box::new(e));
    }

    match serde_json::from_str(&contents) {
        Ok(state) => Ok(state),
        Err(e) => {
            // If parsing fails, backup the file
            drop(file); // Close the file before renaming

            // Create backup file with different name to prevent overwriting it
            let backup_file_name = if let Some(pos) = file_name.rfind('.') {
                format!("{}_old{}", &file_name[..pos], &file_name[pos..])
            } else {
                format!("{}_old", file_name)
            };

            let backup_path = data_path(Some(&backup_file_name));

            if let Err(rename_err) = std::fs::rename(&path, &backup_path) {
                warn!(
                    "Failed to backup corrupted state file '{}' to '{}': {}",
                    path.display(),
                    backup_path.display(),
                    rename_err
                );
            } else {
                info!(
                    "Backed up corrupted state file to '{}'. It can be restored manually.",
                    backup_path.display()
                );
            }

            Err(Box::new(e))
        }
    }
}

pub fn open_data_folder() -> Result<(), InternalError> {
    let pathbuf = data_path(None);

    if pathbuf.exists() {
        if let Err(err) = open::that(&pathbuf) {
            Err(InternalError::Layout(format!(
                "Failed to open data folder: {:?}, error: {}",
                pathbuf, err
            )))
        } else {
            info!("Opened data folder: {:?}", pathbuf);
            Ok(())
        }
    } else {
        Err(InternalError::Layout(format!(
            "Data folder does not exist: {:?}",
            pathbuf
        )))
    }
}

pub fn open_url(url: &str) -> Result<(), InternalError> {
    if let Err(err) = open::that(url) {
        Err(InternalError::Layout(format!(
            "Failed to open URL '{}': {}",
            url, err
        )))
    } else {
        info!("Opened URL: {url}");
        Ok(())
    }
}

/// Data directory name of this fork, kept apart from upstream's `flowsurface/`.
pub const DATA_DIR_NAME: &str = "flowsurface-fork";
/// Data directory name used by upstream Flowsurface builds.
const UPSTREAM_DATA_DIR_NAME: &str = "flowsurface";

pub fn data_path(path_name: Option<&str>) -> PathBuf {
    if let Ok(path) = std::env::var("FLOWSURFACE_DATA_PATH") {
        let root = PathBuf::from(path);
        match path_name {
            Some(name) => root.join(name),
            None => root,
        }
    } else {
        let data_dir = dirs_next::data_dir().unwrap_or_else(|| PathBuf::from("."));
        if let Some(path_name) = path_name {
            data_dir.join(DATA_DIR_NAME).join(path_name)
        } else {
            data_dir.join(DATA_DIR_NAME)
        }
    }
}

/// On the first start of this fork, copy upstream's `saved-state.json` into the fork's own
/// data directory so layouts carry over. Upstream's files are only read, never changed.
///
/// Does nothing when `FLOWSURFACE_DATA_PATH` is set or the fork's data directory already
/// exists. Must run before anything creates the data directory (the logger does).
/// Returns the copied file's path when a copy was made.
pub fn import_upstream_saved_state() -> Result<Option<PathBuf>, std::io::Error> {
    if std::env::var_os("FLOWSURFACE_DATA_PATH").is_some() {
        return Ok(None);
    }
    let Some(base) = dirs_next::data_dir() else {
        return Ok(None);
    };
    import_saved_state_from(
        &base.join(UPSTREAM_DATA_DIR_NAME),
        &base.join(DATA_DIR_NAME),
    )
}

fn import_saved_state_from(
    upstream_dir: &std::path::Path,
    fork_dir: &std::path::Path,
) -> Result<Option<PathBuf>, std::io::Error> {
    if fork_dir.exists() {
        return Ok(None);
    }
    let source = upstream_dir.join(SAVED_STATE_PATH);
    if !source.is_file() {
        return Ok(None);
    }
    std::fs::create_dir_all(fork_dir)?;
    let target = fork_dir.join(SAVED_STATE_PATH);
    std::fs::copy(&source, &target)?;
    Ok(Some(target))
}

fn cleanup_directory(data_path: &PathBuf) -> usize {
    if !data_path.exists() {
        warn!("Data path {:?} does not exist, skipping cleanup", data_path);
        return 0;
    }

    let re =
        regex::Regex::new(r".*-(\d{4}-\d{2}-\d{2})\.zip$").expect("Cleanup regex pattern is valid");
    let today = chrono::Local::now().date_naive();
    let mut deleted_files = Vec::new();

    let entries = match std::fs::read_dir(data_path) {
        Ok(entries) => entries,
        Err(e) => {
            error!("Failed to read data directory {:?}: {}", data_path, e);
            return 0;
        }
    };

    for entry in entries.filter_map(Result::ok) {
        let symbol_dir = match std::fs::read_dir(entry.path()) {
            Ok(dir) => dir,
            Err(e) => {
                error!("Failed to read symbol directory {:?}: {}", entry.path(), e);
                continue;
            }
        };

        for file in symbol_dir.filter_map(Result::ok) {
            let path = file.path();
            let Some(filename) = path.to_str() else {
                continue;
            };

            if let Some(cap) = re.captures(filename)
                && let Ok(file_date) = chrono::NaiveDate::parse_from_str(&cap[1], "%Y-%m-%d")
            {
                let days_old = today.signed_duration_since(file_date).num_days();
                if days_old > 4 {
                    if let Err(e) = std::fs::remove_file(&path) {
                        error!("Failed to remove old file {}: {}", filename, e);
                    } else {
                        deleted_files.push(filename.to_string());
                        info!("Removed old file: {}", filename);
                    }
                }
            }
        }
    }

    deleted_files.len()
}

pub fn load_antialiasing() -> bool {
    read_from_file(SAVED_STATE_PATH)
        .map(|state| state.antialiasing)
        .unwrap_or_else(|_| true)
}

pub fn cleanup_old_market_data() -> usize {
    let paths = ["um", "cm"].map(|market_type| {
        data_path(Some(&format!(
            "market_data/binance/data/futures/{}/daily/aggTrades",
            market_type
        )))
    });

    let total_deleted: usize = paths.iter().map(cleanup_directory).sum();

    info!("File cleanup completed. Deleted {} files", total_deleted);
    total_deleted
}

#[cfg(test)]
mod fork_data_dir_tests {
    use super::{SAVED_STATE_PATH, import_saved_state_from};

    fn temp_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "flowsurface-fork-import-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn copies_upstream_state_into_missing_fork_dir() {
        let root = temp_root("copy");
        let upstream = root.join("flowsurface");
        let fork = root.join("flowsurface-fork");
        std::fs::create_dir_all(&upstream).unwrap();
        std::fs::write(upstream.join(SAVED_STATE_PATH), "{\"upstream\":1}").unwrap();

        let copied = import_saved_state_from(&upstream, &fork).unwrap();

        assert_eq!(copied, Some(fork.join(SAVED_STATE_PATH)));
        assert_eq!(
            std::fs::read_to_string(fork.join(SAVED_STATE_PATH)).unwrap(),
            "{\"upstream\":1}"
        );
        assert_eq!(
            std::fs::read_to_string(upstream.join(SAVED_STATE_PATH)).unwrap(),
            "{\"upstream\":1}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn leaves_existing_fork_dir_alone() {
        let root = temp_root("existing");
        let upstream = root.join("flowsurface");
        let fork = root.join("flowsurface-fork");
        std::fs::create_dir_all(&upstream).unwrap();
        std::fs::create_dir_all(&fork).unwrap();
        std::fs::write(upstream.join(SAVED_STATE_PATH), "{\"upstream\":1}").unwrap();

        assert_eq!(import_saved_state_from(&upstream, &fork).unwrap(), None);
        assert!(!fork.join(SAVED_STATE_PATH).exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn does_nothing_without_upstream_state() {
        let root = temp_root("none");
        let upstream = root.join("flowsurface");
        let fork = root.join("flowsurface-fork");

        assert_eq!(import_saved_state_from(&upstream, &fork).unwrap(), None);
        assert!(!fork.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
