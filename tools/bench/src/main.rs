//! flow-bench: measures end-to-end input latency between two agents on this
//! machine, once over the direct (P2P) path and once relayed through the
//! hub. Everything runs in one process with the simulated backend, so one
//! clock measures the whole pipeline: capture event → agent core → link
//! (QUIC, TLS) → receiving agent → injector.
//!
//! These are localhost numbers (no network latency); on a LAN add the
//! network RTT/2. Real LAN numbers: watch the dashboard's RTT column.

use anyhow::{bail, Context, Result};
use clap::Parser;
use flow_agent::backend::sim::{SimBackend, SimControl};
use flow_agent::backend::LocalEvent;
use flow_agent::config::AgentConfig;
use flow_agent::AgentHandle;
use flow_net::link::Action;
use flow_proto::{Monitor, NodeId, PathKind, PathPolicy, Rect};
use flow_server::{HubConfig, HubHandle};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(version, about = "End-to-end input latency over P2P vs hub relay (localhost)")]
struct Args {
    /// Events measured per path and event kind.
    #[arg(long, default_value_t = 1000)]
    samples: usize,
    /// Print machine-readable JSON instead of a table.
    #[arg(long)]
    json: bool,
}

struct Agent {
    handle: AgentHandle,
    sim: SimControl,
}

async fn agent(hub: &HubHandle, token: &str) -> Result<Agent> {
    let dir = std::env::temp_dir().join(format!("flow-bench-{}", rand::random::<u64>()));
    let cfg = AgentConfig {
        hub: Some(hub.quic_addr.to_string()),
        join_token: Some(token.into()),
        p2p_port: Some(0),
        state_dir: Some(dir),
        ..Default::default()
    };
    let mons = vec![Monitor { name: "bench".into(), rect: Rect::new(0, 0, 1920, 1080), primary: true }];
    let (backend, sim, rx) = SimBackend::with_control(mons);
    let handle = flow_agent::start(cfg, backend, rx, None).await?;
    if !handle.wait(Duration::from_secs(10), |s| s.connected).await {
        bail!("agent did not connect to the in-process hub");
    }
    Ok(Agent { handle, sim })
}

#[derive(Debug)]
struct Stats {
    path: &'static str,
    kind: &'static str,
    n: usize,
    min: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
    mean: f64,
}

fn stats(path: &'static str, kind: &'static str, mut v: Vec<Duration>) -> Stats {
    v.sort();
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    let q = |p: f64| us(v[((v.len() - 1) as f64 * p).round() as usize]);
    Stats {
        path,
        kind,
        n: v.len(),
        min: us(v[0]),
        p50: q(0.5),
        p95: q(0.95),
        p99: q(0.99),
        max: us(*v.last().unwrap()),
        mean: v.iter().map(|d| us(*d)).sum::<f64>() / v.len() as f64,
    }
}

async fn next_matching(rx: &mut mpsc::UnboundedReceiver<Action>, f: impl Fn(&Action) -> bool) -> Result<()> {
    loop {
        let a =
            tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.context("timeout waiting for injection")?;
        match a {
            Some(a) if f(&a) => return Ok(()),
            Some(_) => {}
            None => bail!("observer closed"),
        }
    }
}

async fn measure(
    a: &Agent,
    rx: &mut mpsc::UnboundedReceiver<Action>,
    n: usize,
) -> Result<(Vec<Duration>, Vec<Duration>)> {
    let key = flow_keymap::keys::A;
    let mut keys = Vec::with_capacity(n);
    let mut moves = Vec::with_capacity(n);
    for i in 0..(n + 50) {
        let down = i % 2 == 0;
        let t0 = Instant::now();
        a.sim.emit(LocalEvent::Key { usage: key, down });
        next_matching(rx, |x| *x == Action::Key { usage: key, down }).await?;
        if i >= 50 {
            keys.push(t0.elapsed());
        }
    }
    // Entry is at x = 0: go right first, then back and forth (never cross home).
    let mut dir = -1.0;
    for i in 0..(n + 50) {
        if i % 200 == 0 {
            dir = -dir;
        }
        let t0 = Instant::now();
        a.sim.emit(LocalEvent::Motion { dx: dir, dy: 0.0 });
        next_matching(rx, |x| matches!(x, Action::MoveAbs { .. })).await?;
        if i >= 50 {
            moves.push(t0.elapsed());
        }
        // Space motion events out a little like a real mouse (≈1 kHz).
        tokio::time::sleep(Duration::from_micros(500)).await;
    }
    Ok((keys, moves))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .try_init();
    let hub = flow_server::start(HubConfig {
        data_dir: std::env::temp_dir().join(format!("flow-bench-hub-{}", rand::random::<u64>())),
        quic_addr: "127.0.0.1:0".parse()?,
        http_addr: None,
        admin_token: None,
        mdns: false,
        name: "bench".into(),
        agent_builds: None,
    })
    .await?;
    let token = hub.join_token();
    let a = agent(&hub, &token).await?;
    let b = agent(&hub, &token).await?;
    let idb: NodeId = b.handle.status().node_id.context("B has no id")?;
    if !a.handle.wait(Duration::from_secs(10), |s| s.online_peers.contains(&idb) && s.layout_nodes == 2).await {
        bail!("agents did not see each other");
    }
    if !a.handle.wait_link_path(idb, PathKind::P2p, Duration::from_secs(10)).await {
        bail!("direct path did not come up");
    }
    let mut rx = b.sim.subscribe();
    // A drives B.
    a.sim.emit(LocalEvent::EdgeHit { side: flow_layout::Side::Right, x: 1919, y: 540 });
    next_matching(&mut rx, |x| matches!(x, Action::Entered { .. })).await?;

    let mut results = Vec::new();
    let (k, m) = measure(&a, &mut rx, args.samples).await?;
    let p2p_rtt = a.handle.link_status(idb).and_then(|s| s.p2p_rtt_us);
    results.push(stats("p2p", "key", k));
    results.push(stats("p2p", "motion", m));

    {
        let mut st = hub.hub.lock();
        st.p.settings.path_policy = PathPolicy::ForceRelay;
        st.broadcast_settings();
    }
    if !a.handle.wait_link_path(idb, PathKind::Relay, Duration::from_secs(10)).await {
        bail!("relay path did not come up");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (k, m) = measure(&a, &mut rx, args.samples).await?;
    let relay_rtt = a.handle.link_status(idb).and_then(|s| s.relay_rtt_us);
    results.push(stats("relay", "key", k));
    results.push(stats("relay", "motion", m));

    if args.json {
        let rows: Vec<_> = results
            .iter()
            .map(|s| {
                serde_json::json!({"path": s.path, "kind": s.kind, "samples": s.n, "min_us": s.min, "p50_us": s.p50,
                    "p95_us": s.p95, "p99_us": s.p99, "max_us": s.max, "mean_us": s.mean})
            })
            .collect();
        println!("{}", serde_json::json!({"results": rows, "ping_rtt_us": {"p2p": p2p_rtt, "relay": relay_rtt}}));
    } else {
        println!("FlowCursor end-to-end input latency, localhost (capture event → injection on the other agent)");
        println!(
            "{:<6} {:<7} {:>7} {:>9} {:>9} {:>9} {:>9} {:>9}",
            "path", "event", "samples", "min", "p50", "p95", "p99", "max"
        );
        for s in &results {
            println!(
                "{:<6} {:<7} {:>7} {:>7.0}µs {:>7.0}µs {:>7.0}µs {:>7.0}µs {:>7.0}µs",
                s.path, s.kind, s.n, s.min, s.p50, s.p95, s.p99, s.max
            );
        }
        let f = |v: Option<u32>| v.map(|v| format!("{:.0}µs", v)).unwrap_or_else(|| "–".into());
        println!("link ping RTT (smoothed): p2p {}, relay {}", f(p2p_rtt), f(relay_rtt));
        println!("Note: no network latency here; on a LAN add about half the network RTT per hop.");
    }
    a.handle.shutdown().await;
    b.handle.shutdown().await;
    hub.shutdown().await;
    Ok(())
}
