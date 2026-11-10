//! Kline chart drawings shared by every pane and layout, keyed by ticker.

use crate::chart::kline::DrawingConfig;
use crate::chart::kline::drawing::DrawingId;
use exchange::Ticker;
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;

/// The persisted form: drawings of each ticker.
pub type Config = BTreeMap<Ticker, Vec<DrawingConfig>>;

#[derive(Debug, Default)]
pub struct Library {
    entries: Config,
    /// Bumped on every change of a ticker's drawings; runtime only.
    revisions: FxHashMap<Ticker, u64>,
}

/// What a chart last exchanged with the [`Library`].
#[derive(Debug, Clone, PartialEq)]
pub struct Synced {
    revision: u64,
    drawings: Vec<DrawingConfig>,
}

impl Library {
    pub fn from_config(mut entries: Config) -> Self {
        entries.retain(|_, drawings| !drawings.is_empty());
        Self {
            entries,
            revisions: FxHashMap::default(),
        }
    }

    pub fn to_config(&self) -> Config {
        self.entries.clone()
    }

    pub fn get(&self, ticker: Ticker) -> &[DrawingConfig] {
        self.entries.get(&ticker).map_or(&[], Vec::as_slice)
    }

    pub fn revision(&self, ticker: Ticker) -> u64 {
        self.revisions.get(&ticker).copied().unwrap_or(0)
    }

    fn set(&mut self, ticker: Ticker, drawings: Vec<DrawingConfig>) -> u64 {
        if drawings.is_empty() {
            self.entries.remove(&ticker);
        } else {
            self.entries.insert(ticker, drawings);
        }
        let revision = self.revisions.entry(ticker).or_insert(0);
        *revision += 1;
        *revision
    }

    /// Reconciles one chart's drawings of `ticker` with the library.
    ///
    /// `local` is what the chart shows now and `synced` what it last exchanged. A chart
    /// that edited its drawings publishes them; a chart that fell behind receives the
    /// library's. Returns the drawings the chart must load, if any.
    ///
    /// A chart that never synced (`synced` is `None`) and already holds drawings came
    /// from a config saved before drawings were shared; those are merged into the
    /// library with fresh IDs instead of being dropped.
    pub fn sync(
        &mut self,
        ticker: Ticker,
        local: &[DrawingConfig],
        synced: &mut Option<Synced>,
    ) -> Option<Vec<DrawingConfig>> {
        match synced {
            Some(state) if state.drawings != local => {
                let revision = self.set(ticker, local.to_vec());
                *state = Synced {
                    revision,
                    drawings: local.to_vec(),
                };
                None
            }
            Some(state) if state.revision == self.revision(ticker) => None,
            Some(_) => Some(self.adopt(ticker, synced)),
            None if local.is_empty() => Some(self.adopt(ticker, synced)),
            None => {
                let merged = merge(self.get(ticker), local);
                let revision = self.set(ticker, merged.clone());
                *synced = Some(Synced {
                    revision,
                    drawings: merged.clone(),
                });
                Some(merged)
            }
        }
    }

    fn adopt(&self, ticker: Ticker, synced: &mut Option<Synced>) -> Vec<DrawingConfig> {
        let drawings = self.get(ticker).to_vec();
        *synced = Some(Synced {
            revision: self.revision(ticker),
            drawings: drawings.clone(),
        });
        drawings
    }
}

/// Appends `extra` to `shared`, renumbering `extra` past the highest shared ID.
fn merge(shared: &[DrawingConfig], extra: &[DrawingConfig]) -> Vec<DrawingConfig> {
    let first_id = shared.iter().map(|d| d.id.0).max().unwrap_or(0) + 1;
    let renumbered = (first_id..).zip(extra).map(|(id, drawing)| DrawingConfig {
        id: DrawingId(id),
        ..drawing.clone()
    });
    shared.iter().cloned().chain(renumbered).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::kline::drawing::{DrawingEntity, DrawingObject, DrawingStyle, DrawingX};
    use exchange::UnixMs;
    use exchange::adapter::Exchange;

    fn btc() -> Ticker {
        Ticker::new("BTCUSDT", Exchange::BinanceLinear)
    }

    fn eth() -> Ticker {
        Ticker::new("ETHUSDT", Exchange::BinanceLinear)
    }

    fn line(id: u64) -> DrawingConfig {
        DrawingEntity {
            id: DrawingId(id),
            object: DrawingObject::VerticalLine {
                x: DrawingX::Time(UnixMs::new(1_700_000_000_000 + id)),
            },
            style: DrawingStyle::default(),
            locked: false,
            visible: true,
        }
    }

    #[test]
    fn edit_in_one_chart_reaches_another_chart_of_the_same_ticker() {
        let mut library = Library::default();
        let (mut a, mut b) = (None, None);
        assert_eq!(library.sync(btc(), &[], &mut a), Some(vec![]));
        assert_eq!(library.sync(btc(), &[], &mut b), Some(vec![]));

        assert_eq!(library.sync(btc(), &[line(1)], &mut a), None);
        assert_eq!(library.sync(btc(), &[], &mut b), Some(vec![line(1)]));
        assert_eq!(library.sync(btc(), &[line(1)], &mut b), None);
        assert_eq!(library.sync(btc(), &[line(1)], &mut a), None);
    }

    #[test]
    fn tickers_do_not_share_drawings() {
        let mut library = Library::default();
        let mut a = None;
        library.sync(btc(), &[], &mut a);
        library.sync(btc(), &[line(1)], &mut a);

        let mut b = None;
        assert_eq!(library.sync(eth(), &[], &mut b), Some(vec![]));
        assert!(library.get(eth()).is_empty());
    }

    #[test]
    fn new_chart_loads_saved_drawings() {
        let mut library = Library::from_config(Config::from([(btc(), vec![line(1)])]));
        let mut chart = None;
        assert_eq!(library.sync(btc(), &[], &mut chart), Some(vec![line(1)]));
        assert_eq!(library.sync(btc(), &[line(1)], &mut chart), None);
    }

    #[test]
    fn deleting_all_drawings_removes_the_entry() {
        let mut library = Library::from_config(Config::from([(btc(), vec![line(1)])]));
        let mut chart = None;
        library.sync(btc(), &[], &mut chart);
        library.sync(btc(), &[], &mut chart);
        assert!(library.to_config().is_empty());
    }

    #[test]
    fn legacy_pane_drawings_are_merged_with_fresh_ids() {
        let mut library = Library::from_config(Config::from([(btc(), vec![line(1)])]));
        let mut legacy = None;
        let merged = library.sync(btc(), &[line(1)], &mut legacy).unwrap();

        assert_eq!(merged.len(), 2);
        assert_eq!(merged[1].id, DrawingId(2));
        assert_eq!(merged[1].object, line(1).object);
        assert_eq!(library.get(btc()), merged.as_slice());
    }

    #[test]
    fn empty_entries_are_dropped_on_load() {
        let library = Library::from_config(Config::from([(btc(), vec![])]));
        assert!(library.to_config().is_empty());
    }

    #[test]
    fn config_round_trips_through_json() {
        let config = Config::from([(btc(), vec![line(1)]), (eth(), vec![line(2)])]);
        let json = serde_json::to_string(&config).unwrap();
        let restored: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, config);
    }
}
