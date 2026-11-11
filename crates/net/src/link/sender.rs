//! Sending side of an input link: path selection (P2P ↔ relay), sequence
//! numbers, retransmission on path switches, probing and RTT measurement.

use super::{LinkConfig, PeerTarget, RelayTransport};
use crate::endpoint::{client_config, TransportOpts, SNI};
use crate::frame::{read_frame, write_raw};
use crate::identity::Identity;
use crate::tls::ServerPin;
use flow_proto::{
    Button, InputEvent, LinkDatagram, LinkMsg, Motion, NodeId, PathKind, PathPolicy, ALPN_P2P, MAX_CONTROL_FRAME,
};
use quinn::{Connection, Endpoint};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

/// Snapshot of a link for logs, the agent and the dashboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkStatus {
    pub path: PathKind,
    /// Smoothed RTT of the active path (µs).
    pub rtt_us: Option<u32>,
    pub p2p_rtt_us: Option<u32>,
    pub relay_rtt_us: Option<u32>,
    pub unacked: usize,
    pub session: u64,
}

enum Msg {
    Event(InputEvent),
    Motion { x: i32, y: i32, dx: i32, dy: i32 },
    Active(bool),
    Peer(PeerTarget),
    Config(LinkConfig),
    Back(LinkMsg),
    BackDg(LinkDatagram),
    Shutdown,
    Dialed { gen: u64, res: Result<(Connection, quinn::SendStream, quinn::RecvStream), String> },
    P2pBack(LinkMsg),
    P2pDg(LinkDatagram),
    P2pLost { gen: u64, why: String },
}

/// Cheap, cloneable handle to a link actor.
#[derive(Clone)]
pub struct LinkHandle {
    peer: NodeId,
    tx: mpsc::UnboundedSender<Msg>,
    status: watch::Receiver<LinkStatus>,
}

impl LinkHandle {
    pub fn peer(&self) -> NodeId {
        self.peer
    }
    pub fn send_event(&self, ev: InputEvent) {
        let _ = self.tx.send(Msg::Event(ev));
    }
    /// Pointer motion; (`x`,`y`) absolute in the target's desktop.
    pub fn send_motion(&self, x: i32, y: i32, dx: i32, dy: i32) {
        let _ = self.tx.send(Msg::Motion { x, y, dx, dy });
    }
    /// Active links (currently driving the peer) ping faster.
    pub fn set_active(&self, active: bool) {
        let _ = self.tx.send(Msg::Active(active));
    }
    pub fn set_peer(&self, p: PeerTarget) {
        let _ = self.tx.send(Msg::Peer(p));
    }
    pub fn set_config(&self, c: LinkConfig) {
        let _ = self.tx.send(Msg::Config(c));
    }
    /// Ack received through the relay.
    pub fn deliver(&self, msg: LinkMsg) {
        let _ = self.tx.send(Msg::Back(msg));
    }
    /// Pong received through the relay.
    pub fn deliver_datagram(&self, dg: LinkDatagram) {
        let _ = self.tx.send(Msg::BackDg(dg));
    }
    pub fn status(&self) -> LinkStatus {
        *self.status.borrow()
    }
    pub fn watch(&self) -> watch::Receiver<LinkStatus> {
        self.status.clone()
    }
    /// Waits until the link uses `path` (true) or the timeout expires.
    pub async fn wait_path(&self, path: PathKind, timeout: Duration) -> bool {
        let mut w = self.status.clone();
        tokio::time::timeout(timeout, w.wait_for(|s| s.path == path)).await.map(|r| r.is_ok()).unwrap_or(false)
    }
    /// Waits until every event was acknowledged.
    pub async fn wait_acked(&self, timeout: Duration) -> bool {
        let mut w = self.status.clone();
        tokio::time::timeout(timeout, w.wait_for(|s| s.unacked == 0)).await.map(|r| r.is_ok()).unwrap_or(false)
    }
    pub fn shutdown(&self) {
        let _ = self.tx.send(Msg::Shutdown);
    }
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// Starts the link actor for `peer`.
pub fn spawn_link(
    peer: PeerTarget,
    endpoint: Endpoint,
    identity: Identity,
    relay: Arc<dyn RelayTransport>,
    cfg: LinkConfig,
) -> LinkHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    let session = new_session();
    let (status_tx, status) = watch::channel(LinkStatus {
        path: PathKind::Down,
        rtt_us: None,
        p2p_rtt_us: None,
        relay_rtt_us: None,
        unacked: 0,
        session,
    });
    let id = peer.id;
    let actor = Actor {
        peer,
        endpoint,
        identity,
        relay,
        cfg,
        self_tx: tx.clone(),
        status_tx,
        session,
        next_seq: 1,
        unacked: VecDeque::new(),
        mseq: 0,
        held_keys: BTreeSet::new(),
        held_buttons: BTreeSet::new(),
        entered: false,
        last_pos: (0, 0),
        path: PathKind::Down,
        p2p: None,
        gen: 0,
        dialing: false,
        pings: HashMap::new(),
        next_nonce: 1,
        p2p_rtt: Rtt::default(),
        relay_rtt: Rtt::default(),
        last_p2p_pong: None,
        p2p_streak: 0,
        probe_nonce: None,
        active: false,
        last_ping: Instant::now(),
        last_probe: Instant::now() - Duration::from_secs(3600),
        last_relay_ping: Instant::now() - Duration::from_secs(3600),
    };
    tokio::spawn(actor.run(rx));
    LinkHandle { peer: id, tx, status }
}

fn new_session() -> u64 {
    loop {
        let s: u64 = rand::random();
        if s != 0 {
            return s;
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Rtt {
    srtt: Option<f64>,
}

impl Rtt {
    fn sample(&mut self, d: Duration) {
        let us = d.as_secs_f64() * 1e6;
        self.srtt = Some(match self.srtt {
            None => us,
            Some(s) => s * 0.8 + us * 0.2,
        });
    }
    fn us(&self) -> Option<u32> {
        self.srtt.map(|v| v.round() as u32)
    }
}

struct Pending {
    seq: u64,
    ev: InputEvent,
    sent: Instant,
    path: PathKind,
}

struct P2p {
    gen: u64,
    conn: Connection,
    out: mpsc::UnboundedSender<Vec<u8>>,
    established: Instant,
}

struct Actor {
    peer: PeerTarget,
    endpoint: Endpoint,
    identity: Identity,
    relay: Arc<dyn RelayTransport>,
    cfg: LinkConfig,
    self_tx: mpsc::UnboundedSender<Msg>,
    status_tx: watch::Sender<LinkStatus>,

    session: u64,
    next_seq: u64,
    unacked: VecDeque<Pending>,
    mseq: u64,
    held_keys: BTreeSet<u32>,
    held_buttons: BTreeSet<Button>,
    entered: bool,
    last_pos: (i32, i32),

    path: PathKind,
    p2p: Option<P2p>,
    gen: u64,
    dialing: bool,
    pings: HashMap<u64, (PathKind, Instant)>,
    next_nonce: u64,
    p2p_rtt: Rtt,
    relay_rtt: Rtt,
    last_p2p_pong: Option<Instant>,
    p2p_streak: u32,
    probe_nonce: Option<u64>,
    active: bool,
    last_ping: Instant,
    last_probe: Instant,
    last_relay_ping: Instant,
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        let mut tick = tokio::time::interval(Duration::from_millis(25));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        self.start_dial();
        loop {
            tokio::select! {
                m = rx.recv() => match m {
                    None | Some(Msg::Shutdown) => break,
                    Some(m) => self.handle(m),
                },
                _ = tick.tick() => self.on_tick(),
            }
        }
        if let Some(p) = self.p2p.take() {
            p.conn.close(0u32.into(), b"link closed");
        }
        debug!(peer = %self.peer.id, "link actor stopped");
    }

    fn handle(&mut self, m: Msg) {
        let now = Instant::now();
        match m {
            Msg::Event(ev) => self.send_event(ev, now),
            Msg::Motion { x, y, dx, dy } => self.send_motion(x, y, dx, dy),
            Msg::Active(a) => self.active = a,
            Msg::Peer(p) => {
                if p.fingerprint != self.peer.fingerprint {
                    if let Some(old) = self.p2p.take() {
                        old.conn.close(0u32.into(), b"peer identity changed");
                    }
                }
                let changed = p.addrs != self.peer.addrs;
                self.peer = p;
                if changed && self.p2p.is_none() {
                    self.start_dial();
                }
            }
            Msg::Config(c) => {
                self.cfg = c;
                if self.cfg.policy == PathPolicy::ForceRelay {
                    if let Some(old) = self.p2p.take() {
                        old.conn.close(0u32.into(), b"relay forced");
                    }
                } else if self.p2p.is_none() {
                    self.start_dial();
                }
                self.evaluate(now, "settings changed");
            }
            Msg::Back(msg) | Msg::P2pBack(msg) => {
                if let LinkMsg::Ack { session, upto } = msg {
                    if session == self.session {
                        while self.unacked.front().is_some_and(|p| p.seq <= upto) {
                            self.unacked.pop_front();
                        }
                        self.publish();
                    }
                }
            }
            Msg::BackDg(dg) | Msg::P2pDg(dg) => {
                if let LinkDatagram::Pong { nonce } = dg {
                    self.on_pong(nonce, now);
                }
            }
            Msg::Dialed { gen, res } => {
                self.dialing = false;
                match res {
                    Ok((conn, send, recv)) if self.cfg.policy != PathPolicy::ForceRelay => {
                        info!(peer = %self.peer.id, addr = %conn.remote_address(), "direct connection established");
                        self.install_p2p(gen, conn, send, recv, now);
                    }
                    Ok((conn, ..)) => conn.close(0u32.into(), b"relay forced"),
                    Err(e) => debug!(peer = %self.peer.id, "direct dial failed: {e}"),
                }
            }
            Msg::P2pLost { gen, why } => {
                if self.p2p.as_ref().is_some_and(|p| p.gen == gen) {
                    info!(peer = %self.peer.id, "direct connection lost: {why}");
                    self.p2p = None;
                    self.p2p_streak = 0;
                    self.evaluate(now, "direct connection lost");
                }
            }
            Msg::Shutdown => {}
        }
    }

    fn track_state(&mut self, ev: &InputEvent) {
        match ev {
            InputEvent::Enter { x, y } => {
                self.entered = true;
                self.last_pos = (*x, *y);
            }
            InputEvent::Leave => {
                self.entered = false;
                self.held_keys.clear();
                self.held_buttons.clear();
            }
            InputEvent::Key { usage, down: true } => {
                self.held_keys.insert(*usage);
            }
            InputEvent::Key { usage, down: false } => {
                self.held_keys.remove(usage);
            }
            InputEvent::Button { button, down: true } => {
                self.held_buttons.insert(*button);
            }
            InputEvent::Button { button, down: false } => {
                self.held_buttons.remove(button);
            }
            _ => {}
        }
    }

    fn send_event(&mut self, ev: InputEvent, now: Instant) {
        self.track_state(&ev);
        if self.unacked.len() >= self.cfg.max_unacked {
            self.reset_session(now);
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.unacked.push_back(Pending { seq, ev, sent: now, path: self.path });
        let p = self.unacked.back().expect("just pushed");
        let msg = LinkMsg::Event {
            session: self.session,
            seq,
            base: self.unacked.front().map(|p| p.seq).unwrap_or(seq),
            ev: p.ev.clone(),
        };
        self.transmit(msg);
        self.publish();
    }

    /// Too many unacknowledged events (peer unreachable for long): start a
    /// new session. The receiver releases everything of the old one.
    fn reset_session(&mut self, _now: Instant) {
        warn!(peer = %self.peer.id, unacked = self.unacked.len(), "link backlog overflow, resetting session");
        self.session = new_session();
        self.unacked.clear();
        self.next_seq = 1;
        self.mseq = 0;
        if self.entered {
            let (x, y) = self.last_pos;
            let held = (self.held_keys.clone(), self.held_buttons.clone());
            self.queue_raw(InputEvent::Enter { x, y });
            self.queue_raw(InputEvent::StateSync {
                keys: held.0.into_iter().collect(),
                buttons: held.1.into_iter().collect(),
            });
        }
    }

    fn queue_raw(&mut self, ev: InputEvent) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.unacked.push_back(Pending { seq, ev, sent: Instant::now(), path: self.path });
    }

    fn send_motion(&mut self, x: i32, y: i32, dx: i32, dy: i32) {
        self.last_pos = (x, y);
        self.mseq += 1;
        let m = Motion { session: self.session, mseq: self.mseq, after_seq: self.next_seq - 1, x, y, dx, dy };
        self.transmit_dg(self.path, LinkDatagram::Motion(m));
    }

    fn transmit(&mut self, msg: LinkMsg) {
        match self.path {
            PathKind::P2p => {
                if let Some(p) = &self.p2p {
                    let _ = p.out.send(flow_proto::encode(&msg));
                }
            }
            PathKind::Relay => {
                self.relay.send(self.peer.id, msg);
            }
            PathKind::Down => {}
        }
    }

    fn transmit_dg(&self, path: PathKind, dg: LinkDatagram) {
        match path {
            PathKind::P2p => {
                if let Some(p) = &self.p2p {
                    let _ = p.conn.send_datagram(flow_proto::encode(&dg).into());
                }
            }
            PathKind::Relay => {
                self.relay.send_datagram(self.peer.id, dg);
            }
            PathKind::Down => {}
        }
    }

    fn ping(&mut self, path: PathKind, now: Instant) -> u64 {
        let nonce = self.next_nonce;
        self.next_nonce += 1;
        self.pings.insert(nonce, (path, now));
        self.transmit_dg(path, LinkDatagram::Ping { session: self.session, nonce });
        nonce
    }

    fn on_pong(&mut self, nonce: u64, now: Instant) {
        let Some((path, sent)) = self.pings.remove(&nonce) else { return };
        let rtt = now.duration_since(sent);
        match path {
            PathKind::P2p => {
                self.p2p_rtt.sample(rtt);
                self.last_p2p_pong = Some(now);
                if self.path != PathKind::P2p {
                    self.p2p_streak += 1;
                    debug!(peer = %self.peer.id, streak = self.p2p_streak, rtt_us = rtt.as_micros() as u64, "direct path probe ok");
                }
            }
            PathKind::Relay => self.relay_rtt.sample(rtt),
            PathKind::Down => {}
        }
        self.evaluate(now, "direct path recovered");
        self.publish();
    }

    fn start_dial(&mut self) {
        if self.dialing || self.peer.addrs.is_empty() || self.cfg.policy == PathPolicy::ForceRelay {
            return;
        }
        let cc = match client_config(
            &self.identity,
            ALPN_P2P,
            ServerPin::Exact(self.peer.fingerprint),
            TransportOpts::P2P,
        ) {
            Ok(c) => c,
            Err(e) => {
                warn!("cannot build P2P client config: {e:#}");
                return;
            }
        };
        self.dialing = true;
        self.gen += 1;
        let gen = self.gen;
        let addrs = self.peer.addrs.clone();
        let ep = self.endpoint.clone();
        let tx = self.self_tx.clone();
        let timeout = self.cfg.dial_timeout;
        tokio::spawn(async move {
            let mut last_err = String::from("no address");
            for addr in addrs {
                let attempt = async {
                    let conn = ep
                        .connect_with(cc.clone(), addr, SNI)
                        .map_err(|e| e.to_string())?
                        .await
                        .map_err(|e| e.to_string())?;
                    let (send, recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
                    Ok::<_, String>((conn, send, recv))
                };
                match tokio::time::timeout(timeout, attempt).await {
                    Ok(Ok(r)) => {
                        let _ = tx.send(Msg::Dialed { gen, res: Ok(r) });
                        return;
                    }
                    Ok(Err(e)) => last_err = format!("{addr}: {e}"),
                    Err(_) => last_err = format!("{addr}: timeout"),
                }
            }
            let _ = tx.send(Msg::Dialed { gen, res: Err(last_err) });
        });
    }

    fn install_p2p(
        &mut self,
        gen: u64,
        conn: Connection,
        mut send: quinn::SendStream,
        mut recv: quinn::RecvStream,
        now: Instant,
    ) {
        if let Some(old) = self.p2p.take() {
            old.conn.close(0u32.into(), b"replaced");
        }
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let tx = self.self_tx.clone();
        tokio::spawn(async move {
            while let Some(buf) = out_rx.recv().await {
                if let Err(e) = write_raw(&mut send, &buf).await {
                    let _ = tx.send(Msg::P2pLost { gen, why: format!("write: {e}") });
                    return;
                }
            }
            let _ = send.finish();
        });
        let tx = self.self_tx.clone();
        tokio::spawn(async move {
            loop {
                match read_frame::<_, LinkMsg>(&mut recv, MAX_CONTROL_FRAME).await {
                    Ok(Some(msg)) => {
                        let _ = tx.send(Msg::P2pBack(msg));
                    }
                    Ok(None) => {
                        let _ = tx.send(Msg::P2pLost { gen, why: "stream closed".into() });
                        return;
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::P2pLost { gen, why: format!("read: {e}") });
                        return;
                    }
                }
            }
        });
        let tx = self.self_tx.clone();
        let c2 = conn.clone();
        tokio::spawn(async move {
            loop {
                match c2.read_datagram().await {
                    Ok(b) => {
                        if let Ok(dg) = flow_proto::decode::<LinkDatagram>(&b) {
                            let _ = tx.send(Msg::P2pDg(dg));
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::P2pLost { gen, why: e.to_string() });
                        return;
                    }
                }
            }
        });
        self.p2p = Some(P2p { gen, conn, out: out_tx, established: now });
        // A completed handshake counts as one successful probe; one pong more
        // confirms the path.
        self.p2p_streak = self.cfg.recover_after.saturating_sub(1);
        self.ping(PathKind::P2p, now);
        self.evaluate(now, "direct connection established");
    }

    fn ping_interval(&self) -> Duration {
        if self.active {
            self.cfg.ping_active
        } else {
            self.cfg.ping_idle
        }
    }

    fn p2p_open(&self) -> bool {
        self.p2p.as_ref().is_some_and(|p| p.conn.close_reason().is_none())
    }

    /// Health of the direct path while it is the active one.
    fn p2p_healthy(&self, now: Instant) -> Result<(), &'static str> {
        let Some(p) = &self.p2p else { return Err("no direct connection") };
        if p.conn.close_reason().is_some() {
            return Err("direct connection closed");
        }
        if self.unacked.iter().any(|e| e.path == PathKind::P2p && now.duration_since(e.sent) > self.cfg.fail_timeout) {
            return Err("events not acknowledged in time");
        }
        let last = self.last_p2p_pong.unwrap_or(p.established).max(p.established);
        let dead_after = self.cfg.fail_timeout.max(self.ping_interval() * 3);
        if now.duration_since(last) > dead_after {
            return Err("no pong on direct path");
        }
        Ok(())
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        self.pings.retain(|_, (_, t)| now.duration_since(*t) < Duration::from_secs(5));
        if self.p2p.as_ref().is_some_and(|p| p.conn.close_reason().is_some()) {
            let p = self.p2p.take().expect("checked");
            debug!(peer = %self.peer.id, "direct connection closed: {:?}", p.conn.close_reason());
            self.p2p_streak = 0;
        }
        if now.duration_since(self.last_ping) >= self.ping_interval() && self.path != PathKind::Down {
            self.last_ping = now;
            self.ping(self.path, now);
        }
        if self.cfg.policy == PathPolicy::PreferP2p
            && self.path != PathKind::P2p
            && now.duration_since(self.last_probe) >= self.cfg.probe_interval
        {
            self.last_probe = now;
            if self.p2p_open() {
                if let Some(n) = self.probe_nonce {
                    if self.pings.contains_key(&n) {
                        // Previous probe unanswered.
                        self.p2p_streak = 0;
                    }
                }
                self.probe_nonce = Some(self.ping(PathKind::P2p, now));
            } else {
                self.start_dial();
            }
        }
        if self.path == PathKind::P2p
            && self.relay.is_up()
            && now.duration_since(self.last_relay_ping) >= self.cfg.probe_interval
        {
            self.last_relay_ping = now;
            self.ping(PathKind::Relay, now);
        }
        self.evaluate(now, "");
        self.publish();
    }

    fn evaluate(&mut self, now: Instant, why: &str) {
        let relay_up = self.relay.is_up();
        let (desired, reason) = if self.cfg.policy == PathPolicy::ForceRelay {
            (if relay_up { PathKind::Relay } else { PathKind::Down }, "relay forced by settings")
        } else if self.path == PathKind::P2p {
            match self.p2p_healthy(now) {
                Ok(()) => (PathKind::P2p, ""),
                Err(e) if relay_up => (PathKind::Relay, e),
                Err(e) => (PathKind::Down, e),
            }
        } else if self.p2p_open() && self.p2p_streak >= self.cfg.recover_after {
            (PathKind::P2p, if why.is_empty() { "direct path recovered" } else { why })
        } else if relay_up {
            (PathKind::Relay, "direct path unavailable")
        } else {
            (PathKind::Down, "no path")
        };
        if desired != self.path {
            self.switch(desired, reason, now);
        }
    }

    fn switch(&mut self, to: PathKind, reason: &str, now: Instant) {
        let from = self.path;
        self.path = to;
        let rtt = match to {
            PathKind::P2p => self.p2p_rtt.us(),
            PathKind::Relay => self.relay_rtt.us(),
            PathKind::Down => None,
        };
        info!(peer = %self.peer.id, %from, %to, rtt_us = ?rtt, unacked = self.unacked.len(), "link path switch: {reason}");
        if to == PathKind::P2p {
            self.last_p2p_pong = Some(now);
        } else {
            self.p2p_streak = 0;
            self.probe_nonce = None;
            // Probe soon.
            self.last_probe = now - self.cfg.probe_interval / 2;
        }
        if to != PathKind::Down {
            // Retransmit everything not yet acknowledged; the receiver drops
            // duplicates by sequence number.
            let base = self.unacked.front().map(|p| p.seq);
            let msgs: Vec<LinkMsg> = self
                .unacked
                .iter_mut()
                .map(|p| {
                    p.sent = now;
                    p.path = to;
                    LinkMsg::Event { session: self.session, seq: p.seq, base: base.unwrap_or(p.seq), ev: p.ev.clone() }
                })
                .collect();
            for m in msgs {
                self.transmit(m);
            }
            if self.entered {
                let ev = InputEvent::StateSync {
                    keys: self.held_keys.iter().copied().collect(),
                    buttons: self.held_buttons.iter().copied().collect(),
                };
                self.send_event(ev, now);
            }
            self.ping(to, now);
        }
        self.publish();
    }

    fn publish(&self) {
        let rtt_us = match self.path {
            PathKind::P2p => self.p2p_rtt.us(),
            PathKind::Relay => self.relay_rtt.us(),
            PathKind::Down => None,
        };
        let st = LinkStatus {
            path: self.path,
            rtt_us,
            p2p_rtt_us: self.p2p_rtt.us(),
            relay_rtt_us: self.relay_rtt.us(),
            unacked: self.unacked.len(),
            session: self.session,
        };
        self.status_tx.send_if_modified(|old| {
            if *old != st {
                *old = st;
                true
            } else {
                false
            }
        });
    }
}
