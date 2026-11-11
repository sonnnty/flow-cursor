//! What the tray shows for an agent state: icon variant, tooltip, the
//! status lines at the top of the menu. Platform independent (tested).

use crate::icon::Variant;
use flow_ipc::Snapshot;

/// What the tray knows about the agent.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum AgentState {
    /// Nothing heard yet (startup).
    #[default]
    Unknown,
    /// Nothing answers on the IPC endpoint (agent stopped / crashed).
    Unreachable(String),
    Running(Box<Snapshot>),
}

impl AgentState {
    pub fn snapshot(&self) -> Option<&Snapshot> {
        match self {
            AgentState::Running(s) => Some(s),
            _ => None,
        }
    }
}

/// The tray's rendering of an [`AgentState`].
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub variant: Variant,
    /// One line: what matters most right now.
    pub headline: String,
    /// Status lines for the menu / tooltip, most important first.
    pub lines: Vec<String>,
    /// Sharing paused (None: agent not reachable).
    pub paused: Option<bool>,
    pub dashboard_url: Option<String>,
}

/// The agent needs a join token before it can do anything.
pub fn needs_join(s: &Snapshot) -> bool {
    s.revoked || (s.node_id.is_none() && !s.pending_join)
}

/// Icon variant for a snapshot (see the README for the meaning).
pub fn variant(s: &Snapshot) -> Variant {
    if needs_join(s) || s.rejected.is_some() || !s.problems.is_empty() {
        Variant::Error
    } else if !s.hub_connected {
        Variant::Disconnected
    } else if s.paused || s.away {
        Variant::Away
    } else if s.controlling.is_some() || !s.controlled_by.is_empty() {
        Variant::Active
    } else {
        Variant::Idle
    }
}

fn shorten(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// "laptop" / "node 3" for this machine.
pub fn node_label(s: &Snapshot) -> String {
    match (&s.node_name, s.node_id) {
        (Some(n), _) if !n.is_empty() => n.clone(),
        (_, Some(id)) => format!("node {id}"),
        _ => "(not joined)".into(),
    }
}

/// Where the hub is (configured address, last address that worked, or mDNS).
pub fn hub_label(s: &Snapshot) -> String {
    s.hub_address
        .clone()
        .or_else(|| s.hub_last_address.clone().map(|a| format!("{a} (found with mDNS)")))
        .unwrap_or_else(|| "discovering with mDNS".into())
}

pub fn sharing_label(s: &Snapshot) -> &'static str {
    match (s.paused, s.away) {
        (true, _) => "paused",
        (false, true) => "away (not on an allowed network)",
        _ => "on",
    }
}

pub fn focus_label(s: &Snapshot) -> String {
    if let Some(c) = &s.controlling {
        format!("controlling {}", c.name)
    } else if !s.controlled_by.is_empty() {
        let names: Vec<&str> = s.controlled_by.iter().map(|p| p.name.as_str()).collect();
        format!("controlled by {}", names.join(", "))
    } else if s.holding_edge {
        "at an edge, waiting for the crossing key".into()
    } else {
        "this machine".into()
    }
}

/// "pc: p2p 0.8 ms" / "pc: offline".
pub fn peer_label(p: &flow_ipc::PeerStatus) -> String {
    let state = if !p.enabled {
        "disabled".to_string()
    } else if !p.online {
        "offline".to_string()
    } else if p.away {
        "away".to_string()
    } else {
        let path = p.path.as_deref().unwrap_or("connecting");
        match p.rtt_ms {
            Some(r) => format!("{path} {r:.1} ms"),
            None => path.to_string(),
        }
    };
    format!("{}: {state}", if p.name.is_empty() { format!("node {}", p.id) } else { p.name.clone() })
}

pub fn view(state: &AgentState) -> View {
    let s = match state {
        AgentState::Unknown => {
            return View {
                variant: Variant::Disconnected,
                headline: "FlowCursor: connecting to the agent…".into(),
                lines: vec![],
                paused: None,
                dashboard_url: None,
            }
        }
        AgentState::Unreachable(_) => {
            return View {
                variant: Variant::Disconnected,
                headline: "FlowCursor: the agent is not running".into(),
                lines: vec!["Agent: not running".into()],
                paused: None,
                dashboard_url: None,
            }
        }
        AgentState::Running(s) => s,
    };
    let variant = variant(s);
    let headline = if needs_join(s) {
        if s.revoked {
            "FlowCursor: removed from the hub, join again".to_string()
        } else {
            "FlowCursor: not joined to a hub yet".to_string()
        }
    } else if let Some(r) = &s.rejected {
        format!("FlowCursor: the hub refused this machine ({})", shorten(r, 60))
    } else if !s.problems.is_empty() {
        format!("FlowCursor: {}", shorten(&s.problems[0], 80))
    } else if !s.hub_connected {
        "FlowCursor: not connected to the hub".to_string()
    } else if s.paused {
        "FlowCursor: sharing paused".to_string()
    } else if s.away {
        "FlowCursor: away (not on an allowed network)".to_string()
    } else if s.controlling.is_some() || !s.controlled_by.is_empty() || s.holding_edge {
        format!("FlowCursor: {}", focus_label(s))
    } else {
        let online = s.peers.iter().filter(|p| p.online && p.enabled && !p.away).count();
        format!("FlowCursor: connected, {online} of {} other machines online", s.peers.len())
    };

    let mut lines = Vec::new();
    lines.push(if s.hub_connected {
        format!("Hub: connected ({})", hub_label(s))
    } else {
        let why = s.hub_error.as_deref().map(|e| format!(": {}", shorten(e, 60))).unwrap_or_default();
        format!("Hub: not connected{why}")
    });
    lines.push(format!("This machine: {}", node_label(s)));
    lines.push(format!("Sharing: {}", sharing_label(s)));
    lines.push(format!("Focus: {}", focus_label(s)));
    for p in &s.peers {
        lines.push(peer_label(p));
    }
    for p in &s.problems {
        lines.push(format!("Problem: {}", shorten(p, 80)));
    }
    View { variant, headline, lines, paused: Some(s.paused), dashboard_url: s.dashboard_url.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flow_ipc::{PeerRef, PeerStatus};

    fn joined() -> Snapshot {
        Snapshot {
            node_id: Some(2),
            node_name: Some("laptop".into()),
            hub_connected: true,
            hub_address: Some("192.168.1.10".into()),
            peers: vec![PeerStatus {
                id: 1,
                name: "pc".into(),
                online: true,
                enabled: true,
                path: Some("p2p".into()),
                rtt_ms: Some(0.84),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn variants_follow_the_state() {
        let mut s = joined();
        assert_eq!(variant(&s), Variant::Idle);
        s.controlling = Some(PeerRef { id: 1, name: "pc".into() });
        assert_eq!(variant(&s), Variant::Active);
        s.controlling = None;
        s.controlled_by = vec![PeerRef { id: 1, name: "pc".into() }];
        assert_eq!(variant(&s), Variant::Active);
        s.away = true;
        assert_eq!(variant(&s), Variant::Away, "away beats active");
        s.away = false;
        s.paused = true;
        assert_eq!(variant(&s), Variant::Away);
        s.hub_connected = false;
        assert_eq!(variant(&s), Variant::Disconnected);
        s.problems = vec!["input capture permission was refused".into()];
        assert_eq!(variant(&s), Variant::Error);

        let fresh = Snapshot::default();
        assert!(needs_join(&fresh));
        assert_eq!(variant(&fresh), Variant::Error);
        let pending = Snapshot { pending_join: true, ..Default::default() };
        assert!(!needs_join(&pending));
        assert_eq!(variant(&pending), Variant::Disconnected);
        let revoked = Snapshot { revoked: true, ..joined() };
        assert_eq!(variant(&revoked), Variant::Error);
    }

    #[test]
    fn menu_lines() {
        let v = view(&AgentState::Running(Box::new(joined())));
        assert_eq!(v.headline, "FlowCursor: connected, 1 of 1 other machines online");
        assert_eq!(
            v.lines,
            vec![
                "Hub: connected (192.168.1.10)",
                "This machine: laptop",
                "Sharing: on",
                "Focus: this machine",
                "pc: p2p 0.8 ms",
            ]
        );
        assert_eq!(v.paused, Some(false));
        let mut s = joined();
        s.hub_connected = false;
        s.hub_error = Some("connection refused".into());
        s.peers[0].online = false;
        let v = view(&AgentState::Running(Box::new(s)));
        assert_eq!(v.lines[0], "Hub: not connected: connection refused");
        assert_eq!(v.lines[4], "pc: offline");
        let v = view(&AgentState::Unreachable("no socket".into()));
        assert_eq!(v.variant, Variant::Disconnected);
        assert_eq!(v.paused, None);
    }
}
