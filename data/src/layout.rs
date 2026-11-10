pub use dashboard::Dashboard;
pub use pane::Pane;
use serde::{Deserialize, Serialize};

pub mod dashboard;
pub mod drawings;
pub mod pane;
pub mod presets;
pub mod templates;

pub const MAX_NAME_LENGTH: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layout {
    pub name: String,
    pub dashboard: Dashboard,
    /// Set when the layout was open in its own window at save time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached: Option<DetachedWindow>,
}

/// Where a detached layout window was, and the size of the monitor it was on.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct DetachedWindow {
    pub spec: WindowSpec,
    /// Monitor size reported for the window; `None` when the platform could not tell.
    #[serde(default)]
    pub monitor: Option<(f32, f32)>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            name: "Default".to_string(),
            dashboard: Dashboard::default(),
            detached: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct Window<T = f32> {
    pub width: T,
    pub height: T,
    pub pos_x: T,
    pub pos_y: T,
}

impl<T: Copy> Window<T> {
    pub fn size(&self) -> iced_core::Size<T> {
        iced_core::Size {
            width: self.width,
            height: self.height,
        }
    }

    pub fn position(&self) -> iced_core::Point<T> {
        iced_core::Point {
            x: self.pos_x,
            y: self.pos_y,
        }
    }
}

impl Default for Window<f32> {
    fn default() -> Self {
        Self {
            width: 1024.0,
            height: 768.0,
            pos_x: 0.0,
            pos_y: 0.0,
        }
    }
}

pub type WindowSpec = Window<f32>;

impl From<(&iced_core::Point, &iced_core::Size)> for WindowSpec {
    fn from((point, size): (&iced_core::Point, &iced_core::Size)) -> Self {
        Self {
            width: size.width,
            height: size.height,
            pos_x: point.x,
            pos_y: point.y,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_saved_before_detaching_still_load() {
        let json = serde_json::to_string(&Layout::default()).unwrap();
        assert!(!json.contains("detached"));

        let layout: Layout = serde_json::from_str(&json).unwrap();
        assert!(layout.detached.is_none());
        assert_eq!(layout.name, "Default");
    }

    #[test]
    fn detached_window_round_trips() {
        let layout = Layout {
            detached: Some(DetachedWindow {
                spec: WindowSpec {
                    width: 800.0,
                    height: 600.0,
                    pos_x: -1920.0,
                    pos_y: 32.0,
                },
                monitor: Some((1920.0, 1080.0)),
            }),
            ..Layout::default()
        };

        let json = serde_json::to_string(&layout).unwrap();
        let back: Layout = serde_json::from_str(&json).unwrap();
        let detached = back.detached.expect("detached window is kept");

        assert_eq!(detached.spec.width, 800.0);
        assert_eq!(detached.spec.height, 600.0);
        assert_eq!(detached.spec.pos_x, -1920.0);
        assert_eq!(detached.spec.pos_y, 32.0);
        assert_eq!(detached.monitor, Some((1920.0, 1080.0)));
    }

    #[test]
    fn detached_window_without_monitor_loads() {
        let json = r#"{"name":"A","dashboard":{"pane":"Starter","popout":[]},"detached":{"spec":{"width":1.0,"height":2.0,"pos_x":3.0,"pos_y":4.0}}}"#;
        let layout: Layout = serde_json::from_str(json).unwrap();
        assert!(layout.detached.unwrap().monitor.is_none());
    }
}
