//! Agent P2P listener and the receiving sink shared by P2P and relay.

use super::receiver::{Action, ReceiverState, HEARTBEAT_TIMEOUT};
use crate::frame::{read_frame, write_frame};
use crate::tls::{peer_fingerprint, ClientAuthorizer};
use flow_proto::{Fingerprint, LinkDatagram, LinkMsg, NodeId, MAX_CONTROL_FRAME};
use quinn::{Connection, Endpoint};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Fingerprints of the peers the hub says we may talk to. Used as the TLS
/// client authorizer of the P2P listener; connections of peers that are
/// removed are closed immediately.
#[derive(Debug, Default)]
pub struct PeerDirectory {
    map: RwLock<HashMap<Fingerprint, NodeId>>,
    conns: Mutex<Vec<(Fingerprint, Connection)>>,
}

impl PeerDirectory {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn set(&self, peers: impl IntoIterator<Item = (Fingerprint, NodeId)>) {
        let new: HashMap<Fingerprint, NodeId> = peers.into_iter().collect();
        {
            let mut conns = self.conns.lock().expect("poisoned");
            conns.retain(|(fp, c)| {
                let keep = new.contains_key(fp) && c.close_reason().is_none();
                if !keep {
                    c.close(1u32.into(), b"peer revoked");
                }
                keep
            });
        }
        *self.map.write().expect("poisoned") = new;
    }

    pub fn lookup(&self, fp: &Fingerprint) -> Option<NodeId> {
        self.map.read().expect("poisoned").get(fp).copied()
    }

    fn track(&self, fp: Fingerprint, c: Connection) {
        self.conns.lock().expect("poisoned").push((fp, c));
    }
}

impl ClientAuthorizer for PeerDirectory {
    fn allowed(&self, fp: &Fingerprint) -> bool {
        self.lookup(fp).is_some()
    }
}

/// Where received link traffic goes.
pub trait LinkSink: Send + Sync + 'static {
    /// Reliable message from an authenticated peer. Returns the reply (ack).
    fn on_msg(&self, from: NodeId, msg: LinkMsg) -> Option<LinkMsg>;
    /// Datagram from an authenticated peer. Returns the reply (pong).
    fn on_datagram(&self, from: NodeId, dg: LinkDatagram) -> Option<LinkDatagram>;
}

/// [`LinkSink`] that runs the [`ReceiverState`] and forwards injection
/// actions to a channel.
pub struct LinkReceiver {
    state: Mutex<ReceiverState>,
    actions: mpsc::UnboundedSender<(NodeId, Action)>,
}

impl LinkReceiver {
    /// Creates the receiver and starts its heartbeat watchdog.
    pub fn new(actions: mpsc::UnboundedSender<(NodeId, Action)>) -> Arc<Self> {
        let r = Arc::new(LinkReceiver { state: Mutex::new(ReceiverState::new()), actions });
        let weak = Arc::downgrade(&r);
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_millis(250));
            loop {
                t.tick().await;
                let Some(r) = weak.upgrade() else { return };
                let out = r.state.lock().expect("poisoned").check_timeouts(Instant::now(), HEARTBEAT_TIMEOUT);
                for (id, acts) in out {
                    warn!(source = %id, "controlling source went silent, released all its keys/buttons");
                    r.emit(id, acts);
                }
            }
        });
        r
    }

    fn emit(&self, from: NodeId, acts: Vec<Action>) {
        for a in acts {
            let _ = self.actions.send((from, a));
        }
    }

    /// Releases everything `from` holds (revoked, disabled, offline).
    pub fn release_source(&self, from: NodeId) {
        let acts = self.state.lock().expect("poisoned").release_source(from);
        self.emit(from, acts);
    }

    pub fn release_everything(&self) {
        let out = self.state.lock().expect("poisoned").release_everything();
        for (id, acts) in out {
            self.emit(id, acts);
        }
    }

    pub fn controllers(&self) -> Vec<NodeId> {
        self.state.lock().expect("poisoned").controllers()
    }
}

impl LinkSink for LinkReceiver {
    fn on_msg(&self, from: NodeId, msg: LinkMsg) -> Option<LinkMsg> {
        match msg {
            LinkMsg::Event { session, seq, base, ev } => {
                let out = self.state.lock().expect("poisoned").on_event(from, session, seq, base, ev, Instant::now());
                self.emit(from, out.actions);
                out.ack.map(|upto| LinkMsg::Ack { session, upto })
            }
            LinkMsg::Ack { .. } => None,
        }
    }

    fn on_datagram(&self, from: NodeId, dg: LinkDatagram) -> Option<LinkDatagram> {
        match dg {
            LinkDatagram::Motion(m) => {
                let a = self.state.lock().expect("poisoned").on_motion(from, &m, Instant::now());
                tracing::trace!(source = %from, mseq = m.mseq, after_seq = m.after_seq, applied = a.is_some(), "motion");
                if let Some(a) = a {
                    let _ = self.actions.send((from, a));
                }
                None
            }
            LinkDatagram::Ping { session, nonce } => {
                self.state.lock().expect("poisoned").on_ping(from, session, Instant::now());
                Some(LinkDatagram::Pong { nonce })
            }
            LinkDatagram::Pong { .. } => None,
        }
    }
}

/// Accepts P2P connections from known peers until the endpoint closes.
pub async fn serve_p2p(ep: Endpoint, dir: Arc<PeerDirectory>, sink: Arc<dyn LinkSink>) {
    while let Some(incoming) = ep.accept().await {
        let dir = dir.clone();
        let sink = sink.clone();
        tokio::spawn(async move {
            let remote = incoming.remote_address();
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    debug!(%remote, "P2P handshake failed: {e}");
                    return;
                }
            };
            let Some(fp) = peer_fingerprint(&conn) else { return };
            let Some(from) = dir.lookup(&fp) else {
                conn.close(1u32.into(), b"unknown peer");
                return;
            };
            dir.track(fp, conn.clone());
            info!(peer = %from, %remote, "accepted direct connection");
            handle_conn(conn, from, sink).await;
        });
    }
}

async fn handle_conn(conn: Connection, from: NodeId, sink: Arc<dyn LinkSink>) {
    let c2 = conn.clone();
    let s2 = sink.clone();
    tokio::spawn(async move {
        while let Ok(b) = c2.read_datagram().await {
            match flow_proto::decode::<LinkDatagram>(&b) {
                Ok(dg) => {
                    if let Some(reply) = s2.on_datagram(from, dg) {
                        let _ = c2.send_datagram(flow_proto::encode(&reply).into());
                    }
                }
                Err(e) => debug!(peer = %from, "bad datagram: {e}"),
            }
        }
    });
    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
        let sink = sink.clone();
        tokio::spawn(async move {
            loop {
                match read_frame::<_, LinkMsg>(&mut recv, MAX_CONTROL_FRAME).await {
                    Ok(Some(msg)) => {
                        if let Some(reply) = sink.on_msg(from, msg) {
                            if write_frame(&mut send, &reply).await.is_err() {
                                return;
                            }
                        }
                    }
                    Ok(None) => return,
                    Err(crate::frame::FrameError::Io(e)) => {
                        debug!(peer = %from, "link stream ended: {e}");
                        return;
                    }
                    Err(e) => {
                        warn!(peer = %from, "closing link stream after bad frame: {e}");
                        let _ = recv.stop(2u32.into());
                        return;
                    }
                }
            }
        });
    }
    debug!(peer = %from, "direct connection closed: {:?}", conn.close_reason());
}
