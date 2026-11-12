//! Hub state: persistent part (nodes, layout positions, settings, secrets)
//! stored as JSON in the data volume, and runtime part (online agents,
//! focus, per-pair path reports).

use anyhow::{Context, Result};
use flow_layout::LayoutExt;
use flow_proto::update::{Offer, UpdateFromHub, UpdatePolicy, UpdateReport};
use flow_proto::{
    AgentSettings, BuildId, Capabilities, CursorPark, Fingerprint, FocusEntry, FromHub, HubSettings, Layout, Monitor,
    NodeId, NodeLayout, OsKind, PathKind, PeerInfo, RelayDown,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

pub const STATE_FILE: &str = "state.json";

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeRecord {
    pub id: NodeId,
    pub name: String,
    pub hostname: String,
    /// Hex SHA-256 of the agent certificate (its credential).
    pub fingerprint: String,
    pub os: OsKind,
    pub enabled: bool,
    pub mouse_speed: f32,
    /// Position of the node's desktop origin in the global layout.
    pub x: i32,
    pub y: i32,
    pub monitors: Vec<Monitor>,
    #[serde(default)]
    pub caps: Capabilities,
    pub created: u64,
    pub last_seen: u64,
    /// Own allowed networks (None = inherit the global setting).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_networks: Option<Vec<String>>,
    /// Own crossing key (None = inherit; Some("") = none for this node).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crossing_key: Option<String>,
    /// Own cursor behaviour while controlling another machine (None =
    /// inherit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_park: Option<CursorPark>,
    /// Own update policy (None = inherit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_policy: Option<UpdatePolicy>,
    /// Build and platform the agent reported when it last connected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<BuildId>,
    #[serde(default)]
    pub platform: String,
}

impl NodeRecord {
    pub fn fp(&self) -> Option<Fingerprint> {
        Fingerprint::from_hex(&self.fingerprint)
    }

    /// The global settings with this node's overrides applied.
    pub fn effective_settings(&self, global: &HubSettings) -> AgentSettings {
        let mut s = global.clone();
        if let Some(n) = &self.allowed_networks {
            s.allowed_networks = n.clone();
        }
        if let Some(k) = &self.crossing_key {
            s.crossing_key = k.clone();
        }
        if let Some(c) = self.cursor_park {
            s.cursor_park = c;
        }
        if let Some(p) = self.update_policy {
            s.update_policy = p;
        }
        s
    }

    /// What this node actually does with its effective cursor setting,
    /// given what its backend reported it can do.
    pub fn applied_cursor_park(&self, global: &HubSettings) -> CursorPark {
        self.effective_settings(global).cursor_park.nearest(self.caps.cursor)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Persisted {
    pub version: u32,
    /// Hex of the 16-byte join secret.
    pub join_secret: String,
    /// Dashboard token (used when FLOW_ADMIN_TOKEN is not set).
    pub admin_token: String,
    pub next_id: u32,
    pub nodes: BTreeMap<u32, NodeRecord>,
    #[serde(default)]
    pub settings: HubSettings,
}

fn random_hex(n: usize) -> String {
    (0..n).map(|_| format!("{:02x}", rand::random::<u8>())).collect()
}

impl Persisted {
    pub fn fresh() -> Self {
        Persisted {
            version: 1,
            join_secret: random_hex(16),
            admin_token: random_hex(16),
            next_id: 1,
            nodes: BTreeMap::new(),
            settings: HubSettings::default(),
        }
    }

    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let p = dir.join(STATE_FILE);
        if p.exists() {
            let s = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
            return serde_json::from_str(&s).with_context(|| format!("parsing {}", p.display()));
        }
        let fresh = Persisted::fresh();
        fresh.save(dir)?;
        Ok(fresh)
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self)?;
        flow_net::identity::write_private(&dir.join(STATE_FILE), &json)
    }

    pub fn join_secret_bytes(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(self.join_secret.get(i * 2..i * 2 + 2).unwrap_or("00"), 16).unwrap_or(0);
        }
        out
    }

    pub fn regenerate_join_secret(&mut self) {
        self.join_secret = random_hex(16);
    }

    pub fn node_by_fp(&self, fp: &Fingerprint) -> Option<&NodeRecord> {
        let hex = fp.to_hex();
        self.nodes.values().find(|n| n.fingerprint == hex)
    }

    pub fn unique_name(&self, wanted: &str) -> String {
        let base = if wanted.trim().is_empty() { "node".to_string() } else { wanted.trim().to_string() };
        let mut name = base.clone();
        let mut i = 2;
        while self.nodes.values().any(|n| n.name == name) {
            name = format!("{base}-{i}");
            i += 1;
        }
        name
    }

    /// Layout of enabled nodes that reported monitors.
    pub fn layout(&self) -> Layout {
        Layout {
            nodes: self
                .nodes
                .values()
                .filter(|n| n.enabled && !n.monitors.is_empty())
                .map(|n| NodeLayout { id: n.id, x: n.x, y: n.y, monitors: n.monitors.clone() })
                .collect(),
        }
    }

    pub fn place_new(&self, monitors: &[Monitor]) -> (i32, i32) {
        self.layout().place_new_node(monitors)
    }

    /// Stores the monitors node `id` reported (on connect or on a change)
    /// and keeps the layout usable: other nodes follow the edges they
    /// shared with it and nothing is left overlapping it
    /// ([`flow_layout::apply_monitors`]). An unchanged report still pushes
    /// nodes out of an overlap (one saved earlier). An empty report is
    /// ignored: the node would leave the layout (monitors switched off for a
    /// moment). Returns what changed, None when nothing did.
    pub fn report_monitors(&mut self, id: NodeId, monitors: Vec<Monitor>) -> Option<Relayout> {
        let rec = self.nodes.get(&id.0)?;
        if monitors.is_empty() {
            return None;
        }
        let before = rec.monitors.clone();
        let changed = before != monitors;
        let mut layout = self.layout();
        let moved = if layout.node(id).is_some() {
            if changed {
                flow_layout::apply_monitors(&mut layout, id, monitors.clone())
            } else {
                flow_layout::separate_from(&mut layout, id)
            }
        } else {
            // Not in the layout yet (disabled, or no monitors known): only
            // keep it from landing on top of another node.
            self.nodes.get_mut(&id.0).expect("exists").monitors = monitors.clone();
            let mut l = self.layout();
            if l.node(id).is_some() {
                flow_layout::separate_from(&mut l, id)
            } else {
                Vec::new()
            }
        };
        if !changed && moved.is_empty() {
            return None;
        }
        self.nodes.get_mut(&id.0).expect("exists").monitors = monitors.clone();
        for m in &moved {
            if let Some(n) = self.nodes.get_mut(&m.id.0) {
                (n.x, n.y) = m.to;
            }
        }
        let r = Relayout { node: id, at: unix_now(), before, after: monitors, moved };
        tracing::info!(node = %id, "{}", r.describe(self));
        Some(r)
    }
}

/// What the hub did with a monitor report (shown in the dashboard).
#[derive(Clone, Debug)]
pub struct Relayout {
    pub node: NodeId,
    pub at: u64,
    pub before: Vec<Monitor>,
    pub after: Vec<Monitor>,
    pub moved: Vec<flow_layout::Moved>,
}

impl Relayout {
    /// "LAPTOP now has 1 monitor (had 3): moved SONTY-PC from 4536,0 to
    /// 1536,0 to keep it next to LAPTOP."
    pub fn describe(&self, p: &Persisted) -> String {
        let name = |id: NodeId| p.nodes.get(&id.0).map(|n| n.name.clone()).unwrap_or_else(|| id.to_string());
        let count = |m: &[Monitor]| format!("{} monitor{}", m.len(), if m.len() == 1 { "" } else { "s" });
        let mut s = if self.before == self.after {
            format!("{} reported its monitors again", name(self.node))
        } else if self.before.is_empty() {
            format!("{} reported {}", name(self.node), count(&self.after))
        } else if self.before.len() == self.after.len() {
            format!("{}'s monitors changed", name(self.node))
        } else {
            format!("{} now has {} (had {})", name(self.node), count(&self.after), self.before.len())
        };
        if self.moved.is_empty() {
            s.push_str(": no other machine had to move.");
        } else {
            let moves: Vec<String> = self
                .moved
                .iter()
                .map(|m| format!("{} from {},{} to {},{}", name(m.id), m.from.0, m.from.1, m.to.0, m.to.1))
                .collect();
            s.push_str(&format!(": moved {} to keep the layout touching without overlaps.", moves.join(", ")));
        }
        s
    }
}

/// What a connected agent said about itself (hello, update reports).
#[derive(Clone, Debug, Default)]
pub struct AgentMeta {
    pub build: BuildId,
    pub platform: String,
    pub protocol: u32,
    pub report: Option<UpdateReport>,
}

/// Channels to one connected agent.
pub struct Online {
    pub conn_id: u64,
    pub conn: flow_net::quinn::Connection,
    pub ctl: mpsc::UnboundedSender<FromHub>,
    pub relay: Option<mpsc::UnboundedSender<RelayDown>>,
    pub clip: Option<mpsc::UnboundedSender<flow_proto::ClipboardMsg>>,
    pub remote: SocketAddr,
    pub p2p_addrs: Vec<SocketAddr>,
    pub meta: AgentMeta,
    pub since: u64,
    /// Reported by the agent: not on one of its allowed networks.
    pub away: bool,
    /// The agent's own interface addresses (from its last report).
    pub local_addrs: Vec<IpAddr>,
}

/// An agent of another protocol, connected only to update itself: it is
/// not online for anything else (no peers, layout, relay, clipboard).
pub struct UpdateOnly {
    pub conn_id: u64,
    pub conn: flow_net::quinn::Connection,
    pub tx: mpsc::UnboundedSender<UpdateFromHub>,
    pub remote: SocketAddr,
    pub meta: AgentMeta,
    pub since: u64,
    pub reason: String,
}

/// An agent from before the update handshake (protocol 1–3) that tried
/// to connect: it was told to be updated by hand.
#[derive(Clone, Debug, Serialize)]
pub struct LegacyAgent {
    pub version: String,
    pub protocol: u32,
    pub at: u64,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct PairStat {
    pub path: PathKind,
    pub rtt_us: Option<u32>,
    pub p2p_rtt_us: Option<u32>,
    pub relay_rtt_us: Option<u32>,
    #[serde(skip)]
    pub updated: Option<Instant>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClipMeta {
    pub from: NodeId,
    pub kind: flow_proto::ClipKind,
    pub bytes: usize,
    pub at: u64,
}

pub struct HubState {
    pub data_dir: PathBuf,
    pub p: Persisted,
    pub online: HashMap<NodeId, Online>,
    pub focus: BTreeMap<NodeId, NodeId>,
    pub pairs: HashMap<(NodeId, NodeId), PairStat>,
    pub last_clip: Option<ClipMeta>,
    pub next_conn_id: u64,
    /// Agents connected in update-only mode.
    pub updating: HashMap<NodeId, UpdateOnly>,
    /// Agents too old to update themselves, by node.
    pub legacy: HashMap<NodeId, LegacyAgent>,
    /// The agent builds this hub carries (None: none in this image).
    pub builds: Option<std::sync::Arc<crate::updates::AgentBuilds>>,
    /// The last time a monitor report changed the layout (dashboard note).
    pub last_relayout: Option<Relayout>,
}

impl HubState {
    pub fn save(&self) {
        if let Err(e) = self.p.save(&self.data_dir) {
            tracing::error!("saving hub state: {e:#}");
        }
    }

    pub fn focus_entries(&self) -> Vec<FocusEntry> {
        self.focus.iter().map(|(s, t)| FocusEntry { source: *s, target: *t }).collect()
    }

    /// Settings for `node` (global + its overrides).
    pub fn settings_for(&self, node: NodeId) -> AgentSettings {
        match self.p.nodes.get(&node.0) {
            Some(n) => n.effective_settings(&self.p.settings),
            None => self.p.settings.clone(),
        }
    }

    /// Connected but not on an allowed network.
    pub fn is_away(&self, node: NodeId) -> bool {
        self.online.get(&node).is_some_and(|o| o.away)
    }

    /// Peer list as seen by `me`.
    pub fn peers_for(&self, me: NodeId) -> Vec<PeerInfo> {
        self.p
            .nodes
            .values()
            .filter(|n| n.id != me)
            .filter_map(|n| {
                let online = self.online.get(&n.id);
                Some(PeerInfo {
                    id: n.id,
                    name: n.name.clone(),
                    fingerprint: n.fp()?,
                    addrs: online.map(|o| o.p2p_addrs.clone()).unwrap_or_default(),
                    online: online.is_some(),
                    enabled: n.enabled,
                    mouse_speed: n.mouse_speed,
                    caps: n.caps,
                    away: online.is_some_and(|o| o.away),
                    crossing_key: n.effective_settings(&self.p.settings).crossing_key,
                })
            })
            .collect()
    }

    pub fn send_to(&self, id: NodeId, msg: FromHub) {
        if let Some(o) = self.online.get(&id) {
            let _ = o.ctl.send(msg);
        }
    }

    pub fn broadcast_peers(&self) {
        for id in self.online.keys() {
            self.send_to(*id, FromHub::Peers(self.peers_for(*id)));
        }
    }

    /// Applies a monitor report ([`Persisted::report_monitors`]); saves and
    /// tells every agent when the layout changed. `broadcast` false: the
    /// caller sends the layout itself (a node that is just connecting).
    pub fn report_monitors(&mut self, id: NodeId, monitors: Vec<Monitor>, broadcast: bool) {
        if let Some(r) = self.p.report_monitors(id, monitors) {
            self.last_relayout = Some(r);
            self.save();
            if broadcast {
                self.broadcast_layout();
            }
        }
    }

    pub fn broadcast_layout(&self) {
        let l = self.p.layout();
        for o in self.online.values() {
            let _ = o.ctl.send(FromHub::Layout(l.clone()));
        }
    }

    pub fn broadcast_focus(&self) {
        let f = self.focus_entries();
        for o in self.online.values() {
            let _ = o.ctl.send(FromHub::Focus(f.clone()));
        }
    }

    pub fn broadcast_settings(&self) {
        for (id, o) in &self.online {
            let _ = o.ctl.send(FromHub::Settings(self.settings_for(*id)));
        }
    }

    /// What `node` reported about itself, in a full or update-only session.
    pub fn agent_meta(&self, node: NodeId) -> Option<&AgentMeta> {
        self.online.get(&node).map(|o| &o.meta).or_else(|| self.updating.get(&node).map(|u| &u.meta))
    }

    pub fn agent_meta_mut(&mut self, node: NodeId) -> Option<&mut AgentMeta> {
        match self.online.get_mut(&node) {
            Some(o) => Some(&mut o.meta),
            None => self.updating.get_mut(&node).map(|u| &mut u.meta),
        }
    }

    /// The update offer for `node`: its effective policy and the build for
    /// its platform.
    pub fn offer_for(&self, node: NodeId) -> Offer {
        let policy = self.settings_for(node).update_policy;
        let platform = self.agent_meta(node).map(|m| m.platform.clone()).unwrap_or_default();
        Offer { policy, release: self.builds.as_ref().and_then(|b| b.release(&platform)) }
    }

    /// Sends an update message to `node` in whichever session it has.
    pub fn send_update(&self, node: NodeId, msg: UpdateFromHub) {
        if let Some(o) = self.online.get(&node) {
            let _ = o.ctl.send(FromHub::Update(msg));
        } else if let Some(u) = self.updating.get(&node) {
            let _ = u.tx.send(msg);
        }
    }

    /// Pushes the current offer to every connected agent (settings change).
    pub fn broadcast_offers(&self) {
        for id in self.online.keys().chain(self.updating.keys()) {
            self.send_update(*id, UpdateFromHub::Offer(self.offer_for(*id)));
        }
    }

    /// Drops focus entries that involve `id` as target, telling the
    /// sources to release control.
    pub fn release_focus_on(&mut self, id: NodeId) {
        let sources: Vec<NodeId> = self.focus.iter().filter(|(_, t)| **t == id).map(|(s, _)| *s).collect();
        for s in sources {
            self.focus.remove(&s);
            self.send_to(s, FromHub::ReleaseControl);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: u32, name: &str, x: i32, y: i32, monitors: Vec<Monitor>) -> NodeRecord {
        NodeRecord {
            id: NodeId(id),
            name: name.into(),
            hostname: name.into(),
            fingerprint: Fingerprint([id as u8; 32]).to_hex(),
            os: OsKind::Linux,
            enabled: true,
            mouse_speed: 1.0,
            x,
            y,
            monitors,
            caps: Capabilities::default(),
            created: 0,
            last_seen: 0,
            allowed_networks: None,
            crossing_key: None,
            cursor_park: None,
            update_policy: None,
            agent: None,
            platform: String::new(),
        }
    }

    fn mon(x: i32, y: i32, w: i32, h: i32) -> Monitor {
        Monitor { name: format!("{w}x{h}"), rect: flow_proto::Rect::new(x, y, w, h), primary: false }
    }

    fn docked() -> Vec<Monitor> {
        vec![mon(0, 0, 1536, 960), mon(1536, 0, 1920, 1080), mon(3456, 0, 1080, 1920)]
    }

    #[test]
    fn monitor_reports_keep_the_neighbour_touching() {
        let mut p = Persisted::fresh();
        p.nodes.insert(1, rec(1, "LAPTOP", 0, 0, vec![mon(0, 0, 1536, 960)]));
        p.nodes.insert(2, rec(2, "SONTY-PC", 1536, 0, vec![mon(0, 0, 1920, 1080)]));
        // Docked: the PC moves right of the new width.
        let r = p.report_monitors(NodeId(1), docked()).expect("layout changed");
        assert_eq!((p.nodes[&2].x, p.nodes[&2].y), (4536, 0));
        assert_eq!(p.nodes[&1].monitors.len(), 3);
        assert_eq!(
            r.describe(&p),
            "LAPTOP now has 3 monitors (had 1): moved SONTY-PC from 1536,0 to 4536,0 to keep the layout touching \
             without overlaps."
        );
        // The same report again (agent reconnects): nothing to do.
        assert!(p.report_monitors(NodeId(1), docked()).is_none());
        // Monitors switched off for a moment: ignored, the node stays.
        assert!(p.report_monitors(NodeId(1), vec![]).is_none());
        assert_eq!(p.layout().nodes.len(), 2);
        // Undocked (also when reported by the Hello after a restart).
        p.report_monitors(NodeId(1), vec![mon(0, 0, 1536, 960)]).expect("layout changed");
        assert_eq!((p.nodes[&2].x, p.nodes[&2].y), (1536, 0));
        assert!(p.report_monitors(NodeId(9), docked()).is_none(), "unknown node");
    }

    #[test]
    fn an_unchanged_report_still_ends_a_saved_overlap() {
        // The owner's hub: the PC was placed at 1536 while the laptop already
        // reported three monitors.
        let mut p = Persisted::fresh();
        p.nodes.insert(1, rec(1, "LAPTOP", 0, 0, docked()));
        p.nodes.insert(2, rec(2, "SONTY-PC", 1536, 0, vec![mon(0, 0, 1920, 1080)]));
        assert_eq!(flow_layout::overlaps(&p.layout()).len(), 1);
        let r = p.report_monitors(NodeId(1), docked()).expect("separated");
        assert!(r.describe(&p).starts_with("LAPTOP reported its monitors again: moved SONTY-PC"), "{}", r.describe(&p));
        assert!(flow_layout::overlaps(&p.layout()).is_empty());
        assert!(flow_layout::isolated(&p.layout()).is_empty());
    }

    #[test]
    fn persisted_roundtrip_and_names() {
        let dir = std::env::temp_dir().join(format!("flow-hub-state-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = Persisted::load_or_create(&dir).unwrap();
        assert_eq!(p.join_secret.len(), 32);
        let secret = p.join_secret_bytes();
        p.nodes.insert(
            1,
            NodeRecord {
                id: NodeId(1),
                name: "pc".into(),
                hostname: "pc".into(),
                fingerprint: Fingerprint([1; 32]).to_hex(),
                os: OsKind::Linux,
                enabled: true,
                mouse_speed: 1.0,
                x: 0,
                y: 0,
                monitors: vec![Monitor { name: "m".into(), rect: flow_proto::Rect::new(0, 0, 10, 10), primary: true }],
                caps: Capabilities::default(),
                created: 0,
                last_seen: 0,
                allowed_networks: None,
                crossing_key: Some("ScrollLock".into()),
                cursor_park: Some(CursorPark::CenterHide),
                update_policy: Some(UpdatePolicy::Notify),
                agent: Some(BuildId::new("0.2.0", "5.gabc")),
                platform: "linux-x86_64".into(),
            },
        );
        p.save(&dir).unwrap();
        let q = Persisted::load_or_create(&dir).unwrap();
        assert_eq!(q.join_secret_bytes(), secret);
        assert_eq!(q.unique_name("pc"), "pc-2");
        assert_eq!(q.unique_name("  "), "node");
        assert!(q.node_by_fp(&Fingerprint([1; 32])).is_some());
        assert_eq!(q.layout().nodes.len(), 1);
        assert_eq!(q.place_new(&[]), (10, 0));
        // Overrides survive the round trip and beat the global settings.
        let global = HubSettings {
            crossing_key: "Ctrl".into(),
            allowed_networks: vec!["10.0.0.0/8".into()],
            ..Default::default()
        };
        let eff = q.nodes[&1].effective_settings(&global);
        assert_eq!(eff.crossing_key, "ScrollLock");
        assert_eq!(eff.allowed_networks, global.allowed_networks, "inherited");
        assert_eq!(eff.cursor_park, CursorPark::CenterHide);
        assert_eq!(eff.update_policy, UpdatePolicy::Notify);
        assert_eq!(q.nodes[&1].agent.as_ref().unwrap().full(), "0.2.0+5.gabc");
        // The backend reported no cursor support (old agent, GNOME): the
        // closest behaviour is staying at the edge.
        assert_eq!(q.nodes[&1].applied_cursor_park(&global), CursorPark::Edge);
        let mut full = q.nodes[&1].clone();
        full.caps.cursor = flow_proto::CursorSupport::FULL;
        assert_eq!(full.applied_cursor_park(&global), CursorPark::CenterHide);
        full.cursor_park = None;
        assert_eq!(full.applied_cursor_park(&global), CursorPark::Edge, "inherits the global default");
        std::fs::remove_dir_all(dir).ok();
    }
}
