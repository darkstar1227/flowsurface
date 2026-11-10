use super::ScaleFactor;
use super::sidebar::Sidebar;
use super::timezone::UserTimezone;
use crate::config::network::Network;
use crate::layout::WindowSpec;
use crate::{AudioStream, Layout, Theme};

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct Layouts {
    pub layouts: Vec<Layout>,
    pub active_layout: Option<String>,
    #[serde(default)]
    pub chart_presets: Vec<crate::chart::kline::ChartPreset>,
    /// Kline drawings shared by every pane, keyed by ticker.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub drawings: crate::layout::drawings::Config,
}

#[derive(Default, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct State {
    pub layout_manager: Layouts,
    pub selected_theme: Theme,
    pub custom_theme: Option<Theme>,
    pub main_window: Option<WindowSpec>,
    pub timezone: UserTimezone,
    pub sidebar: Sidebar,
    pub scale_factor: ScaleFactor,
    pub audio_cfg: AudioStream,
    pub network: Network,
    pub exchange_accounts: super::exchange_accounts::ExchangeAccounts,
    pub size_in_quote_ccy: exchange::SizeUnit,
    pub cache_market_metadata: bool,
    #[serde(default = "default_antialiasing")]
    pub antialiasing: bool,
}

impl State {
    pub fn from_parts(
        layout_manager: Layouts,
        selected_theme: Theme,
        custom_theme: Option<Theme>,
        main_window: Option<WindowSpec>,
        timezone: UserTimezone,
        sidebar: Sidebar,
        scale_factor: ScaleFactor,
        audio_cfg: AudioStream,
        network: Network,
        exchange_accounts: super::exchange_accounts::ExchangeAccounts,
        volume_size_unit: exchange::SizeUnit,
        cache_market_metadata: bool,
        antialiasing: bool,
    ) -> Self {
        State {
            layout_manager,
            selected_theme: Theme(selected_theme.0),
            custom_theme: custom_theme.map(|t| Theme(t.0)),
            main_window,
            timezone,
            sidebar,
            scale_factor,
            audio_cfg,
            network,
            exchange_accounts,
            size_in_quote_ccy: volume_size_unit,
            cache_market_metadata,
            antialiasing,
        }
    }
}

const fn default_antialiasing() -> bool {
    true
}
