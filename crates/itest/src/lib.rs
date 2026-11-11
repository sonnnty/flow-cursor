//! Test harness: in-process hub and simulated agents.

use flow_agent::backend::sim::{SimBackend, SimControl};
use flow_agent::config::AgentConfig;
use flow_agent::netcheck::StaticAddrs;
use flow_agent::{AgentHandle, StartOptions};
use flow_net::endpoint::{client_config, TransportOpts, SNI};
use flow_net::frame::{read_frame, write_frame};
use flow_net::link::Action;
use flow_net::quinn;
use flow_net::tls::ServerPin;
use flow_net::Identity;
use flow_proto::{
    Capabilities, FromHub, Hello, Monitor, NodeId, OsKind, Rect, RelayDatagramDown, RelayDown, RelayUp, StreamKind,
    ToHub, ALPN_HUB, MAX_CONTROL_FRAME, PROTOCOL_VERSION,
};
use flow_server::{HubConfig, HubHandle};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

pub const ADMIN: &str = "test-admin-token";

pub fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .with_test_writer()
        .try_init();
}

pub fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("flow-itest-{tag}-{}", rand::random::<u64>()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub async fn start_hub() -> HubHandle {
    start_hub_in(temp_dir("hub")).await
}

pub async fn start_hub_in(data_dir: PathBuf) -> HubHandle {
    flow_server::start(HubConfig {
        data_dir,
        quic_addr: "127.0.0.1:0".parse().unwrap(),
        http_addr: Some("127.0.0.1:0".parse().unwrap()),
        admin_token: Some(ADMIN.into()),
        mdns: false,
        name: "itest".into(),
    })
    .await
    .expect("hub starts")
}

pub fn monitor(w: i32, h: i32) -> Vec<Monitor> {
    vec![Monitor { name: format!("SIM-{w}x{h}"), rect: Rect::new(0, 0, w, h), primary: true }]
}

pub struct TestAgent {
    pub agent: AgentHandle,
    pub sim: SimControl,
    pub state_dir: PathBuf,
    /// The agent's (simulated) interface addresses; `set` simulates a
    /// network change, picked up within [`NET_CHECK`].
    pub net: StaticAddrs,
}

/// How often test agents re-read their (simulated) addresses.
pub const NET_CHECK: std::time::Duration = std::time::Duration::from_millis(50);

/// Parses an IP address (test shorthand).
pub fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

pub struct AgentSpec {
    pub token: Option<String>,
    pub monitors: Vec<Monitor>,
    pub p2p_port: u16,
    pub advertise: Vec<SocketAddr>,
    pub state_dir: Option<PathBuf>,
    /// Initial interface addresses.
    pub addrs: Vec<IpAddr>,
}

impl AgentSpec {
    pub fn new(token: &str) -> Self {
        AgentSpec {
            token: Some(token.into()),
            monitors: monitor(1920, 1080),
            p2p_port: 0,
            advertise: vec![],
            state_dir: None,
            addrs: vec![ip("10.10.15.10")],
        }
    }
}

pub async fn start_agent(hub: &HubHandle, spec: AgentSpec) -> TestAgent {
    let state_dir = spec.state_dir.unwrap_or_else(|| temp_dir("agent"));
    let cfg = AgentConfig {
        hub: Some(hub.quic_addr.to_string()),
        join_token: spec.token,
        p2p_port: Some(spec.p2p_port),
        p2p_advertise: spec.advertise,
        backend: Some("sim".into()),
        state_dir: Some(state_dir.clone()),
        ..Default::default()
    };
    let (backend, sim, rx) = SimBackend::with_control(spec.monitors);
    let net = StaticAddrs::new(spec.addrs);
    let opts = StartOptions { log_hook: None, addrs: Arc::new(net.clone()), net_check_interval: NET_CHECK };
    let agent = flow_agent::start_with(cfg, backend, rx, opts).await.expect("agent starts");
    TestAgent { agent, sim, state_dir, net }
}

/// Collects injected actions until `pred` is satisfied by the collected
/// list or the timeout expires; returns everything collected.
pub async fn collect_until(sim: &SimControl, timeout: Duration, pred: impl Fn(&[Action]) -> bool) -> Vec<Action> {
    let deadline = Instant::now() + timeout;
    let mut all = Vec::new();
    loop {
        all.extend(sim.take_injected());
        if pred(&all) || Instant::now() >= deadline {
            return all;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Panics unless every key/button pressed in `actions` is released later
/// and nothing is released without a press (no stuck or phantom keys).
pub fn assert_balanced(actions: &[Action]) {
    let mut keys = std::collections::BTreeSet::new();
    let mut buttons = std::collections::BTreeSet::new();
    for a in actions {
        match *a {
            Action::Key { usage, down: true } => {
                assert!(keys.insert(usage), "key {usage:#x} pressed twice: {actions:?}")
            }
            Action::Key { usage, down: false } => {
                assert!(keys.remove(&usage), "key {usage:#x} released without a press: {actions:?}")
            }
            Action::Button { button, down: true } => assert!(buttons.insert(button), "{button:?} pressed twice"),
            Action::Button { button, down: false } => {
                assert!(buttons.remove(&button), "{button:?} released without a press: {actions:?}")
            }
            _ => {}
        }
    }
    assert!(keys.is_empty() && buttons.is_empty(), "stuck: keys {keys:x?} buttons {buttons:?} in {actions:?}");
}

/// Waits until `f` returns true.
pub async fn eventually(timeout: Duration, f: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    f()
}

/// Minimal HTTP/1.1 client for the dashboard API.
pub async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let body = body.unwrap_or("");
    let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let status = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, body)
}

/// A node that speaks the hub protocol by hand instead of running an agent
/// core, for tests that need to send what a real agent never would. It is
/// registered (Hello with the join token, Welcome received), so its frames
/// reach the hub's routing code.
pub struct RawNode {
    pub id: NodeId,
    pub identity: Identity,
    pub conn: quinn::Connection,
    pub endpoint: quinn::Endpoint,
    /// Keeps the control stream open.
    _ctl: quinn::SendStream,
}

impl RawNode {
    pub async fn join(hub: &HubHandle, token: &str, hostname: &str) -> RawNode {
        let identity = Identity::generate(hostname).unwrap();
        let cc = client_config(&identity, ALPN_HUB, ServerPin::Exact(hub.fingerprint()), TransportOpts::HUB).unwrap();
        let endpoint = flow_net::endpoint::bind("127.0.0.1:0".parse().unwrap(), None).unwrap();
        let conn = endpoint.connect_with(cc, hub.quic_addr, SNI).unwrap().await.unwrap();
        let (mut ctl, mut ctl_r) = conn.open_bi().await.unwrap();
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            agent_version: "0.1.0".into(),
            hostname: hostname.into(),
            os: OsKind::Linux,
            join_token: Some(token.into()),
            p2p_port: 0,
            p2p_addrs: vec![],
            monitors: monitor(1920, 1080),
            caps: Capabilities { capture: true, inject: true, clipboard: true },
        };
        write_frame(&mut ctl, &ToHub::Hello(hello)).await.unwrap();
        let id = match read_frame::<_, FromHub>(&mut ctl_r, MAX_CONTROL_FRAME).await.unwrap() {
            Some(FromHub::Welcome(w)) => w.node_id,
            other => panic!("expected Welcome, got {other:?}"),
        };
        // Drain later control messages (layout, peers) so the hub's writer
        // never blocks on flow control.
        tokio::spawn(async move {
            while let Ok(Some(_)) = read_frame::<_, FromHub>(&mut ctl_r, MAX_CONTROL_FRAME).await {}
        });
        RawNode { id, identity, conn, endpoint, _ctl: ctl }
    }

    /// Opens a relay stream; frames the hub delivers on it arrive on the
    /// returned channel.
    pub async fn relay(&self) -> (quinn::SendStream, mpsc::UnboundedReceiver<RelayDown>) {
        let (mut s, mut r) = self.conn.open_bi().await.unwrap();
        write_frame(&mut s, &StreamKind::Relay).await.unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(Some(d)) = read_frame::<_, RelayDown>(&mut r, MAX_CONTROL_FRAME).await {
                if tx.send(d).is_err() {
                    break;
                }
            }
        });
        (s, rx)
    }

    /// Relay datagrams the hub delivers to this node.
    pub fn datagrams(&self) -> mpsc::UnboundedReceiver<RelayDatagramDown> {
        let conn = self.conn.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(b) = conn.read_datagram().await {
                if let Ok(d) = flow_proto::decode::<RelayDatagramDown>(&b) {
                    if tx.send(d).is_err() {
                        break;
                    }
                }
            }
        });
        rx
    }
}

/// Sends `probe` frames on `send` until one arrives on `rx`: the hub
/// registers a relay stream asynchronously, so frames sent right after
/// opening it may be dropped. Returns the first delivered frame.
pub async fn relay_probe(
    send: &mut quinn::SendStream,
    probe: &RelayUp,
    rx: &mut mpsc::UnboundedReceiver<RelayDown>,
) -> RelayDown {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        write_frame(send, probe).await.unwrap();
        if let Ok(Some(d)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            return d;
        }
        assert!(Instant::now() < deadline, "relay to {} never got through", probe.to);
    }
}
