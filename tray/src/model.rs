//! What the tray shows for an agent state: icon variant, tooltip, the
//! status lines at the top of the menu. Platform independent (tested).

use crate::icon::Variant;
use flow_ipc::{SessionIdle, Snapshot};

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
    /// "Update now" is offered (the label names the version).
    pub update_now: Option<String>,
}

/// The agent needs a join token before it can do anything.
pub fn needs_join(s: &Snapshot) -> bool {
    s.revoked || (s.node_id.is_none() && !s.pending_join)
}

/// Connected only to update, and the update needs someone (policy
/// notify/off, failed or impossible).
fn update_blocked(s: &Snapshot) -> bool {
    s.update.update_only.is_some() && matches!(s.update.state.as_str(), "available" | "unsupported")
}

/// Icon variant for a snapshot (see the README for the meaning).
pub fn variant(s: &Snapshot) -> Variant {
    if needs_join(s) || s.rejected.is_some() || !s.problems.is_empty() || update_blocked(s) {
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

/// The neutral explanation for a machine that cannot be controlled right
/// now because its remote control session is closed on purpose (the agent
/// reports why in [`Snapshot::session_idle`]; this is no problem and no
/// warning, and it never changes the icon).
pub fn idle_note(s: &Snapshot) -> Option<&'static str> {
    Some(match s.session_idle? {
        SessionIdle::Away => "Other machines can control this one again when it is back on an allowed network",
        SessionIdle::Paused => "Other machines can control this one again when sharing is resumed",
        SessionIdle::Disconnected => "Other machines can control this one again once it is connected to the hub",
        SessionIdle::UpdateOnly => "Other machines can control this one again once it is updated and connected",
        SessionIdle::Other => "Other machines cannot control this one right now",
    })
}

/// "yes", or "no" with the reason when it is the intended state.
pub fn inject_label(s: &Snapshot) -> &'static str {
    match (s.can_inject, s.session_idle) {
        (true, _) => "yes",
        (false, Some(_)) => "no (closed while sharing is off)",
        (false, None) => "no",
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

/// "0.2.0+142.gabc (linux-x86_64)".
pub fn version_label(s: &Snapshot) -> String {
    if s.platform.is_empty() {
        s.agent_version.clone()
    } else {
        format!("{} ({})", s.agent_version, s.platform)
    }
}

/// The update line: what the updater is doing, or None when there is
/// nothing to say (current, or the hub sent no policy yet).
pub fn update_label(s: &Snapshot) -> Option<String> {
    let u = &s.update;
    let v = u.available.clone().unwrap_or_default();
    let line = match u.state.as_str() {
        "available" if u.failed.as_deref().is_some_and(|f| f.starts_with(&v)) => {
            format!("{v} available (the last attempt failed)")
        }
        "available" => format!("{v} available"),
        "downloading" => match u.progress {
            Some(p) => format!("downloading {v} ({p}%)"),
            None => format!("downloading {v}"),
        },
        "ready" => format!("{v} ready, installs when this machine is idle"),
        "installing" => format!("installing {v}, restarting"),
        "unsupported" => {
            format!("cannot update itself: {}", u.detail.as_deref().unwrap_or("unsupported installation"))
        }
        _ => return u.failed.as_ref().map(|f| format!("last update failed: {f}")),
    };
    Some(line)
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
                update_now: None,
            }
        }
        AgentState::Unreachable(_) => {
            return View {
                variant: Variant::Disconnected,
                headline: "FlowCursor: the agent is not running".into(),
                lines: vec!["Agent: not running".into()],
                paused: None,
                dashboard_url: None,
                update_now: None,
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
    } else if s.update.update_only.is_some() {
        match s.update.state.as_str() {
            "available" => "FlowCursor: update needed to work with this hub (Update now)".to_string(),
            "unsupported" => "FlowCursor: update needed to work with this hub (by hand)".to_string(),
            _ => "FlowCursor: updating to work with this hub…".to_string(),
        }
    } else if !s.hub_connected {
        "FlowCursor: not connected to the hub".to_string()
    } else if s.paused {
        "FlowCursor: sharing paused".to_string()
    } else if s.away {
        "FlowCursor: away (not on an allowed network)".to_string()
    } else if s.controlling.is_some() || !s.controlled_by.is_empty() || s.holding_edge {
        format!("FlowCursor: {}", focus_label(s))
    } else if s.update.can_update_now && s.update.policy == "notify" {
        format!("FlowCursor: update available ({})", s.update.available.as_deref().unwrap_or("new version"))
    } else {
        let online = s.peers.iter().filter(|p| p.online && p.enabled && !p.away).count();
        format!("FlowCursor: connected, {online} of {} other machines online", s.peers.len())
    };

    let mut lines = Vec::new();
    lines.push(if s.hub_connected {
        format!("Hub: connected ({})", hub_label(s))
    } else if s.update.update_only.is_some() {
        format!("Hub: connected only to update ({})", hub_label(s))
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
    lines.push(format!("Version: {}", s.agent_version));
    if let Some(u) = update_label(s) {
        lines.push(format!("Update: {}", shorten(&u, 80)));
    }
    let update_now = s
        .update
        .can_update_now
        .then(|| format!("Update now to {}", s.update.available.as_deref().unwrap_or("the hub's version")));
    View { variant, headline, lines, paused: Some(s.paused), dashboard_url: s.dashboard_url.clone(), update_now }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flow_ipc::{PeerRef, PeerStatus, UpdateInfo};

    fn joined() -> Snapshot {
        Snapshot {
            agent_version: "0.2.0+5.gabc".into(),
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

    /// What the agent sends in each situation (see `flow-agent`'s
    /// `controllable` module): the tray decides from these fields only.
    fn closed_for(why: SessionIdle) -> Snapshot {
        Snapshot { can_inject: false, can_clipboard: false, session_idle: Some(why), ..joined() }
    }

    #[test]
    fn a_session_closed_on_purpose_is_not_an_error() {
        // Away: yellow icon, a neutral explanation, no problem lines.
        let s = Snapshot { away: true, ..closed_for(SessionIdle::Away) };
        assert_eq!(variant(&s), Variant::Away);
        assert_eq!(
            idle_note(&s),
            Some("Other machines can control this one again when it is back on an allowed network")
        );
        let v = view(&AgentState::Running(Box::new(s.clone())));
        assert_eq!(v.headline, "FlowCursor: away (not on an allowed network)");
        assert!(!v.lines.iter().any(|l| l.starts_with("Problem")), "{:?}", v.lines);
        assert_eq!(inject_label(&s), "no (closed while sharing is off)");

        // Paused: yellow.
        let s = Snapshot { paused: true, ..closed_for(SessionIdle::Paused) };
        assert_eq!(variant(&s), Variant::Away);
        assert_eq!(idle_note(&s), Some("Other machines can control this one again when sharing is resumed"));

        // Not connected (hub down, grace over): grey.
        let s = Snapshot { hub_connected: false, ..closed_for(SessionIdle::Disconnected) };
        assert_eq!(variant(&s), Variant::Disconnected);
        assert_eq!(idle_note(&s), Some("Other machines can control this one again once it is connected to the hub"));

        // Update-only, the update is on its way: not an error either. (Waiting
        // for the user to start the update is, see `update_lines_and_actions`.)
        let mut s = Snapshot { hub_connected: false, ..closed_for(SessionIdle::UpdateOnly) };
        s.update.update_only = Some("protocol 5 vs 4".into());
        s.update.state = "installing".into();
        assert_eq!(variant(&s), Variant::Disconnected);
        assert!(idle_note(&s).unwrap().contains("updated"));
        assert_eq!(variant(&Snapshot { hub_connected: true, ..closed_for(SessionIdle::Other) }), Variant::Idle);
    }

    #[test]
    fn no_note_when_the_session_is_open_or_wanted() {
        // Open and working.
        let s =
            Snapshot { can_inject: true, can_clipboard: true, controllable: true, session_wanted: true, ..joined() };
        assert_eq!((variant(&s), idle_note(&s), inject_label(&s)), (Variant::Idle, None, "yes"));
        // Hub-drop grace period: the session is still open (or wanted), the
        // machine only shows that the hub is gone.
        let s = Snapshot { hub_connected: false, session_wanted: true, can_inject: true, ..joined() };
        assert_eq!((variant(&s), idle_note(&s)), (Variant::Disconnected, None));
        // An agent without the field (older): no note, `can_inject: false`
        // stays what it was.
        let s = Snapshot { can_inject: false, ..joined() };
        assert_eq!((idle_note(&s), inject_label(&s)), (None, "no"));
    }

    #[test]
    fn a_session_that_should_be_open_but_is_not_stays_an_error() {
        // GNOME dialog refused: the agent lists the refusal, the session is
        // wanted, nothing is closed on purpose.
        let s = Snapshot {
            can_inject: false,
            session_wanted: true,
            problems: vec!["remote control permission (RemoteDesktop portal) was refused in the GNOME dialog".into()],
            ..joined()
        };
        assert_eq!(variant(&s), Variant::Error);
        assert_eq!(idle_note(&s), None, "no neutral explanation for a fault");
        let v = view(&AgentState::Running(Box::new(s)));
        assert!(v.headline.contains("refused in the GNOME dialog"), "{}", v.headline);
        assert!(v.lines.iter().any(|l| l.starts_with("Problem: remote control permission")));

        // Portal missing (never wanted open, or away): a problem at once.
        for away in [false, true] {
            let s = Snapshot {
                can_inject: false,
                away,
                problems: vec!["This machine cannot inject input: other machines cannot control it.".into()],
                ..joined()
            };
            assert_eq!(variant(&s), Variant::Error, "away: {away}");
            assert_eq!(idle_note(&s), None);
        }
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
                "Version: 0.2.0+5.gabc",
            ]
        );
        assert_eq!(v.update_now, None);
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

    #[test]
    fn update_lines_and_actions() {
        let mut s = joined();
        s.update = UpdateInfo {
            policy: "notify".into(),
            state: "available".into(),
            available: Some("0.2.1+9.gdef".into()),
            can_update_now: true,
            ..Default::default()
        };
        let v = view(&AgentState::Running(Box::new(s.clone())));
        assert_eq!(v.headline, "FlowCursor: update available (0.2.1+9.gdef)");
        assert_eq!(v.lines.last().unwrap(), "Update: 0.2.1+9.gdef available");
        assert_eq!(v.update_now.as_deref(), Some("Update now to 0.2.1+9.gdef"));
        assert_eq!(v.variant, Variant::Idle, "an optional update is no error");

        s.update.state = "downloading".into();
        s.update.progress = Some(40);
        s.update.can_update_now = false;
        let v = view(&AgentState::Running(Box::new(s.clone())));
        assert_eq!(v.lines.last().unwrap(), "Update: downloading 0.2.1+9.gdef (40%)");
        assert_eq!(v.update_now, None);

        // Connected only to update, waiting for the user: needs attention.
        s.hub_connected = false;
        s.update.update_only = Some("protocol 5 vs 4".into());
        s.update.state = "available".into();
        s.update.can_update_now = true;
        let v = view(&AgentState::Running(Box::new(s.clone())));
        assert_eq!(v.variant, Variant::Error);
        assert!(v.headline.contains("update needed"), "{}", v.headline);
        assert!(v.lines[0].starts_with("Hub: connected only to update"), "{:?}", v.lines);

        s.update = UpdateInfo {
            state: "unsupported".into(),
            detail: Some("installed by package".into()),
            ..Default::default()
        };
        s.hub_connected = true;
        let v = view(&AgentState::Running(Box::new(s.clone())));
        assert_eq!(v.lines.last().unwrap(), "Update: cannot update itself: installed by package");
        s.update = UpdateInfo {
            state: "current".into(),
            failed: Some("0.2.1+9.gdef: did not reach the hub".into()),
            failed_rolled_back: true,
            ..Default::default()
        };
        let v = view(&AgentState::Running(Box::new(s)));
        assert_eq!(v.lines.last().unwrap(), "Update: last update failed: 0.2.1+9.gdef: did not reach the hub");
    }
}
