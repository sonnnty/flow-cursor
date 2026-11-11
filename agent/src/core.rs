//! Agent core: owns the focus state machine of this machine as an input
//! source, the links to other machines, the receiver for machines that
//! control this one, and clipboard sync.
//!
//! Every machine is an input source: when the local pointer crosses an edge
//! that the layout connects to another machine, this agent captures its
//! local devices and drives that machine over a link (P2P or relayed). The
//! hub is told about focus changes but is not needed for the switch itself
//! (cached layout and peers keep working while the hub is down, P2P only).

use crate::backend::{monitor_at, Backend, EventRx, LocalEvent};
use crate::config::{AgentConfig, AgentState};
use crate::edge::EdgeGate;
use crate::hub::{HubChannels, HubClient, HubEvent, HubRelay, Links};
use anyhow::{Context, Result};
use flow_keymap::{parse_combo, Combo};
use flow_layout::{LayoutExt, Move};
use flow_net::endpoint::{bind, server_config, TransportOpts};
use flow_net::link::p2p::LinkReceiver;
use flow_net::link::{serve_p2p, spawn_link, Action, LinkConfig, LinkStatus, PeerDirectory, PeerTarget};
use flow_net::quinn::Endpoint;
use flow_net::Identity;
use flow_proto::{
    AgentSettings, Button, ClipContent, Fingerprint, InputEvent, Layout, NodeId, PathKind, PathReport, PeerInfo, ToHub,
    ALPN_P2P,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

/// Applies a new log filter (installed by the binary).
pub type LogHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Observable agent state (tests, logs).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentStatus {
    pub node_id: Option<NodeId>,
    pub connected: bool,
    /// Machine this one's devices currently drive.
    pub controlling: Option<NodeId>,
    /// Machines currently driving this one.
    pub controlled_by: Vec<NodeId>,
    pub revoked: bool,
    pub rejected: Option<String>,
    pub settings: Option<AgentSettings>,
    pub layout_nodes: usize,
    pub online_peers: Vec<NodeId>,
}

pub struct AgentHandle {
    pub p2p_addr: SocketAddr,
    pub fingerprint: Fingerprint,
    status: watch::Receiver<AgentStatus>,
    links: Links,
    endpoint: Endpoint,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl AgentHandle {
    pub fn status(&self) -> AgentStatus {
        self.status.borrow().clone()
    }

    pub fn watch(&self) -> watch::Receiver<AgentStatus> {
        self.status.clone()
    }

    /// Waits until `pred` holds for the status (true) or timeout (false).
    pub async fn wait(&self, timeout: Duration, pred: impl Fn(&AgentStatus) -> bool) -> bool {
        let mut w = self.status.clone();
        tokio::time::timeout(timeout, w.wait_for(|s| pred(s))).await.map(|r| r.is_ok()).unwrap_or(false)
    }

    pub fn link_status(&self, peer: NodeId) -> Option<LinkStatus> {
        self.links.lock().expect("poisoned").get(&peer).map(|l| l.status())
    }

    pub async fn wait_link_path(&self, peer: NodeId, path: PathKind, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let link = self.links.lock().expect("poisoned").get(&peer).cloned();
            if let Some(l) = link {
                let left = deadline.saturating_duration_since(Instant::now());
                return l.wait_path(path, left).await;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub async fn shutdown(self) {
        for t in &self.tasks {
            t.abort();
        }
        for l in self.links.lock().expect("poisoned").values() {
            l.shutdown();
        }
        self.endpoint.close(0u32.into(), b"agent stopped");
    }
}

/// Starts the agent with an already created backend.
pub async fn start(
    cfg: AgentConfig,
    backend: Arc<dyn Backend>,
    events: EventRx,
    log_hook: Option<LogHook>,
) -> Result<AgentHandle> {
    let state_dir = cfg.state_dir();
    std::fs::create_dir_all(&state_dir).with_context(|| format!("creating {}", state_dir.display()))?;
    let host = hostname::get().ok().and_then(|h| h.into_string().ok()).unwrap_or_else(|| "agent".into());
    let identity = Identity::load_or_create(&state_dir, "agent", &host)?;
    let state = Arc::new(Mutex::new(AgentState::load(&state_dir)?));

    let dir = PeerDirectory::new();
    let sc = server_config(&identity, ALPN_P2P, dir.clone(), TransportOpts::P2P)?;
    let port = cfg.p2p_port.unwrap_or(flow_proto::DEFAULT_P2P_PORT);
    let endpoint = bind(SocketAddr::from(([0, 0, 0, 0], port)), Some(sc))?;
    let p2p_addr = endpoint.local_addr()?;
    info!(%p2p_addr, fingerprint = %identity.fingerprint, backend = backend.name(), "agent P2P listener up");

    let (act_tx, act_rx) = mpsc::unbounded_channel();
    let receiver = LinkReceiver::new(act_tx);
    let links: Links = Arc::default();
    let relay = Arc::new(HubRelay::default());
    let (hub_ev_tx, hub_ev_rx) = mpsc::unbounded_channel();
    let (to_hub_tx, to_hub_rx) = mpsc::unbounded_channel();
    let (clip_tx, clip_rx) = mpsc::unbounded_channel();
    let (status_tx, status) = watch::channel(AgentStatus {
        node_id: state.lock().expect("poisoned").node_id.map(NodeId),
        ..Default::default()
    });

    let mut tasks = Vec::new();
    tasks.push(tokio::spawn(serve_p2p(endpoint.clone(), dir.clone(), receiver.clone())));

    let client = HubClient {
        endpoint: endpoint.clone(),
        identity: identity.clone(),
        cfg: cfg.clone(),
        state_dir: state_dir.clone(),
        state: state.clone(),
        relay: relay.clone(),
        receiver: receiver.clone(),
        links: links.clone(),
        backend: backend.clone(),
        events: hub_ev_tx,
        p2p_port: p2p_addr.port(),
    };
    tasks.push(tokio::spawn(client.run(HubChannels { to_hub: to_hub_rx, clip: clip_rx })));

    let settings = AgentSettings::default();
    let mut core = Core {
        me: None,
        backend,
        layout: Layout::default(),
        peers: HashMap::new(),
        combo: parse_combo(&settings.emergency_hotkey).expect("default hotkey parses"),
        gate: EdgeGate::new(settings.edge.clone()),
        settings,
        cfg,
        mode: Mode::Local,
        sent_keys: BTreeSet::new(),
        sent_buttons: BTreeSet::new(),
        held_local: HashSet::new(),
        links: links.clone(),
        dir,
        receiver,
        relay,
        endpoint: endpoint.clone(),
        identity: identity.clone(),
        to_hub: to_hub_tx,
        clip_out: clip_tx,
        last_clip_hash: None,
        clip_set_at: None,
        status: status_tx,
        log_hook,
        down_since: None,
        controllers: BTreeSet::new(),
    };
    core.apply_settings(AgentSettings::default());
    tasks.push(tokio::spawn(core.run(events, hub_ev_rx, act_rx)));

    Ok(AgentHandle { p2p_addr, fingerprint: identity.fingerprint, status, links, endpoint, tasks })
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Local,
    Remote { target: NodeId, x: f64, y: f64 },
}

struct Core {
    me: Option<NodeId>,
    backend: Arc<dyn Backend>,
    layout: Layout,
    peers: HashMap<NodeId, PeerInfo>,
    settings: AgentSettings,
    cfg: AgentConfig,
    combo: Combo,
    gate: EdgeGate,
    mode: Mode,
    /// Keys/buttons forwarded as pressed to the current target.
    sent_keys: BTreeSet<u32>,
    sent_buttons: BTreeSet<Button>,
    /// Keys physically held while capturing (for the emergency hotkey).
    held_local: HashSet<u32>,
    links: Links,
    dir: Arc<PeerDirectory>,
    receiver: Arc<LinkReceiver>,
    relay: Arc<HubRelay>,
    endpoint: Endpoint,
    identity: Identity,
    to_hub: mpsc::UnboundedSender<ToHub>,
    clip_out: mpsc::UnboundedSender<ClipContent>,
    last_clip_hash: Option<u64>,
    /// When and what kind of content was last set from a remote copy.
    clip_set_at: Option<(Instant, flow_proto::ClipKind)>,
    status: watch::Sender<AgentStatus>,
    log_hook: Option<LogHook>,
    down_since: Option<Instant>,
    controllers: BTreeSet<NodeId>,
}

impl Core {
    async fn run(
        mut self,
        mut events: EventRx,
        mut hub: mpsc::UnboundedReceiver<HubEvent>,
        mut actions: mpsc::UnboundedReceiver<(NodeId, Action)>,
    ) {
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut report = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                Some(ev) = events.recv() => self.on_local(ev),
                Some(ev) = hub.recv() => self.on_hub(ev),
                Some((from, a)) = actions.recv() => self.on_action(from, a),
                _ = tick.tick() => self.on_tick(),
                _ = report.tick() => self.report_paths(),
                else => break,
            }
        }
    }

    fn publish(&self) {
        let controlling = match self.mode {
            Mode::Remote { target, .. } => Some(target),
            Mode::Local => None,
        };
        let me = self.me;
        let online: Vec<NodeId> = {
            let mut v: Vec<NodeId> = self.peers.values().filter(|p| p.online && p.enabled).map(|p| p.id).collect();
            v.sort();
            v
        };
        self.status.send_modify(|s| {
            s.node_id = me.or(s.node_id);
            s.controlling = controlling;
            s.controlled_by = self.controllers.iter().copied().collect();
            s.settings = Some(self.settings.clone());
            s.layout_nodes = self.layout.nodes.len();
            s.online_peers = online;
        });
    }

    // ------------------------------------------------------------------
    // hub events
    // ------------------------------------------------------------------

    fn on_hub(&mut self, ev: HubEvent) {
        match ev {
            HubEvent::Connected(w) => {
                self.me = Some(w.node_id);
                self.status.send_modify(|s| {
                    s.connected = true;
                    s.revoked = false;
                    s.rejected = None;
                    s.node_id = Some(w.node_id);
                });
                self.apply_settings(w.settings);
                self.layout = w.layout;
                self.set_peers(w.peers);
                if let Mode::Remote { target, .. } = self.mode {
                    let _ = self.to_hub.send(ToHub::FocusChanged { target: Some(target) });
                }
                self.report_paths();
            }
            HubEvent::Disconnected => {
                self.status.send_modify(|s| s.connected = false);
            }
            HubEvent::Layout(l) => {
                self.layout = l;
                self.refresh_edges();
                if let Mode::Remote { target, .. } = self.mode {
                    if self.layout.node(target).is_none() {
                        info!(%target, "target left the layout, returning control");
                        self.leave_remote(None);
                    }
                }
            }
            HubEvent::Peers(p) => self.set_peers(p),
            HubEvent::Settings(s) => {
                info!("settings updated by hub");
                self.apply_settings(s);
            }
            HubEvent::Focus(f) => debug!(?f, "focus table"),
            HubEvent::ReleaseControl => {
                info!("hub asked to release control");
                self.leave_remote(None);
            }
            HubEvent::Revoked => {
                self.leave_remote(None);
                self.me = None;
                self.set_peers(Vec::new());
                self.receiver.release_everything();
                self.status.send_modify(|s| {
                    s.revoked = true;
                    s.node_id = None;
                });
            }
            HubEvent::Rejected(r) => self.status.send_modify(|s| s.rejected = Some(r)),
            HubEvent::Clipboard { from, content } => {
                if !content.allowed_by(&self.settings.clipboard) {
                    debug!(?from, ?content, "clipboard from hub ignored (settings)");
                    return;
                }
                debug!(?from, ?content, "clipboard set from remote");
                self.last_clip_hash = Some(content.hash64());
                self.clip_set_at = Some((Instant::now(), content.kind()));
                self.backend.set_clipboard(&content);
            }
        }
        self.publish();
    }

    fn apply_settings(&mut self, s: AgentSettings) {
        let hotkey =
            self.cfg.emergency_hotkey.clone().filter(|h| !h.trim().is_empty()).unwrap_or(s.emergency_hotkey.clone());
        self.combo = match parse_combo(&hotkey) {
            Ok(c) => c,
            Err(e) => {
                warn!("invalid emergency hotkey {hotkey:?} ({e}), using the default");
                parse_combo(flow_proto::settings::DEFAULT_EMERGENCY_HOTKEY).expect("default parses")
            }
        };
        self.gate.cfg = s.edge.clone();
        let lc = LinkConfig::from_settings(&s);
        for l in self.links.lock().expect("poisoned").values() {
            l.set_config(lc.clone());
        }
        if self.cfg.log_level.is_none() && s.log_level != self.settings.log_level {
            if let Some(h) = &self.log_hook {
                h(&s.log_level);
            }
        }
        self.settings = s;
    }

    fn set_peers(&mut self, list: Vec<PeerInfo>) {
        let me = self.me;
        let old: HashSet<NodeId> = self.peers.keys().copied().collect();
        self.peers = list.into_iter().filter(|p| Some(p.id) != me).map(|p| (p.id, p)).collect();
        // Who may connect to us directly.
        self.dir.set(self.peers.values().filter(|p| p.enabled).map(|p| (p.fingerprint, p.id)));
        // Release anything held by sources that disappeared or were disabled.
        for id in old {
            if !self.peers.get(&id).is_some_and(|p| p.enabled) {
                self.receiver.release_source(id);
            }
        }
        // Links to every online, enabled peer (so paths and RTT are known
        // before the pointer ever crosses).
        let lc = LinkConfig::from_settings(&self.settings);
        let mut links = self.links.lock().expect("poisoned");
        links.retain(|id, l| {
            let keep = self.peers.get(id).is_some_and(|p| p.online && p.enabled);
            if !keep {
                l.shutdown();
            }
            keep
        });
        for p in self.peers.values().filter(|p| p.online && p.enabled) {
            let target = PeerTarget { id: p.id, fingerprint: p.fingerprint, addrs: p.addrs.clone() };
            match links.get(&p.id) {
                Some(l) => l.set_peer(target),
                None => {
                    debug!(peer = %p.id, addrs = ?p.addrs, "creating link");
                    let l = spawn_link(
                        target,
                        self.endpoint.clone(),
                        self.identity.clone(),
                        self.relay.clone(),
                        lc.clone(),
                    );
                    links.insert(p.id, l);
                }
            }
        }
        drop(links);
        if let Mode::Remote { target, .. } = self.mode {
            if !self.peers.get(&target).is_some_and(|p| p.online && p.enabled) {
                info!(%target, "target went offline or was disabled, returning control");
                self.leave_remote(None);
            }
        }
        self.refresh_edges();
    }

    fn reachable(&self, id: NodeId) -> bool {
        self.peers.get(&id).is_some_and(|p| p.online && p.enabled && p.caps.inject)
    }

    fn refresh_edges(&self) {
        let Some(me) = self.me else {
            self.backend.set_edges(Vec::new());
            return;
        };
        let edges: Vec<_> = self.layout.edges_of(me).into_iter().filter(|e| self.reachable(e.to)).collect();
        self.backend.set_edges(edges);
    }

    // ------------------------------------------------------------------
    // local events
    // ------------------------------------------------------------------

    fn on_local(&mut self, ev: LocalEvent) {
        match ev {
            LocalEvent::EdgeHit { side, x, y } => self.on_edge(side, x, y),
            LocalEvent::Motion { dx, dy } => self.on_motion(dx, dy),
            LocalEvent::Key { usage, down } => {
                if !matches!(self.mode, Mode::Remote { .. }) {
                    return;
                }
                if down {
                    self.held_local.insert(usage);
                    if self.combo.is_held(|k| self.held_local.contains(&k)) {
                        warn!("emergency hotkey pressed, returning control to this machine");
                        self.leave_remote(None);
                        return;
                    }
                } else {
                    self.held_local.remove(&usage);
                }
                if down {
                    self.sent_keys.insert(usage);
                } else if !self.sent_keys.remove(&usage) {
                    return; // never forwarded as pressed (e.g. pressed before switching)
                }
                self.send(InputEvent::Key { usage, down });
            }
            LocalEvent::Button { button, down } => {
                if !matches!(self.mode, Mode::Remote { .. }) {
                    return;
                }
                if down {
                    self.sent_buttons.insert(button);
                } else if !self.sent_buttons.remove(&button) {
                    return;
                }
                self.send(InputEvent::Button { button, down });
            }
            LocalEvent::Wheel { dx, dy } => {
                if matches!(self.mode, Mode::Remote { .. }) {
                    self.send(InputEvent::Wheel { dx, dy });
                }
            }
            LocalEvent::CaptureEnded => {
                if matches!(self.mode, Mode::Remote { .. }) {
                    warn!("local capture ended by the system, returning control");
                    self.leave_remote(None);
                }
            }
            LocalEvent::Clipboard(c) => self.on_local_clipboard(c),
            LocalEvent::MonitorsChanged(m) => {
                info!(monitors = m.len(), "monitor configuration changed");
                let _ = self.to_hub.send(ToHub::UpdateMonitors(m));
            }
        }
        self.publish();
    }

    fn on_local_clipboard(&mut self, c: ClipContent) {
        let h = c.hash64();
        if Some(h) == self.last_clip_hash {
            return;
        }
        // Backends suppress notifications for content they set themselves;
        // as a second line of defence ignore a change of the same kind right
        // after a remote update (re-encoded images would otherwise ping-pong).
        if self.clip_set_at.is_some_and(|(t, k)| k == c.kind() && t.elapsed() < Duration::from_millis(250)) {
            self.last_clip_hash = Some(h);
            return;
        }
        if !c.allowed_by(&self.settings.clipboard) {
            debug!(?c, "local clipboard change not synced (settings)");
            return;
        }
        self.last_clip_hash = Some(h);
        let _ = self.clip_out.send(c);
    }

    fn on_edge(&mut self, side: flow_layout::Side, x: i32, y: i32) {
        if matches!(self.mode, Mode::Remote { .. }) {
            return;
        }
        let deny = |b: &Arc<dyn Backend>| b.end_capture(None);
        let Some(me) = self.me else { return deny(&self.backend) };
        let Some((to, ex, ey)) = self.layout.crossing_local(me, x as f64, y as f64, side) else {
            return deny(&self.backend);
        };
        if !self.reachable(to) {
            return deny(&self.backend);
        }
        let usable = self.links.lock().expect("poisoned").get(&to).is_some_and(|l| l.status().path != PathKind::Down);
        if !usable {
            debug!(%to, "edge crossing refused: no path to target");
            return deny(&self.backend);
        }
        let mon = monitor_at(&self.backend.monitors(), x, y);
        if !self.gate.hit(Instant::now(), side, x, y, mon) {
            return deny(&self.backend);
        }
        self.gate.reset();
        self.backend.begin_capture();
        self.enter(to, ex, ey);
    }

    fn link(&self, id: NodeId) -> Option<flow_net::link::LinkHandle> {
        self.links.lock().expect("poisoned").get(&id).cloned()
    }

    fn send(&self, ev: InputEvent) {
        if let Mode::Remote { target, .. } = self.mode {
            if let Some(l) = self.link(target) {
                l.send_event(ev);
            }
        }
    }

    fn enter(&mut self, to: NodeId, x: f64, y: f64) {
        let Some(link) = self.link(to) else { return };
        link.set_active(true);
        link.send_event(InputEvent::Enter { x: x.round() as i32, y: y.round() as i32 });
        self.mode = Mode::Remote { target: to, x, y };
        self.down_since = None;
        let _ = self.to_hub.send(ToHub::FocusChanged { target: Some(to) });
        let st = link.status();
        info!(target = %to, path = %st.path, rtt_us = ?st.rtt_us, "focus → remote machine");
    }

    /// Releases everything forwarded to the current target and leaves it.
    fn release_target(&mut self) {
        if let Mode::Remote { target, .. } = self.mode {
            if let Some(l) = self.link(target) {
                for usage in std::mem::take(&mut self.sent_keys) {
                    l.send_event(InputEvent::Key { usage, down: false });
                }
                for button in std::mem::take(&mut self.sent_buttons) {
                    l.send_event(InputEvent::Button { button, down: false });
                }
                l.send_event(InputEvent::Leave);
                l.set_active(false);
            }
        }
        self.sent_keys.clear();
        self.sent_buttons.clear();
    }

    fn leave_remote(&mut self, warp: Option<(i32, i32)>) {
        if !matches!(self.mode, Mode::Remote { .. }) {
            return;
        }
        self.release_target();
        self.mode = Mode::Local;
        self.held_local.clear();
        self.backend.end_capture(warp);
        let _ = self.to_hub.send(ToHub::FocusChanged { target: None });
        info!("focus → this machine");
        self.publish();
    }

    fn on_motion(&mut self, dx: f64, dy: f64) {
        let Mode::Remote { target, x, y } = self.mode else { return };
        let speed = self.peers.get(&target).map(|p| p.mouse_speed as f64).unwrap_or(1.0);
        let (sdx, sdy) = (dx * speed, dy * speed);
        match self.layout.move_cursor(target, x, y, sdx, sdy) {
            Move::Inside { x: nx, y: ny } => {
                self.mode = Mode::Remote { target, x: nx, y: ny };
                if let Some(l) = self.link(target) {
                    l.send_motion(nx.round() as i32, ny.round() as i32, sdx.round() as i32, sdy.round() as i32);
                }
            }
            Move::Cross { to, x: nx, y: ny, .. } => {
                if Some(to) == self.me {
                    self.leave_remote(Some((nx.round() as i32, ny.round() as i32)));
                } else if self.reachable(to) && self.link(to).is_some_and(|l| l.status().path != PathKind::Down) {
                    self.release_target();
                    self.enter(to, nx, ny);
                } else {
                    // Unreachable machine: stay at the edge.
                    if let Some(l) = self.link(target) {
                        l.send_motion(x.round() as i32, y.round() as i32, 0, 0);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // actions from controlling machines
    // ------------------------------------------------------------------

    fn on_action(&mut self, from: NodeId, a: Action) {
        match a {
            Action::Entered { .. } => {
                self.controllers.insert(from);
                info!(source = %from, "remote machine took control of this one");
            }
            Action::Left => {
                self.controllers.remove(&from);
                info!(source = %from, "remote machine released this one");
            }
            _ => {}
        }
        self.backend.inject(&a);
        if matches!(a, Action::Entered { .. } | Action::Left) {
            self.publish();
        }
    }

    fn on_tick(&mut self) {
        if let Mode::Remote { target, .. } = self.mode {
            let path = self.link(target).map(|l| l.status().path).unwrap_or(PathKind::Down);
            if path == PathKind::Down {
                let since = *self.down_since.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_millis(1500) {
                    warn!(%target, "no path to the controlled machine, returning control");
                    self.leave_remote(None);
                }
            } else {
                self.down_since = None;
            }
        }
    }

    fn report_paths(&self) {
        let reports: Vec<PathReport> = self
            .links
            .lock()
            .expect("poisoned")
            .iter()
            .map(|(id, l)| {
                let s = l.status();
                PathReport {
                    peer: *id,
                    path: s.path,
                    rtt_us: s.rtt_us,
                    p2p_rtt_us: s.p2p_rtt_us,
                    relay_rtt_us: s.relay_rtt_us,
                }
            })
            .collect();
        if !reports.is_empty() {
            for r in &reports {
                debug!(peer = %r.peer, path = %r.path, rtt_us = ?r.rtt_us, "link status");
            }
            let _ = self.to_hub.send(ToHub::PathReports(reports));
        }
    }
}
