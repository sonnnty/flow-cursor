//! Settings owned by the hub, persisted in its volume and pushed live to
//! every agent over the control stream.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PathPolicy {
    /// Use the direct agent ↔ agent path, fall back to the hub relay.
    #[default]
    PreferP2p,
    /// Always relay input through the hub.
    ForceRelay,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardSettings {
    pub enabled: bool,
    pub text: bool,
    pub images: bool,
    pub max_text_bytes: u32,
    pub max_image_bytes: u32,
}

impl Default for ClipboardSettings {
    fn default() -> Self {
        ClipboardSettings {
            enabled: true,
            text: true,
            images: true,
            max_text_bytes: 1024 * 1024,
            max_image_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EdgeSettings {
    /// The pointer must keep pushing against the edge this long before
    /// switching (0 = immediate).
    pub switch_delay_ms: u32,
    /// Require two separate bumps against the edge within
    /// `double_bump_window_ms`.
    pub double_bump: bool,
    pub double_bump_window_ms: u32,
    /// Pixels from a monitor corner in which edges never switch.
    pub dead_corner_px: u32,
}

impl Default for EdgeSettings {
    fn default() -> Self {
        EdgeSettings { switch_delay_ms: 0, double_bump: false, double_bump_window_ms: 500, dead_corner_px: 0 }
    }
}

/// Global settings (per-node settings such as name, enable and mouse speed
/// travel in `PeerInfo`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HubSettings {
    /// Combo that always returns focus to the local machine, e.g.
    /// `"Ctrl+Alt+Shift+Escape"` (names from `flow-keymap`).
    pub emergency_hotkey: String,
    pub clipboard: ClipboardSettings,
    pub edge: EdgeSettings,
    pub path_policy: PathPolicy,
    /// How often a link on the relay path probes the direct path.
    pub probe_interval_ms: u32,
    /// `tracing` filter for agents (e.g. "info", "debug", "flow_net=trace").
    pub log_level: String,
}

pub const DEFAULT_EMERGENCY_HOTKEY: &str = "Ctrl+Alt+Shift+Escape";

impl Default for HubSettings {
    fn default() -> Self {
        HubSettings {
            emergency_hotkey: DEFAULT_EMERGENCY_HOTKEY.to_string(),
            clipboard: ClipboardSettings::default(),
            edge: EdgeSettings::default(),
            path_policy: PathPolicy::PreferP2p,
            probe_interval_ms: 2000,
            log_level: "info".to_string(),
        }
    }
}

/// What an agent receives. Currently identical to the hub's global settings.
pub type AgentSettings = HubSettings;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_json_uses_defaults() {
        let s: HubSettings =
            serde_json::from_str(r#"{"probe_interval_ms": 500, "edge": {"double_bump": true}}"#).unwrap();
        assert_eq!(s.probe_interval_ms, 500);
        assert!(s.edge.double_bump);
        assert_eq!(s.edge.double_bump_window_ms, 500);
        assert_eq!(s.emergency_hotkey, DEFAULT_EMERGENCY_HOTKEY);
        let bin = crate::encode(&s);
        assert_eq!(crate::decode::<HubSettings>(&bin).unwrap(), s);
    }
}
