//! Link failover without a hub: the relay is an in-process fake that hands
//! messages straight to the receiver, the direct path goes through a UDP
//! proxy that can be blocked.

use flow_net::endpoint::{bind, server_config, TransportOpts};
use flow_net::link::p2p::{LinkReceiver, LinkSink};
use flow_net::link::{
    serve_p2p, spawn_link, Action, LinkConfig, LinkHandle, PeerDirectory, PeerTarget, RelayTransport,
};
use flow_net::testutil::UdpProxy;
use flow_net::Identity;
use flow_proto::{InputEvent, LinkDatagram, LinkMsg, NodeId, PathKind, PathPolicy, ALPN_P2P};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;

const A: NodeId = NodeId(1);
const B: NodeId = NodeId(2);

struct FakeRelay {
    sink: Arc<LinkReceiver>,
    back: OnceLock<LinkHandle>,
    up: AtomicBool,
    sent: AtomicUsize,
}

impl RelayTransport for FakeRelay {
    fn send(&self, _to: NodeId, msg: LinkMsg) -> bool {
        if !self.up.load(Ordering::SeqCst) {
            return false;
        }
        self.sent.fetch_add(1, Ordering::SeqCst);
        if let Some(reply) = self.sink.on_msg(A, msg) {
            if let Some(h) = self.back.get() {
                h.deliver(reply);
            }
        }
        true
    }
    fn send_datagram(&self, _to: NodeId, dg: LinkDatagram) -> bool {
        if !self.up.load(Ordering::SeqCst) {
            return false;
        }
        if let Some(reply) = self.sink.on_datagram(A, dg) {
            if let Some(h) = self.back.get() {
                h.deliver_datagram(reply);
            }
        }
        true
    }
    fn is_up(&self) -> bool {
        self.up.load(Ordering::SeqCst)
    }
}

fn key(usage: u32, down: bool) -> InputEvent {
    InputEvent::Key { usage, down }
}

async fn drain(rx: &mut mpsc::UnboundedReceiver<(NodeId, Action)>) -> Vec<Action> {
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut v = Vec::new();
    while let Ok((from, a)) = rx.try_recv() {
        assert_eq!(from, A);
        v.push(a);
    }
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p2p_falls_back_to_relay_and_recovers_without_loss_or_duplicates() {
    let id_a = Identity::generate("a").unwrap();
    let id_b = Identity::generate("b").unwrap();

    // Node B: P2P listener accepting only A.
    let dir = PeerDirectory::new();
    dir.set([(id_a.fingerprint, A)]);
    let sc = server_config(&id_b, ALPN_P2P, dir.clone(), TransportOpts::P2P).unwrap();
    let ep_b = bind("127.0.0.1:0".parse().unwrap(), Some(sc)).unwrap();
    let (act_tx, mut act_rx) = mpsc::unbounded_channel();
    let receiver = LinkReceiver::new(act_tx);
    tokio::spawn(serve_p2p(ep_b.clone(), dir, receiver.clone()));
    let proxy = UdpProxy::start(ep_b.local_addr().unwrap()).await.unwrap();

    // Node A: link to B through the proxy.
    let relay = Arc::new(FakeRelay {
        sink: receiver.clone(),
        back: OnceLock::new(),
        up: AtomicBool::new(true),
        sent: AtomicUsize::new(0),
    });
    let ep_a = bind("127.0.0.1:0".parse().unwrap(), None).unwrap();
    let cfg = LinkConfig { probe_interval: Duration::from_millis(300), ..Default::default() };
    let link = spawn_link(
        PeerTarget { id: B, fingerprint: id_b.fingerprint, addrs: vec![proxy.addr] },
        ep_a,
        id_a.clone(),
        relay.clone(),
        cfg,
    );
    relay.back.set(link.clone()).ok();
    link.set_active(true);

    assert!(link.wait_path(PathKind::P2p, Duration::from_secs(5)).await, "P2P is used by default");
    link.send_event(InputEvent::Enter { x: 10, y: 20 });
    link.send_event(key(0x0007_0004, true));
    assert!(link.wait_acked(Duration::from_secs(2)).await);
    let relayed_before = relay.sent.load(Ordering::SeqCst);
    assert_eq!(
        drain(&mut act_rx).await,
        vec![Action::Entered { x: 10, y: 20 }, Action::Key { usage: 0x0007_0004, down: true }]
    );
    assert_eq!(relayed_before, 0, "nothing relayed while P2P works");

    // Block the direct path, keep typing.
    proxy.set_blocked(true);
    link.send_event(key(0x0007_0004, false));
    link.send_event(key(0x0007_0005, true));
    assert!(link.wait_path(PathKind::Relay, Duration::from_secs(3)).await, "falls back to relay");
    assert!(link.wait_acked(Duration::from_secs(2)).await);
    link.send_event(key(0x0007_0005, false));
    assert!(link.wait_acked(Duration::from_secs(2)).await);
    assert!(relay.sent.load(Ordering::SeqCst) > 0);

    // Unblock: back to P2P.
    proxy.set_blocked(false);
    assert!(link.wait_path(PathKind::P2p, Duration::from_secs(10)).await, "returns to P2P");
    link.send_event(key(0x0007_0006, true));
    link.send_event(key(0x0007_0006, false));
    link.send_event(InputEvent::Leave);
    assert!(link.wait_acked(Duration::from_secs(2)).await);

    let acts = drain(&mut act_rx).await;
    assert_eq!(
        acts,
        vec![
            Action::Key { usage: 0x0007_0004, down: false },
            Action::Key { usage: 0x0007_0005, down: true },
            Action::Key { usage: 0x0007_0005, down: false },
            Action::Key { usage: 0x0007_0006, down: true },
            Action::Key { usage: 0x0007_0006, down: false },
            Action::Left,
        ],
        "every event exactly once, in order, nothing stuck"
    );
    let st = link.status();
    assert!(st.p2p_rtt_us.is_some() && st.relay_rtt_us.is_some(), "{st:?}");
    link.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forced_relay_and_unknown_peer() {
    let id_a = Identity::generate("a").unwrap();
    let id_b = Identity::generate("b").unwrap();
    let dir = PeerDirectory::new();
    // B does not know A at all: direct connections are refused.
    let sc = server_config(&id_b, ALPN_P2P, dir.clone(), TransportOpts::P2P).unwrap();
    let ep_b = bind("127.0.0.1:0".parse().unwrap(), Some(sc)).unwrap();
    let (act_tx, mut act_rx) = mpsc::unbounded_channel();
    let receiver = LinkReceiver::new(act_tx);
    tokio::spawn(serve_p2p(ep_b.clone(), dir, receiver.clone()));

    let relay = Arc::new(FakeRelay {
        sink: receiver.clone(),
        back: OnceLock::new(),
        up: AtomicBool::new(true),
        sent: AtomicUsize::new(0),
    });
    let ep_a = bind("127.0.0.1:0".parse().unwrap(), None).unwrap();
    let cfg = LinkConfig { probe_interval: Duration::from_millis(200), ..Default::default() };
    let link = spawn_link(
        PeerTarget { id: B, fingerprint: id_b.fingerprint, addrs: vec![ep_b.local_addr().unwrap()] },
        ep_a,
        id_a,
        relay.clone(),
        cfg.clone(),
    );
    relay.back.set(link.clone()).ok();
    assert!(link.wait_path(PathKind::Relay, Duration::from_secs(2)).await);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(link.status().path, PathKind::Relay, "unauthenticated direct path must never be used");
    link.send_event(InputEvent::Enter { x: 1, y: 1 });
    assert!(link.wait_acked(Duration::from_secs(2)).await);
    assert_eq!(drain(&mut act_rx).await, vec![Action::Entered { x: 1, y: 1 }]);

    link.set_config(LinkConfig { policy: PathPolicy::ForceRelay, ..cfg });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(link.status().path, PathKind::Relay);
    relay.up.store(false, Ordering::SeqCst);
    assert!(link.wait_path(PathKind::Down, Duration::from_secs(2)).await);
}
