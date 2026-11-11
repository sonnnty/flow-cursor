//! Hub state: persistent part (nodes, layout positions, settings, secrets)
//! stored as JSON in the data volume, and runtime part (online agents,
//! focus, per-pair path reports).

use anyhow::{Context, Result};
use flow_layout::LayoutExt;
use flow_proto::{
    Capabilities, Fingerprint, FocusEntry, FromHub, HubSettings, Layout, Monitor, NodeId, NodeLayout, OsKind, PathKind,
    PeerInfo, RelayDown,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
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
}

impl NodeRecord {
    pub fn fp(&self) -> Option<Fingerprint> {
        Fingerprint::from_hex(&self.fingerprint)
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
    pub agent_version: String,
    pub since: u64,
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
        for o in self.online.values() {
            let _ = o.ctl.send(FromHub::Settings(self.p.settings.clone()));
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
        std::fs::remove_dir_all(dir).ok();
    }
}
