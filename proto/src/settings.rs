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

/// Where this machine's own cursor goes while its mouse and keyboard drive
/// another machine. When control comes back the cursor always enters at
/// the edge, at the position mapped from where it left the other machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CursorPark {
    /// Stays at the edge where it left (the behaviour before this setting).
    #[default]
    Edge,
    /// Jumps to the centre of the monitor it left from.
    Center,
    /// As `Center`, and hidden until control comes back.
    CenterHide,
}

impl CursorPark {
    pub const ALL: [CursorPark; 3] = [CursorPark::Edge, CursorPark::Center, CursorPark::CenterHide];

    pub fn as_str(self) -> &'static str {
        match self {
            CursorPark::Edge => "edge",
            CursorPark::Center => "center",
            CursorPark::CenterHide => "center_hide",
        }
    }

    pub fn moves(self) -> bool {
        self != CursorPark::Edge
    }

    pub fn hides(self) -> bool {
        self == CursorPark::CenterHide
    }

    /// The closest behaviour a backend with `support` can deliver (what a
    /// machine actually does with this setting): hiding needs both moving
    /// and hiding, otherwise centre without hiding, otherwise the edge.
    pub fn nearest(self, support: CursorSupport) -> CursorPark {
        match self {
            CursorPark::CenterHide if support.center && support.hide => CursorPark::CenterHide,
            CursorPark::CenterHide | CursorPark::Center if support.center => CursorPark::Center,
            _ => CursorPark::Edge,
        }
    }
}

/// What a backend can do with the local cursor while capturing (reported
/// by the agent in its capabilities; the dashboard shows the limits).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CursorSupport {
    /// Move the cursor while this machine drives another one.
    pub center: bool,
    /// Hide the cursor while this machine drives another one.
    pub hide: bool,
}

impl CursorSupport {
    pub const FULL: CursorSupport = CursorSupport { center: true, hide: true };
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
    /// CIDR blocks (IPv4/IPv6) this machine must be on to share input and
    /// clipboard; empty = no restriction. Decided by each agent from its
    /// own interface addresses (see [`crate::net`]).
    pub allowed_networks: Vec<String>,
    /// Key combo that must be held for the pointer to cross an edge out of
    /// this machine's screen (`flow-keymap` names, e.g. `"Ctrl"`,
    /// `"LeftCtrl+LeftAlt"`, `"ScrollLock"`); empty = cross freely.
    pub crossing_key: String,
    /// Where the local cursor goes while this machine controls another.
    pub cursor_park: CursorPark,
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
            allowed_networks: Vec::new(),
            crossing_key: String::new(),
            cursor_park: CursorPark::Edge,
        }
    }
}

/// What an agent receives: the hub's global settings with this node's own
/// overrides (`allowed_networks`, `crossing_key`, `cursor_park`) already
/// applied.
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
        // Settings stored by an older hub keep today's behaviour.
        assert!(s.allowed_networks.is_empty());
        assert!(s.crossing_key.is_empty());
        assert_eq!(s.cursor_park, CursorPark::Edge);
        let bin = crate::encode(&s);
        assert_eq!(crate::decode::<HubSettings>(&bin).unwrap(), s);
    }

    #[test]
    fn cursor_park_json_names() {
        for p in CursorPark::ALL {
            assert_eq!(serde_json::to_string(&p).unwrap(), format!("\"{}\"", p.as_str()));
        }
        let s: HubSettings = serde_json::from_str(r#"{"cursor_park": "center_hide"}"#).unwrap();
        assert_eq!(s.cursor_park, CursorPark::CenterHide);
        assert!(serde_json::from_str::<CursorPark>(r#""middle""#).is_err());
    }

    #[test]
    fn nearest_supported_behaviour() {
        use CursorPark::*;
        let none = CursorSupport::default();
        let move_only = CursorSupport { center: true, hide: false };
        let hide_only = CursorSupport { center: false, hide: true };
        for p in CursorPark::ALL {
            assert_eq!(p.nearest(CursorSupport::FULL), p, "full support keeps every choice");
            assert_eq!(p.nearest(none), Edge, "nothing possible (GNOME): stays at the edge");
            assert_eq!(p.nearest(hide_only), Edge, "hiding at the edge is not an option");
        }
        assert_eq!(CenterHide.nearest(move_only), Center);
        assert_eq!(Center.nearest(move_only), Center);
        assert!(CenterHide.hides() && CenterHide.moves() && Center.moves() && !Center.hides() && !Edge.moves());
    }
}
