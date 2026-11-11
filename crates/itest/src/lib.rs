//! Test harness: in-process hub and simulated agents.

use flow_agent::backend::sim::{SimBackend, SimControl};
use flow_agent::config::AgentConfig;
use flow_agent::AgentHandle;
use flow_net::link::Action;
use flow_proto::{Monitor, Rect};
use flow_server::{HubConfig, HubHandle};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
}

pub struct AgentSpec {
    pub token: Option<String>,
    pub monitors: Vec<Monitor>,
    pub p2p_port: u16,
    pub advertise: Vec<SocketAddr>,
    pub state_dir: Option<PathBuf>,
}

impl AgentSpec {
    pub fn new(token: &str) -> Self {
        AgentSpec {
            token: Some(token.into()),
            monitors: monitor(1920, 1080),
            p2p_port: 0,
            advertise: vec![],
            state_dir: None,
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
    let agent = flow_agent::start(cfg, backend, rx, None).await.expect("agent starts");
    TestAgent { agent, sim, state_dir }
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
