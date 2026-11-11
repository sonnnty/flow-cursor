//! Allowed networks ("away") and the crossing key, end to end: hub +
//! simulated agents on localhost, real QUIC, settings through the
//! dashboard API, network changes through the injectable address source.

use flow_agent::backend::LocalEvent;
use flow_itest::*;
use flow_keymap::keys;
use flow_layout::Side;
use flow_net::link::Action;
use flow_proto::{Button, ClipContent, NodeId};
use flow_server::HubHandle;
use serde_json::{json, Value};
use std::time::Duration;

const T: Duration = Duration::from_secs(5);
const KEY_A: u32 = keys::A;

fn key(usage: u32, down: bool) -> LocalEvent {
    LocalEvent::Key { usage, down }
}

fn button(down: bool) -> LocalEvent {
    LocalEvent::Button { button: Button::Left, down }
}

/// Two machines side by side: A (1920x1080) | B (1920x1080).
struct Pair {
    hub: HubHandle,
    a: TestAgent,
    b: TestAgent,
    ida: NodeId,
    idb: NodeId,
}

async fn pair() -> Pair {
    init_logs();
    let hub = start_hub().await;
    let token = hub.join_token();
    let a = start_agent(&hub, AgentSpec { addrs: vec![ip("10.10.15.10")], ..AgentSpec::new(&token) }).await;
    assert!(a.agent.wait(T, |s| s.connected).await);
    let b = start_agent(&hub, AgentSpec { addrs: vec![ip("10.10.15.11")], ..AgentSpec::new(&token) }).await;
    assert!(b.agent.wait(T, |s| s.connected).await);
    let (ida, idb) = (a.agent.status().node_id.unwrap(), b.agent.status().node_id.unwrap());
    for x in [&a, &b] {
        assert!(x.agent.wait(T, |s| s.online_peers.len() == 1 && s.layout_nodes == 2).await);
    }
    // Links up before anyone crosses.
    assert!(a.agent.wait_link_path(idb, flow_proto::PathKind::P2p, T).await);
    assert!(b.agent.wait_link_path(ida, flow_proto::PathKind::P2p, T).await);
    Pair { hub, a, b, ida, idb }
}

/// Changes global settings through the dashboard API (validation path).
async fn set_global(hub: &HubHandle, changes: Value) -> (u16, String) {
    let web = hub.http_addr.unwrap();
    let (code, body) = http(web, "GET", "/api/settings", Some(ADMIN), None).await;
    assert_eq!(code, 200);
    let mut s: Value = serde_json::from_str(&body).unwrap();
    for (k, v) in changes.as_object().unwrap() {
        s[k] = v.clone();
    }
    http(web, "PUT", "/api/settings", Some(ADMIN), Some(&s.to_string())).await
}

async fn patch_node(hub: &HubHandle, id: NodeId, body: Value) -> (u16, String) {
    http(hub.http_addr.unwrap(), "PATCH", &format!("/api/nodes/{}", id.0), Some(ADMIN), Some(&body.to_string())).await
}

async fn node_view(hub: &HubHandle, id: NodeId) -> Value {
    let (code, body) = http(hub.http_addr.unwrap(), "GET", "/api/state", Some(ADMIN), None).await;
    assert_eq!(code, 200);
    let st: Value = serde_json::from_str(&body).unwrap();
    st["nodes"].as_array().unwrap().iter().find(|n| n["id"] == json!(id.0)).cloned().unwrap()
}

async fn wait_status(hub: &HubHandle, id: NodeId, want: &str) -> bool {
    let deadline = std::time::Instant::now() + T;
    while std::time::Instant::now() < deadline {
        if node_view(hub, id).await["status"] == json!(want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    false
}

fn injected(t: &TestAgent) -> Vec<Action> {
    t.sim.state.lock().unwrap().injected.clone()
}

fn keys_of(actions: &[Action]) -> Vec<(u32, bool)> {
    actions
        .iter()
        .filter_map(|a| match *a {
            Action::Key { usage, down } => Some((usage, down)),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn away_node_stops_sharing_releases_keys_and_comes_back() {
    let w = pair().await;
    let (ida, idb) = (w.ida, w.idb);

    // Invalid CIDRs are refused with a clear message.
    let (code, msg) = set_global(&w.hub, json!({ "allowed_networks": ["10.10.15.0/33"] })).await;
    assert_eq!(code, 400);
    assert!(msg.contains("10.10.15.0/33"), "{msg}");

    let (code, _) = set_global(&w.hub, json!({ "allowed_networks": ["10.10.15.0/24", "fd00::/8"] })).await;
    assert_eq!(code, 200);
    for x in [&w.a, &w.b] {
        assert!(x.agent.wait(T, |s| s.settings.as_ref().is_some_and(|s| s.allowed_networks.len() == 2)).await);
        assert!(!x.agent.status().away, "both are at home");
    }
    assert!(wait_status(&w.hub, idb, "online").await);

    // A controls B and holds a key and a button.
    w.a.sim.push_edge(Side::Right, 1919, 540);
    assert!(w.a.agent.wait(T, |s| s.controlling == Some(idb)).await);
    w.a.sim.emit(key(KEY_A, true));
    w.a.sim.emit(button(true));
    assert!(eventually(T, || injected(&w.b).iter().any(|a| matches!(a, Action::Button { down: true, .. }))).await);

    // B leaves home; its VPN keeps it connected to the hub. The VPN address
    // is not in the allowed block, so B is away.
    w.b.net.set([ip("192.168.1.50"), ip("10.8.0.5"), ip("fe80::1")]);
    assert!(w.b.agent.wait(T, |s| s.away).await);
    assert!(w.b.agent.status().connected, "away nodes stay connected");
    assert!(w.a.agent.wait(T, |s| s.controlling.is_none()).await, "A gets its devices back");
    assert!(!w.a.sim.capturing());
    assert!(w.b.agent.wait(T, |s| s.controlled_by.is_empty()).await);
    assert!(eventually(T, || injected(&w.b).contains(&Action::Left)).await);
    // Physical key/button still held on A: their releases are not forwarded.
    w.a.sim.emit(key(KEY_A, false));
    w.a.sim.emit(button(false));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let got_b = w.b.sim.take_injected();
    assert_balanced(&got_b);

    // The dashboard shows why.
    assert!(wait_status(&w.hub, idb, "away").await);
    let v = node_view(&w.hub, idb).await;
    assert!(v["local_addrs"].as_array().unwrap().contains(&json!("192.168.1.50")), "{v}");
    assert!(w.hub.hub.lock().focus.is_empty());

    // A cannot cross into B (B is skipped like an offline node).
    assert!(w.a.agent.wait(T, |s| s.online_peers.is_empty()).await);
    let ends = w.a.sim.end_captures();
    w.a.sim.push_edge(Side::Right, 1919, 540);
    assert!(eventually(T, || w.a.sim.end_captures() > ends).await, "crossing refused");
    assert_eq!(w.a.agent.status().controlling, None);
    // B cannot cross out: no edges at all while away.
    assert!(eventually(T, || w.b.sim.edges().is_empty()).await);
    w.b.sim.push_edge(Side::Left, 0, 540);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(w.b.agent.status().controlling, None);
    // No clipboard either way.
    w.b.sim.copy(ClipContent::Text("from away".into()));
    w.a.sim.copy(ClipContent::Text("from home".into()));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(w.a.sim.clipboard(), Some(ClipContent::Text("from home".into())));
    assert_eq!(w.b.sim.clipboard(), Some(ClipContent::Text("from away".into())));
    assert!(w.b.sim.take_injected().is_empty(), "nothing injected into an away node");

    // Back home: everything resumes without a restart.
    w.b.net.set([ip("10.10.15.11")]);
    assert!(w.b.agent.wait(T, |s| !s.away).await);
    assert!(wait_status(&w.hub, idb, "online").await);
    assert!(eventually(T, || !w.b.sim.edges().is_empty()).await);
    assert!(w.a.agent.wait(T, |s| s.online_peers == vec![idb]).await);
    w.a.sim.push_edge(Side::Right, 1919, 540);
    assert!(w.a.agent.wait(T, |s| s.controlling == Some(idb)).await, "A crosses into B again");
    w.a.sim.copy(ClipContent::Text("shared again".into()));
    assert!(eventually(T, || w.b.sim.clipboard() == Some(ClipContent::Text("shared again".into()))).await);

    // Going away while *controlling* another machine: B drives A, holds a
    // key, then B's network changes.
    w.a.sim.emit(LocalEvent::Motion { dx: -3000.0, dy: 0.0 });
    assert!(w.a.agent.wait(T, |s| s.controlling.is_none()).await);
    w.a.sim.take_injected();
    w.b.sim.push_edge(Side::Left, 0, 540);
    assert!(w.b.agent.wait(T, |s| s.controlling == Some(ida)).await);
    w.b.sim.emit(key(keys::LEFT_SHIFT, true));
    assert!(eventually(T, || injected(&w.a).contains(&Action::Key { usage: keys::LEFT_SHIFT, down: true })).await);
    w.b.net.set([ip("172.20.0.4")]);
    assert!(w.b.agent.wait(T, |s| s.away && s.controlling.is_none()).await);
    assert!(!w.b.sim.capturing(), "B's own pointer is back");
    assert!(eventually(T, || injected(&w.a).contains(&Action::Left)).await);
    assert_balanced(&w.a.sim.take_injected());
    w.hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crossing_key_gates_both_directions_without_stuck_keys() {
    let w = pair().await;
    let idb = w.idb;
    let (code, msg) = set_global(&w.hub, json!({ "crossing_key": "Ctrl+Nope" })).await;
    assert_eq!(code, 400, "{msg}");
    let (code, _) = set_global(&w.hub, json!({ "crossing_key": "Ctrl" })).await;
    assert_eq!(code, 200);
    assert!(eventually(T, || w.a.sim.crossing_key().is_some()).await, "backend told about the combo");
    assert!(w.a.agent.wait(T, |s| s.settings.as_ref().is_some_and(|s| s.crossing_key == "Ctrl")).await);

    // Without the key the pointer stays (refused at every push).
    for _ in 0..5 {
        w.a.sim.push_edge(Side::Right, 1919, 540);
    }
    assert!(eventually(T, || w.a.sim.end_captures() >= 5).await);
    assert_eq!(w.a.agent.status().controlling, None);
    assert!(!w.a.sim.capturing());

    // Already pushing against the edge; pressing Ctrl crosses on the next
    // push, without moving away and back.
    w.a.sim.key(keys::RIGHT_CTRL, true);
    w.a.sim.push_edge(Side::Right, 1919, 540);
    assert!(w.a.agent.wait(T, |s| s.controlling == Some(idb)).await);
    w.a.sim.emit(key(KEY_A, true));
    w.a.sim.emit(key(KEY_A, false));
    // Ctrl released while on B: it was never pressed there, not forwarded.
    w.a.sim.key(keys::RIGHT_CTRL, false);

    // Coming back also needs the key (B inherits Ctrl): without it the
    // virtual cursor stays at B's left edge.
    w.a.sim.emit(LocalEvent::Motion { dx: -3000.0, dy: 0.0 });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(w.a.agent.status().controlling, Some(idb), "blocked at B's edge");
    assert!(eventually(T, || injected(&w.b).iter().any(|a| matches!(a, Action::MoveAbs { x: 0, .. }))).await);
    // Ctrl pressed while on B is a normal key there; holding it lets the
    // pointer come back, and B gets the matching release.
    w.a.sim.key(keys::LEFT_CTRL, true);
    w.a.sim.emit(LocalEvent::Motion { dx: -10.0, dy: 0.0 });
    assert!(w.a.agent.wait(T, |s| s.controlling.is_none()).await);
    assert!(eventually(T, || injected(&w.b).contains(&Action::Left)).await);
    w.a.sim.key(keys::LEFT_CTRL, false);
    let got = w.b.sim.take_injected();
    assert_balanced(&got);
    assert_eq!(
        keys_of(&got),
        vec![(KEY_A, true), (KEY_A, false), (keys::LEFT_CTRL, true), (keys::LEFT_CTRL, false)],
        "the crossing key's own press/release never reached B"
    );
    assert!(!keys_of(&got).iter().any(|(k, _)| *k == keys::RIGHT_CTRL));

    // After returning, a stale key must not count: the next crossing needs
    // the key again.
    let ends = w.a.sim.end_captures();
    w.a.sim.push_edge(Side::Right, 1919, 540);
    assert!(eventually(T, || w.a.sim.end_captures() > ends).await);
    assert_eq!(w.a.agent.status().controlling, None);

    // Clearing the setting restores free crossing.
    let (code, _) = set_global(&w.hub, json!({ "crossing_key": "" })).await;
    assert_eq!(code, 200);
    assert!(eventually(T, || w.a.sim.crossing_key().is_none()).await);
    w.a.sim.push_edge(Side::Right, 1919, 540);
    assert!(w.a.agent.wait(T, |s| s.controlling == Some(idb)).await);
    w.hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wayland_like_edge_hold() {
    let w = pair().await;
    let idb = w.idb;
    w.a.sim.set_wayland_like(true);
    let (code, _) = set_global(&w.hub, json!({ "crossing_key": "ScrollLock" })).await;
    assert_eq!(code, 200);
    assert!(eventually(T, || w.a.sim.crossing_key().is_some()).await);
    let scroll = flow_keymap::usage_from_name("ScrollLock").unwrap();

    // The barrier fires (capture active): held at the edge, not crossed.
    w.a.sim.push_edge(Side::Right, 1920, 540);
    assert!(w.a.agent.wait(T, |s| s.holding_edge).await);
    assert!(w.a.sim.capturing());
    // Pushing on keeps the hold (no release/re-activation churn).
    let ends = w.a.sim.end_captures();
    for _ in 0..5 {
        w.a.sim.emit(LocalEvent::Motion { dx: 4.0, dy: 0.0 });
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(w.a.sim.end_captures(), ends);
    assert!(w.a.agent.status().holding_edge);
    // Pressing the key while pushing crosses.
    w.a.sim.key(scroll, true);
    assert!(w.a.agent.wait(T, |s| s.controlling == Some(idb) && !s.holding_edge).await);
    assert!(w.a.sim.capturing(), "the barrier's capture becomes the remote session");
    w.a.sim.emit(key(KEY_A, true));
    w.a.sim.emit(key(KEY_A, false));
    w.a.sim.key(scroll, false);
    w.a.sim.key(scroll, true); // held again to come back (B inherits ScrollLock)
    w.a.sim.emit(LocalEvent::Motion { dx: -3000.0, dy: 0.0 });
    assert!(w.a.agent.wait(T, |s| s.controlling.is_none()).await);
    w.a.sim.key(scroll, false);
    assert!(eventually(T, || injected(&w.b).contains(&Action::Left)).await);
    let got = w.b.sim.take_injected();
    assert_balanced(&got);
    // ScrollLock pressed on B (while there) was forwarded with its release;
    // the press that crossed was not.
    assert_eq!(keys_of(&got), vec![(KEY_A, true), (KEY_A, false), (scroll, true), (scroll, false)]);

    // Moving back inward gives the pointer back where the motion takes it.
    w.a.sim.push_edge(Side::Right, 1920, 540);
    assert!(w.a.agent.wait(T, |s| s.holding_edge).await);
    w.a.sim.emit(LocalEvent::Motion { dx: -4.0, dy: 1.0 });
    assert!(w.a.agent.wait(T, |s| !s.holding_edge).await);
    assert!(!w.a.sim.capturing());
    assert_eq!(w.a.sim.last_warp(), Some((1916, 541)));

    // Typing while the pointer rests at the edge ends the hold at once.
    w.a.sim.push_edge(Side::Right, 1920, 300);
    assert!(w.a.agent.wait(T, |s| s.holding_edge).await);
    w.a.sim.emit(key(KEY_A, true));
    assert!(w.a.agent.wait(T, |s| !s.holding_edge).await);
    assert!(!w.a.sim.capturing());
    w.a.sim.key(KEY_A, false); // not capturing any more: goes to the local OS

    // Pausing ends it too.
    w.a.sim.push_edge(Side::Right, 1920, 300);
    assert!(w.a.agent.wait(T, |s| s.holding_edge).await);
    assert!(w.a.agent.wait(Duration::from_secs(2), |s| !s.holding_edge).await);
    assert!(!w.a.sim.capturing());
    assert_eq!(w.a.agent.status().controlling, None);
    assert!(w.b.sim.take_injected().is_empty(), "nothing leaked to B");
    w.hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_node_overrides_beat_the_global_settings() {
    let w = pair().await;
    let (ida, idb) = (w.ida, w.idb);
    let (code, _) = set_global(&w.hub, json!({ "crossing_key": "Ctrl", "allowed_networks": ["10.10.15.0/24"] })).await;
    assert_eq!(code, 200);

    // A: own crossing key "none" → crosses freely although the global key is Ctrl.
    let (code, _) = patch_node(&w.hub, ida, json!({ "crossing_key": "" })).await;
    assert_eq!(code, 204);
    // B: own network list; it moves there and is not away.
    let (code, msg) = patch_node(&w.hub, idb, json!({ "allowed_networks": ["192.168.1.0/24", "bogus"] })).await;
    assert_eq!(code, 400, "{msg}");
    let (code, _) = patch_node(&w.hub, idb, json!({ "allowed_networks": ["192.168.1.0/24"] })).await;
    assert_eq!(code, 204);
    w.b.net.set([ip("192.168.1.50")]);

    assert!(w.a.agent.wait(T, |s| s.settings.as_ref().is_some_and(|s| s.crossing_key.is_empty())).await);
    assert!(
        w.b.agent
            .wait(T, |s| s
                .settings
                .as_ref()
                .is_some_and(|s| s.crossing_key == "Ctrl" && s.allowed_networks == ["192.168.1.0/24"]))
            .await
    );
    tokio::time::sleep(NET_CHECK * 4).await;
    assert!(!w.b.agent.status().away, "B's own list allows 192.168.1.0/24");
    let v = node_view(&w.hub, idb).await;
    assert_eq!(v["allowed_networks"], json!(["192.168.1.0/24"]));
    assert_eq!(v["effective_crossing_key"], json!("Ctrl"));
    assert_eq!(node_view(&w.hub, ida).await["crossing_key"], json!(""));

    // A crosses without any key; coming back out of B needs B's key (Ctrl).
    assert!(w.a.agent.wait(T, |s| s.online_peers == vec![idb]).await);
    w.a.sim.push_edge(Side::Right, 1919, 540);
    assert!(w.a.agent.wait(T, |s| s.controlling == Some(idb)).await);
    w.a.sim.emit(LocalEvent::Motion { dx: -3000.0, dy: 0.0 });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(w.a.agent.status().controlling, Some(idb));
    w.a.sim.key(keys::LEFT_CTRL, true);
    w.a.sim.emit(LocalEvent::Motion { dx: -5.0, dy: 0.0 });
    assert!(w.a.agent.wait(T, |s| s.controlling.is_none()).await);
    w.a.sim.key(keys::LEFT_CTRL, false);

    // B pushes at its own edge: its (inherited) Ctrl is required.
    let ends = w.b.sim.end_captures();
    w.b.sim.push_edge(Side::Left, 0, 540);
    assert!(eventually(T, || w.b.sim.end_captures() > ends).await);
    assert_eq!(w.b.agent.status().controlling, None);
    w.b.sim.key(keys::LEFT_CTRL, true);
    w.b.sim.push_edge(Side::Left, 0, 540);
    assert!(w.b.agent.wait(T, |s| s.controlling == Some(ida)).await);

    // Back to inherit: B is not on the global network → away.
    let (code, _) = patch_node(&w.hub, idb, json!({ "allowed_networks": null })).await;
    assert_eq!(code, 204);
    assert!(w.b.agent.wait(T, |s| s.away && s.controlling.is_none()).await);
    assert!(wait_status(&w.hub, idb, "away").await);
    assert!(eventually(T, || injected(&w.a).contains(&Action::Left)).await);
    assert_balanced(&w.a.sim.take_injected());
    w.hub.shutdown().await;
}
