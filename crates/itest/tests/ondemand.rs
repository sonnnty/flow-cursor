//! Remote control session on demand (GNOME/Wayland's RemoteDesktop portal
//! session, simulated): open only while the machine is connected to the
//! hub with sharing on, closed (after a grace period for hub drops)
//! otherwise; peers never route input to a machine whose session is closed
//! and nothing stays held there.

use flow_agent::backend::LocalEvent;
use flow_agent::AgentStatus;
use flow_itest::*;
use flow_keymap::keys;
use flow_layout::Side;
use flow_net::link::Action;
use flow_proto::{Button, NodeId, PathKind};
use flow_server::HubHandle;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const T: Duration = Duration::from_secs(5);
const KEY_B: u32 = 0x0007_0005;
const KEY_C: u32 = 0x0007_0006;
/// Grace period of the test agents (the real one is 10 s).
const GRACE: Duration = Duration::from_millis(2500);

fn key(usage: u32, down: bool) -> LocalEvent {
    LocalEvent::Key { usage, down }
}

fn button(down: bool) -> LocalEvent {
    LocalEvent::Button { button: Button::Left, down }
}

fn on_demand(sim: &flow_agent::backend::sim::SimControl) {
    sim.set_on_demand(Duration::from_millis(300));
}

fn on_demand_slow(sim: &flow_agent::backend::sim::SimControl) {
    sim.set_on_demand(Duration::from_millis(1500));
}

/// A (plain backend, like X11/Windows) | B (on-demand session), A left of B.
struct Pair {
    hub: Option<HubHandle>,
    hub_dir: PathBuf,
    hub_addr: SocketAddr,
    a: TestAgent,
    b: TestAgent,
    idb: NodeId,
}

async fn pair(prepare_b: fn(&flow_agent::backend::sim::SimControl)) -> Pair {
    init_logs();
    let hub_dir = temp_dir("hub");
    let hub = start_hub_in(hub_dir.clone()).await;
    let token = hub.join_token();
    let spec = |addr: &str| AgentSpec { addrs: vec![ip(addr)], hub_grace: Some(GRACE), ..AgentSpec::new(&token) };
    let a = start_agent(&hub, spec("10.10.15.10")).await;
    assert!(a.agent.wait(T, |s| s.connected).await);
    let b = start_agent(&hub, AgentSpec { prepare: Some(prepare_b), ..spec("10.10.15.11") }).await;
    assert!(b.agent.wait(T, |s| s.connected).await);
    let idb = b.agent.status().node_id.unwrap();
    for x in [&a, &b] {
        assert!(x.agent.wait(T, |s| s.online_peers.len() == 1 && s.layout_nodes == 2).await);
    }
    Pair { hub_addr: hub.quic_addr, hub: Some(hub), hub_dir, a, b, idb }
}

/// What A believes about B: online and able to be controlled.
fn sees_controllable(s: &AgentStatus, b: NodeId) -> bool {
    s.peers.iter().any(|p| p.id == b && p.online && !p.away && p.caps.inject)
}

/// B's session is open, the hub knows and A has a direct link to B.
async fn wait_b_controllable(w: &Pair) {
    let idb = w.idb;
    assert!(w.b.agent.wait(T, |s| s.controllable).await, "B's session opens");
    assert!(w.b.sim.session_open());
    assert!(w.a.agent.wait(T, |s| sees_controllable(s, idb)).await, "A learns that B can be controlled");
    assert!(w.a.agent.wait_link_path(idb, PathKind::P2p, T).await);
}

async fn a_controls_b(w: &Pair) {
    let idb = w.idb;
    w.a.sim.push_edge(Side::Right, 1919, 540);
    assert!(w.a.agent.wait(T, |s| s.controlling == Some(idb)).await, "A crosses into B");
    let got = collect_until(&w.b.sim, T, |v| v.iter().any(|a| matches!(a, Action::Entered { .. }))).await;
    assert!(got.iter().any(|a| matches!(a, Action::Entered { .. })), "{got:?}");
}

fn injected(t: &TestAgent) -> Vec<Action> {
    t.sim.state.lock().unwrap().injected.clone()
}

/// A tries to cross into B; it must be refused (pointer stays home).
async fn crossing_refused(w: &Pair) {
    let before = injected(&w.b).len();
    w.a.sim.push_edge(Side::Right, 1919, 540);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(w.a.agent.status().controlling, None, "A must not cross into B");
    assert!(!w.a.sim.capturing());
    assert_eq!(injected(&w.b).len(), before, "nothing reaches B");
}

/// A holds a key and a button on B.
async fn hold_key_and_button(w: &Pair) {
    w.a.sim.emit(key(keys::A, true));
    w.a.sim.emit(button(true));
    assert!(eventually(T, || injected(&w.b).iter().any(|a| matches!(a, Action::Button { down: true, .. }))).await);
}

/// Everything B's "OS" saw is balanced and ends with control handed back,
/// and nothing was injected while its session was closed.
fn assert_released_cleanly(b: &TestAgent) {
    let all = injected(b);
    assert_balanced(&all);
    assert_eq!(all.last(), Some(&Action::Left), "control ends with Left: {all:?}");
    assert_eq!(b.sim.injected_while_closed(), vec![], "nothing injected while closed");
    let s = b.agent.status();
    assert!(!s.keys_held && s.controlled_by.is_empty(), "{s:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_opens_with_the_hub_and_closes_after_the_grace_period() {
    let mut w = pair(on_demand_slow).await;
    let idb = w.idb;

    // Connected: B opens its session, which takes a while (the portal round
    // trip). Until it is open, A sees B as not controllable and does not
    // cross, even though B is online.
    assert!(w.b.agent.wait(T, |s| s.session_wanted).await);
    if !w.b.sim.session_open() {
        assert!(w.a.agent.wait(T, |s| s.peers.iter().any(|p| p.id == idb && p.online && !p.caps.inject)).await);
        if !w.b.sim.session_open() {
            crossing_refused(&w).await;
        }
    }
    wait_b_controllable(&w).await;
    assert_eq!(w.b.sim.session_log(), vec![true]);
    a_controls_b(&w).await;
    hold_key_and_button(&w).await;

    // The hub goes down. Within the grace period B stays controllable over
    // the direct link (the session does not flap).
    let dropped = Instant::now();
    w.hub.take().unwrap().shutdown().await;
    assert!(w.b.agent.wait(T, |s| !s.connected).await);
    w.a.sim.emit(key(KEY_B, true));
    w.a.sim.emit(key(KEY_B, false));
    assert!(eventually(T, || injected(&w.b).contains(&Action::Key { usage: KEY_B, down: false })).await);
    assert!(w.b.sim.session_open() && dropped.elapsed() < GRACE, "still open during the grace period");

    // After it: B releases what A holds, hands control back and closes the
    // session; A loses its link to B and returns home.
    assert!(w.b.agent.wait(GRACE + T, |s| !s.controllable).await);
    assert!(dropped.elapsed() >= GRACE, "closed after {:?}", dropped.elapsed());
    assert!(!w.b.sim.session_open());
    assert_eq!(w.b.sim.session_log(), vec![true, false]);
    assert!(w.a.agent.wait(T, |s| s.controlling.is_none()).await, "A returns home");
    assert_released_cleanly(&w.b);
    // A's devices are still its own and nothing is stuck there.
    assert!(w.a.agent.wait(T, |s| !s.keys_held).await);

    // B cannot be controlled while the hub is down (the accepted trade-off);
    // A can still control others, which uses its own capture.
    crossing_refused(&w).await;
    assert_eq!(w.b.sim.injected_while_closed(), vec![], "no input routed to B while closed");

    // The hub comes back: B reopens (no new permission dialog on GNOME: the
    // stored restore token is reused) and A can control it again.
    let hub = restart_hub(w.hub_dir.clone(), w.hub_addr).await;
    w.hub = Some(hub);
    for x in [&w.a, &w.b] {
        assert!(x.agent.wait(T, |s| s.connected).await);
    }
    wait_b_controllable(&w).await;
    assert_eq!(w.b.sim.session_log(), vec![true, false, true]);
    a_controls_b(&w).await;
    w.a.sim.emit(key(KEY_C, true));
    w.a.sim.emit(key(KEY_C, false));
    assert!(eventually(T, || injected(&w.b).contains(&Action::Key { usage: KEY_C, down: false })).await);
    w.hub.take().unwrap().shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quick_hub_restart_keeps_the_session_open() {
    let mut w = pair(on_demand).await;
    wait_b_controllable(&w).await;

    let dropped = Instant::now();
    w.hub.take().unwrap().shutdown().await;
    assert!(w.b.agent.wait(T, |s| !s.connected).await);
    let hub = restart_hub(w.hub_dir.clone(), w.hub_addr).await;
    w.hub = Some(hub);
    for x in [&w.a, &w.b] {
        assert!(x.agent.wait(T, |s| s.connected).await);
    }
    assert!(dropped.elapsed() < GRACE, "reconnected within the grace period ({:?})", dropped.elapsed());
    // Past the end of the grace period counted from the drop: still open,
    // never closed in between.
    tokio::time::sleep(GRACE.saturating_sub(dropped.elapsed()) + Duration::from_millis(500)).await;
    assert!(w.b.sim.session_open());
    assert_eq!(w.b.sim.session_log(), vec![true], "the session never closed");
    assert!(w.b.agent.status().controllable);
    wait_b_controllable(&w).await;
    a_controls_b(&w).await;
    w.hub.take().unwrap().shutdown().await;
}

async fn set_allowed_networks(hub: &HubHandle, nets: Value) {
    let web = hub.http_addr.unwrap();
    let (code, body) = http(web, "GET", "/api/settings", Some(ADMIN), None).await;
    assert_eq!(code, 200);
    let mut s: Value = serde_json::from_str(&body).unwrap();
    s["allowed_networks"] = nets;
    let (code, msg) = http(web, "PUT", "/api/settings", Some(ADMIN), Some(&s.to_string())).await;
    assert_eq!(code, 200, "{msg}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_and_away_close_the_session_at_once() {
    let w = pair(on_demand).await;
    let idb = w.idb;
    wait_b_controllable(&w).await;
    a_controls_b(&w).await;
    hold_key_and_button(&w).await;

    // Paused while being controlled: released first, then closed, no grace.
    w.b.agent.set_paused(true);
    assert!(w.b.agent.wait(Duration::from_secs(1), |s| s.paused && !s.controllable).await);
    assert!(!w.b.sim.session_open());
    assert!(w.a.agent.wait(T, |s| s.controlling.is_none()).await, "A returns home");
    assert_released_cleanly(&w.b);
    crossing_refused(&w).await;

    w.b.agent.set_paused(false);
    wait_b_controllable(&w).await;
    assert_eq!(w.b.sim.session_log(), vec![true, false, true]);

    // Away (not on an allowed network): the same.
    set_allowed_networks(w.hub.as_ref().unwrap(), json!(["10.10.15.0/24"])).await;
    a_controls_b(&w).await;
    hold_key_and_button(&w).await;
    w.b.net.set([ip("192.168.1.50")]);
    assert!(w.b.agent.wait(T, |s| s.away && !s.controllable).await);
    assert!(w.b.agent.status().connected, "away nodes stay connected");
    assert!(!w.b.sim.session_open());
    assert!(w.a.agent.wait(T, |s| s.controlling.is_none()).await);
    assert_released_cleanly(&w.b);
    assert!(w.a.agent.wait(T, |s| !sees_controllable(s, idb)).await);
    crossing_refused(&w).await;

    w.b.net.set([ip("10.10.15.11")]);
    wait_b_controllable(&w).await;
    assert_eq!(w.b.sim.session_log(), vec![true, false, true, false, true]);
    a_controls_b(&w).await;
    w.hub.unwrap().shutdown().await;
}

/// The IPC snapshot the tray gets from `t`.
fn snapshot(t: &TestAgent) -> flow_ipc::Snapshot {
    let (control, _rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = flow_agent::ipc::IpcContext {
        agent: t.agent.observer(),
        info: std::sync::Arc::new(flow_agent::ipc::AgentInfo {
            config_file: t.state_dir.join("agent.toml"),
            log_file: None,
            cfg: flow_agent::config::AgentConfig { state_dir: Some(t.state_dir.clone()), ..Default::default() },
        }),
        logs: Default::default(),
        control,
    };
    flow_agent::ipc::snapshot(&ctx)
}

/// No problem, no warning, and the reason for a closed session (or none).
fn assert_calm(s: &flow_ipc::Snapshot, idle: Option<flow_ipc::SessionIdle>) {
    assert_eq!(s.session_idle, idle, "{s:?}");
    assert!(s.problems.is_empty(), "no problem: {:?}", s.problems);
    assert!(s.warnings.is_empty(), "no warning: {:?}", s.warnings);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_closed_on_purpose_is_no_problem_in_the_status() {
    use flow_ipc::SessionIdle::*;
    let mut w = pair(on_demand).await;
    wait_b_controllable(&w).await;
    let s = snapshot(&w.b);
    assert!(s.can_inject && s.can_clipboard && s.controllable && s.session_wanted);
    assert_calm(&s, None);

    // Paused: closed on purpose.
    w.b.agent.set_paused(true);
    assert!(w.b.agent.wait(T, |s| s.paused && !s.controllable).await);
    let s = snapshot(&w.b);
    assert!(!s.can_inject && !s.can_clipboard && !s.session_wanted);
    assert_calm(&s, Some(Paused));
    w.b.agent.set_paused(false);
    wait_b_controllable(&w).await;
    assert_calm(&snapshot(&w.b), None);

    // Away.
    set_allowed_networks(w.hub.as_ref().unwrap(), json!(["10.10.15.0/24"])).await;
    w.b.net.set([ip("192.168.1.50")]);
    assert!(w.b.agent.wait(T, |s| s.away && !s.controllable).await);
    assert_calm(&snapshot(&w.b), Some(Away));
    w.b.net.set([ip("10.10.15.11")]);
    wait_b_controllable(&w).await;

    // The hub is gone and the grace period is over.
    w.hub.take().unwrap().shutdown().await;
    assert!(w.b.agent.wait(GRACE + T, |s| !s.controllable && !s.connected).await);
    let s = snapshot(&w.b);
    assert!(!s.hub_connected);
    assert_calm(&s, Some(Disconnected));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_lost_by_the_backend_makes_the_machine_uncontrollable() {
    let w = pair(on_demand).await;
    let idb = w.idb;
    wait_b_controllable(&w).await;
    a_controls_b(&w).await;
    hold_key_and_button(&w).await;

    // The portal session dies under B (EI disconnect): the keys cannot be
    // released through it any more, but B forgets them, the hub and A learn
    // that B cannot be controlled, and A returns home.
    w.b.sim.lose_session();
    assert!(w.b.agent.wait(T, |s| !s.controllable && !s.keys_held && s.controlled_by.is_empty()).await);
    assert!(w.a.agent.wait(T, |s| s.controlling.is_none() && !sees_controllable(s, idb)).await);
    assert!(w.b.agent.status().session_wanted, "the core still wants it open");
    w.hub.unwrap().shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_the_agent_releases_and_closes_the_session() {
    let w = pair(on_demand).await;
    wait_b_controllable(&w).await;
    a_controls_b(&w).await;
    hold_key_and_button(&w).await;
    let sim = w.b.sim.clone();
    w.b.agent.shutdown().await;
    assert!(!sim.session_open());
    assert_eq!(sim.session_log(), vec![true, false]);
    let all = sim.state.lock().unwrap().injected.clone();
    assert_balanced(&all);
    assert_eq!(all.last(), Some(&Action::Left));
    w.hub.unwrap().shutdown().await;
}
