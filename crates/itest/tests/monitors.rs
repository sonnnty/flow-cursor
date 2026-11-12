//! Monitors that change while everything runs: a laptop is docked and
//! undocked (and restarted undocked), the hub re-lays out around it, edges
//! come back and crossing works again without restarting anything; layouts
//! in which crossing cannot work show up as warnings in the dashboard state
//! and in the agent's status.

use flow_agent::backend::LocalEvent;
use flow_itest::*;
use flow_layout::Side;
use flow_net::link::Action;
use flow_proto::{Monitor, NodeId, PathKind, Rect};
use flow_server::HubHandle;
use serde_json::Value;
use std::time::Duration;

const T: Duration = Duration::from_secs(5);

fn mon(x: i32, y: i32, w: i32, h: i32) -> Monitor {
    Monitor { name: format!("SIM-{w}x{h}@{x}"), rect: Rect::new(x, y, w, h), primary: x == 0 && y == 0 }
}

/// The owner's laptop on its own, and docked (4536 px wide in all).
fn alone() -> Vec<Monitor> {
    vec![mon(0, 0, 1536, 960)]
}

fn docked() -> Vec<Monitor> {
    vec![mon(0, 0, 1536, 960), mon(1536, 0, 1920, 1080), mon(3456, 0, 1080, 1920)]
}

fn pos(hub: &HubHandle, id: NodeId) -> (i32, i32) {
    let st = hub.hub.lock();
    let n = &st.p.nodes[&id.0];
    (n.x, n.y)
}

async fn state(hub: &HubHandle) -> Value {
    let (code, body) = http(hub.http_addr.unwrap(), "GET", "/api/state", Some(ADMIN), None).await;
    assert_eq!(code, 200);
    serde_json::from_str(&body).unwrap()
}

async fn put_layout(hub: &HubHandle, positions: &[(NodeId, i32, i32)]) {
    let list: Vec<String> =
        positions.iter().map(|(id, x, y)| format!(r#"{{"id":{},"x":{x},"y":{y}}}"#, id.0)).collect();
    let body = format!(r#"{{"positions":[{}]}}"#, list.join(","));
    let (code, _) = http(hub.http_addr.unwrap(), "PUT", "/api/layout", Some(ADMIN), Some(&body)).await;
    assert_eq!(code, 204);
}

/// The kinds and node ids of the dashboard's layout warnings.
fn warnings(st: &Value) -> Vec<(String, Vec<u64>)> {
    st["layout_warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            let nodes = w["nodes"].as_array().unwrap().iter().map(|n| n.as_u64().unwrap()).collect();
            (w["kind"].as_str().unwrap().to_string(), nodes)
        })
        .collect()
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

/// `from` leads to `to` over its right edge at `x` (local).
fn right_edge_to(t: &TestAgent, x: i32, to: NodeId) -> bool {
    t.sim.edges().iter().any(|e| e.side == Side::Right && e.pos == x && e.to == to)
}

/// Crosses from the laptop's right edge (at local `x`) to the PC and back.
async fn cross_and_back(laptop: &TestAgent, pc: &TestAgent, x: i32, ids: (NodeId, NodeId)) {
    let (idl, idp) = ids;
    assert!(laptop.agent.wait_link_path(idp, PathKind::P2p, T).await, "laptop → PC link");
    pc.sim.take_injected();
    laptop.sim.emit(LocalEvent::EdgeHit { side: Side::Right, x: x - 1, y: 480 });
    assert!(laptop.agent.wait(T, |s| s.controlling == Some(idp)).await, "laptop → PC");
    laptop.sim.emit(LocalEvent::Motion { dx: 10.0, dy: 0.0 });
    let got = collect_until(&pc.sim, T, |v| v.iter().any(|a| matches!(a, Action::MoveAbs { .. }))).await;
    assert!(matches!(got.first(), Some(Action::Entered { x: 0, .. })), "{got:?}");
    assert!(pc.agent.wait(T, |s| s.controlled_by == vec![idl]).await);
    laptop.sim.emit(LocalEvent::Motion { dx: -5000.0, dy: 0.0 });
    assert!(laptop.agent.wait(T, |s| s.controlling.is_none()).await, "back on the laptop");
    let (wx, _) = laptop.sim.last_warp().unwrap();
    assert_eq!(wx, x - 1, "enters at the laptop's current right edge");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn docking_undocking_and_restarting_keep_the_pc_next_to_the_laptop() {
    init_logs();
    let hub = start_hub().await;
    let token = hub.join_token();
    let laptop = start_agent(&hub, AgentSpec { monitors: alone(), ..AgentSpec::new(&token) }).await;
    assert!(laptop.agent.wait(T, |s| s.connected).await);
    let pc = start_agent(&hub, AgentSpec { addrs: vec![ip("10.10.15.11")], ..AgentSpec::new(&token) }).await;
    assert!(pc.agent.wait(T, |s| s.connected).await);
    let idl = laptop.agent.status().node_id.unwrap();
    let idp = pc.agent.status().node_id.unwrap();
    for x in [&laptop, &pc] {
        assert!(x.agent.wait(T, |s| s.online_peers.len() == 1 && s.layout_nodes == 2).await);
    }
    // New machines go right of everything: the PC touches the laptop.
    assert_eq!(pos(&hub, idp), (1536, 0));
    assert!(eventually(T, || right_edge_to(&laptop, 1536, idp)).await);
    assert!(pc.agent.wait(T, |s| s.edges == Some(1) && !s.no_edges).await);
    cross_and_back(&laptop, &pc, 1536, (idl, idp)).await;

    // --- docked: the agent reports three monitors mid-session ------------
    laptop.sim.set_monitors(docked());
    assert!(eventually(T, || pos(&hub, idp) == (4536, 0)).await, "the PC follows the laptop's right edge");
    assert_eq!(hub.hub.lock().p.nodes[&idl.0].monitors.len(), 3);
    assert!(eventually(T, || right_edge_to(&laptop, 4536, idp)).await, "{:?}", laptop.sim.edges());
    // The PC's agent got the new layout too: its left edge leads back.
    assert!(eventually(T, || pc.sim.edges().iter().any(|e| e.side == Side::Left && e.to == idl)).await);
    let st = state(&hub).await;
    assert!(warnings(&st).is_empty(), "{st}");
    let note = st["last_relayout"]["text"].as_str().unwrap();
    assert!(note.contains("now has 3 monitors (had 1)") && note.contains("from 1536,0 to 4536,0"), "{note}");
    assert_eq!(st["last_relayout"]["moved"][0].as_u64(), Some(idp.0 as u64));
    cross_and_back(&laptop, &pc, 4536, (idl, idp)).await;

    // --- undocked -----------------------------------------------------------
    laptop.sim.set_monitors(alone());
    assert!(eventually(T, || pos(&hub, idp) == (1536, 0)).await);
    assert!(eventually(T, || right_edge_to(&laptop, 1536, idp)).await);
    cross_and_back(&laptop, &pc, 1536, (idl, idp)).await;
    // Screens off for a moment: an empty list changes nothing.
    laptop.sim.set_monitors(vec![]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hub.hub.lock().p.nodes[&idl.0].monitors, alone());

    // --- docked again, then the agent restarts undocked ------------------
    laptop.sim.set_monitors(docked());
    assert!(eventually(T, || pos(&hub, idp) == (4536, 0)).await);
    let dir = laptop.state_dir.clone();
    laptop.agent.shutdown().await;
    assert!(pc.agent.wait(T, |s| s.online_peers.is_empty()).await);
    let laptop =
        start_agent(&hub, AgentSpec { token: None, state_dir: Some(dir), monitors: alone(), ..AgentSpec::new("") })
            .await;
    assert!(laptop.agent.wait(T, |s| s.connected && s.online_peers.len() == 1).await);
    assert_eq!(pos(&hub, idp), (1536, 0), "the report in the hello re-lays out too");
    assert!(eventually(T, || right_edge_to(&laptop, 1536, idp)).await);
    assert!(eventually(T, || pc.sim.edges().iter().any(|e| e.side == Side::Left && e.to == idl)).await);
    cross_and_back(&laptop, &pc, 1536, (idl, idp)).await;
    assert!(warnings(&state(&hub).await).is_empty());
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlap_and_no_edge_warnings_in_the_dashboard_and_the_agent() {
    init_logs();
    let hub = start_hub().await;
    let token = hub.join_token();
    let laptop = start_agent(&hub, AgentSpec { monitors: alone(), ..AgentSpec::new(&token) }).await;
    assert!(laptop.agent.wait(T, |s| s.connected).await);
    let pc = start_agent(&hub, AgentSpec { addrs: vec![ip("10.10.15.11")], ..AgentSpec::new(&token) }).await;
    assert!(pc.agent.wait(T, |s| s.connected).await);
    let idl = laptop.agent.status().node_id.unwrap();
    let idp = pc.agent.status().node_id.unwrap();
    for x in [&laptop, &pc] {
        assert!(x.agent.wait(T, |s| s.online_peers.len() == 1 && s.layout_nodes == 2).await);
    }
    let (l, p) = (idl.0 as u64, idp.0 as u64);
    assert!(warnings(&state(&hub).await).is_empty());
    assert!(snapshot(&laptop).warnings.is_empty());

    // Dragged on top of the laptop and saved: an overlap, no edges.
    put_layout(&hub, &[(idp, 1000, 0)]).await;
    let st = state(&hub).await;
    assert_eq!(warnings(&st), vec![("overlap".to_string(), vec![l, p])]);
    let text = st["layout_warnings"][0]["text"].as_str().unwrap();
    assert!(text.contains("overlap") && text.contains("drag them apart"), "{text}");
    for x in [&laptop, &pc] {
        assert!(x.agent.wait(T, |s| s.no_edges && s.edges == Some(0)).await);
    }
    assert!(laptop.sim.edges().is_empty());
    assert_eq!(snapshot(&laptop).warnings, vec![flow_agent::ipc::NO_EDGES.to_string()]);
    assert!(snapshot(&laptop).problems.is_empty(), "a note, not a problem");

    // Diagonal: no overlap, but neither touches the other.
    put_layout(&hub, &[(idp, 1536, 960)]).await;
    let st = state(&hub).await;
    assert_eq!(warnings(&st), vec![("no_edge".to_string(), vec![l]), ("no_edge".to_string(), vec![p])]);
    let name = st["nodes"].as_array().unwrap().iter().find(|n| n["id"] == p).unwrap()["name"].clone();
    let text = st["layout_warnings"][1]["text"].as_str().unwrap();
    assert_eq!(text, format!("{} does not touch any other screen: drag it next to one.", name.as_str().unwrap()));
    // An offline machine is not warned about.
    pc.agent.shutdown().await;
    assert!(eventually(T, || hub.hub.lock().online.len() == 1).await);
    assert_eq!(warnings(&state(&hub).await), vec![("no_edge".to_string(), vec![l])]);

    // Overlapping again; the laptop's next monitor report pushes the
    // (offline) PC out of the way, the shortest way, so that they touch.
    put_layout(&hub, &[(idp, 1000, 0)]).await;
    assert_eq!(warnings(&state(&hub).await).len(), 1);
    laptop.sim.set_monitors(vec![mon(0, 0, 1536, 960), mon(1536, 0, 1920, 1080)]);
    assert!(eventually(T, || pos(&hub, idp) != (1000, 0)).await);
    let st = state(&hub).await;
    assert!(warnings(&st).is_empty(), "{st}");
    assert!(laptop.agent.wait(T, |s| !s.no_edges && s.edges.unwrap_or(0) > 0).await);
    assert!(snapshot(&laptop).warnings.is_empty());
    hub.shutdown().await;
}
