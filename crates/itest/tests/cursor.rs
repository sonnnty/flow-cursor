//! Where the local cursor goes while a machine controls another one
//! (cursor-park setting), end to end: hub + simulated agents on
//! localhost, settings through the dashboard API. The simulated backend
//! records parks, hiding and warps; its hiding is modelled as outliving
//! the process, the worst case for the startup restore.

use flow_agent::backend::sim::SimControl;
use flow_agent::backend::LocalEvent;
use flow_agent::park::Park;
use flow_itest::*;
use flow_keymap::keys;
use flow_layout::Side;
use flow_proto::{CursorPark, CursorSupport, NodeId};
use flow_server::HubHandle;
use serde_json::{json, Value};
use std::time::Duration;

const T: Duration = Duration::from_secs(5);
/// Centre of A's only monitor (1920x1080).
const CENTRE: (i32, i32) = (960, 540);

/// A (1920x1080) | B (1920x1080), A's right edge leads to B.
struct Pair {
    hub: HubHandle,
    a: TestAgent,
    b: TestAgent,
    ida: NodeId,
    idb: NodeId,
}

async fn pair(prepare_a: Option<fn(&SimControl)>) -> Pair {
    init_logs();
    let hub = start_hub().await;
    let token = hub.join_token();
    let a = start_agent(&hub, AgentSpec { prepare: prepare_a, ..AgentSpec::new(&token) }).await;
    assert!(a.agent.wait(T, |s| s.connected).await);
    let b = start_agent(&hub, AgentSpec { addrs: vec![ip("10.10.15.11")], ..AgentSpec::new(&token) }).await;
    assert!(b.agent.wait(T, |s| s.connected).await);
    let (ida, idb) = (a.agent.status().node_id.unwrap(), b.agent.status().node_id.unwrap());
    for x in [&a, &b] {
        assert!(x.agent.wait(T, |s| s.online_peers.len() == 1 && s.layout_nodes == 2).await);
    }
    assert!(a.agent.wait_link_path(idb, flow_proto::PathKind::P2p, T).await);
    assert!(b.agent.wait_link_path(ida, flow_proto::PathKind::P2p, T).await);
    Pair { hub, a, b, ida, idb }
}

async fn set_global(hub: &HubHandle, changes: Value) {
    let web = hub.http_addr.unwrap();
    let (code, body) = http(web, "GET", "/api/settings", Some(ADMIN), None).await;
    assert_eq!(code, 200);
    let mut s: Value = serde_json::from_str(&body).unwrap();
    for (k, v) in changes.as_object().unwrap() {
        s[k] = v.clone();
    }
    let (code, body) = http(web, "PUT", "/api/settings", Some(ADMIN), Some(&s.to_string())).await;
    assert_eq!(code, 200, "{body}");
}

async fn patch_node(hub: &HubHandle, id: NodeId, body: Value) -> u16 {
    http(hub.http_addr.unwrap(), "PATCH", &format!("/api/nodes/{}", id.0), Some(ADMIN), Some(&body.to_string())).await.0
}

async fn node_view(hub: &HubHandle, id: NodeId) -> Value {
    let (code, body) = http(hub.http_addr.unwrap(), "GET", "/api/state", Some(ADMIN), None).await;
    assert_eq!(code, 200);
    let st: Value = serde_json::from_str(&body).unwrap();
    st["nodes"].as_array().unwrap().iter().find(|n| n["id"] == json!(id.0)).cloned().unwrap()
}

/// Waits until the agent runs with `want` as its effective setting.
async fn wait_park(t: &TestAgent, want: CursorPark) {
    assert!(
        t.agent.wait(T, |s| s.settings.as_ref().is_some_and(|s| s.cursor_park == want)).await,
        "agent never got cursor_park = {want:?}"
    );
}

/// A's pointer pushes against its right edge at y and crosses to B.
async fn cross(w: &Pair, y: i32) {
    w.a.sim.push_edge(Side::Right, 1919, y);
    assert!(w.a.agent.wait(T, |s| s.controlling == Some(w.idb)).await, "A controls B");
    assert!(w.a.sim.capturing());
}

/// Control is back on A, the cursor is visible and not parked.
async fn assert_back_and_shown(w: &Pair, how: &str) {
    assert!(w.a.agent.wait(T, |s| s.controlling.is_none() && s.cursor_parked.is_none()).await, "{how}: A is back");
    assert!(!w.a.sim.capturing(), "{how}: capture ended");
    assert!(!w.a.sim.cursor_hidden(), "{how}: cursor shown again");
}

/// Waits until A can reach B again (online, link not down).
async fn wait_reachable(a: &TestAgent, idb: NodeId) {
    assert!(a.agent.wait(T, |s| s.online_peers == vec![idb]).await, "B online for A");
    assert!(
        eventually(T, || a.agent.link_status(idb).is_some_and(|l| l.path != flow_proto::PathKind::Down)).await,
        "link to B up"
    );
}

fn key(usage: u32, down: bool) -> LocalEvent {
    LocalEvent::Key { usage, down }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_option_parks_on_leaving_and_returns_at_the_mapped_edge() {
    let w = pair(None).await;
    // The default is today's behaviour.
    wait_park(&w.a, CursorPark::Edge).await;
    assert_eq!(node_view(&w.hub, w.ida).await["caps"]["cursor"], json!({ "center": true, "hide": true }));

    for (opt, park) in [
        (CursorPark::Edge, None),
        (CursorPark::Center, Some(Park { at: CENTRE, hide: false })),
        (CursorPark::CenterHide, Some(Park { at: CENTRE, hide: true })),
    ] {
        set_global(&w.hub, json!({ "cursor_park": opt })).await;
        wait_park(&w.a, opt).await;
        let parks_before = w.a.sim.parks().len();

        cross(&w, 300).await;
        assert_eq!(w.a.agent.status().cursor_parked, park, "{opt:?}");
        match park {
            None => {
                assert_eq!(w.a.sim.parks().len(), parks_before, "{opt:?}: nothing asked of the backend");
                assert_eq!(w.a.sim.pointer(), (1919, 300), "{opt:?}: stays at the edge");
            }
            Some(p) => {
                assert_eq!(w.a.sim.parks().last(), Some(&p), "{opt:?}");
                assert_eq!(w.a.sim.pointer(), CENTRE, "{opt:?}: centre of the monitor it left from");
            }
        }
        assert_eq!(w.a.sim.cursor_hidden(), opt == CursorPark::CenterHide, "{opt:?}");

        // Move down on B, then back over B's left edge: A's cursor enters at
        // the mapped edge position, not at the centre, and is visible.
        w.a.sim.emit(LocalEvent::Motion { dx: 5.0, dy: 100.0 });
        w.a.sim.emit(LocalEvent::Motion { dx: -50.0, dy: 0.0 });
        assert_back_and_shown(&w, &format!("{opt:?} over the edge")).await;
        assert_eq!(w.a.sim.last_warp(), Some((1919, 400)), "{opt:?}: enters at the mapped edge position");
        assert_eq!(w.a.sim.pointer(), (1919, 400));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_machine_override_beats_the_global_value() {
    let w = pair(None).await;
    set_global(&w.hub, json!({ "cursor_park": "center_hide" })).await;
    wait_park(&w.a, CursorPark::CenterHide).await;

    // Own value for A: stay at the edge.
    assert_eq!(patch_node(&w.hub, w.ida, json!({ "cursor_park": "edge" })).await, 204);
    assert_eq!(patch_node(&w.hub, w.ida, json!({ "cursor_park": "corner" })).await, 422, "unknown choice");
    wait_park(&w.a, CursorPark::Edge).await;
    wait_park(&w.b, CursorPark::CenterHide).await;
    let v = node_view(&w.hub, w.ida).await;
    assert_eq!(
        (&v["cursor_park"], &v["effective_cursor_park"], &v["applied_cursor_park"]),
        (&json!("edge"), &json!("edge"), &json!("edge"))
    );
    cross(&w, 500).await;
    assert_eq!(w.a.agent.status().cursor_parked, None);
    assert!(!w.a.sim.cursor_hidden());
    assert_eq!(w.a.sim.pointer(), (1919, 500));
    w.a.sim.emit(LocalEvent::Motion { dx: -50.0, dy: 0.0 });
    assert_back_and_shown(&w, "override edge").await;

    // Own value: centre without hiding.
    assert_eq!(patch_node(&w.hub, w.ida, json!({ "cursor_park": "center" })).await, 204);
    wait_park(&w.a, CursorPark::Center).await;
    cross(&w, 500).await;
    assert_eq!(w.a.agent.status().cursor_parked, Some(Park { at: CENTRE, hide: false }));
    assert!(!w.a.sim.cursor_hidden());
    w.a.sim.emit(LocalEvent::Motion { dx: -50.0, dy: 0.0 });
    assert_back_and_shown(&w, "override centre").await;

    // Back to inherit: the global centre+hide applies again.
    assert_eq!(patch_node(&w.hub, w.ida, json!({ "cursor_park": null })).await, 204);
    wait_park(&w.a, CursorPark::CenterHide).await;
    assert_eq!(node_view(&w.hub, w.ida).await["cursor_park"], Value::Null);
    cross(&w, 500).await;
    assert!(w.a.sim.cursor_hidden());
    w.a.sim.emit(LocalEvent::Motion { dx: -50.0, dy: 0.0 });
    assert_back_and_shown(&w, "inherit").await;
}

fn gnome_like(sim: &SimControl) {
    sim.set_cursor_support(CursorSupport::default());
    sim.set_wayland_like(true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gnome_like_machine_stays_at_the_edge_and_the_dashboard_says_so() {
    let w = pair(Some(gnome_like)).await;
    set_global(&w.hub, json!({ "cursor_park": "center_hide" })).await;
    wait_park(&w.a, CursorPark::CenterHide).await;

    let v = node_view(&w.hub, w.ida).await;
    assert_eq!(v["caps"]["cursor"], json!({ "center": false, "hide": false }));
    assert_eq!(v["effective_cursor_park"], json!("center_hide"));
    assert_eq!(v["applied_cursor_park"], json!("edge"), "the closest it can do");
    assert_eq!(node_view(&w.hub, w.idb).await["applied_cursor_park"], json!("center_hide"));

    // The barrier hit captures; the cursor is left at the edge, visible.
    cross(&w, 200).await;
    assert!(w.a.sim.parks().is_empty(), "nothing the backend cannot do is asked");
    assert_eq!(w.a.agent.status().cursor_parked, None);
    assert!(!w.a.sim.cursor_hidden());
    assert_eq!(w.a.sim.pointer(), (1919, 200));
    // Release with the mapped position as the cursor hint.
    w.a.sim.emit(LocalEvent::Motion { dx: 0.0, dy: 300.0 });
    w.a.sim.emit(LocalEvent::Motion { dx: -50.0, dy: 0.0 });
    assert_back_and_shown(&w, "gnome").await;
    assert_eq!(w.a.sim.last_warp(), Some((1919, 500)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_way_back_shows_the_cursor() {
    let w = pair(None).await;
    set_global(&w.hub, json!({ "cursor_park": "center_hide", "allowed_networks": ["10.10.15.0/24"] })).await;
    wait_park(&w.a, CursorPark::CenterHide).await;

    let parked = |w: &Pair| {
        assert!(w.a.sim.cursor_hidden(), "hidden while controlling B");
        assert_eq!(w.a.sim.pointer(), CENTRE);
    };

    // Emergency hotkey: shown where it is (the centre), no warp.
    cross(&w, 300).await;
    parked(&w);
    let warps = w.a.sim.state.lock().unwrap().warps.len();
    for k in [keys::LEFT_CTRL, keys::LEFT_ALT, keys::LEFT_SHIFT, keys::ESCAPE] {
        w.a.sim.emit(key(k, true));
    }
    assert_back_and_shown(&w, "emergency hotkey").await;
    assert_eq!(w.a.sim.pointer(), CENTRE, "stays at the centre");
    assert_eq!(w.a.sim.state.lock().unwrap().warps.len(), warps, "no warp");
    for k in [keys::ESCAPE, keys::LEFT_SHIFT, keys::LEFT_ALT, keys::LEFT_CTRL] {
        w.a.sim.emit(key(k, false));
    }

    // Pause from the tray.
    cross(&w, 300).await;
    parked(&w);
    w.a.agent.set_paused(true);
    assert_back_and_shown(&w, "pause").await;
    w.a.agent.set_paused(false);
    assert!(w.a.agent.wait(T, |s| !s.paused).await);

    // Away: A leaves the allowed network.
    cross(&w, 300).await;
    parked(&w);
    w.a.net.set([ip("192.168.1.50")]);
    assert!(w.a.agent.wait(T, |s| s.away).await);
    assert_back_and_shown(&w, "A away").await;
    w.a.net.set([ip("10.10.15.10")]);
    assert!(w.a.agent.wait(T, |s| !s.away).await);
    wait_reachable(&w.a, w.idb).await;

    // The controlled machine goes away.
    cross(&w, 300).await;
    parked(&w);
    w.b.net.set([ip("192.168.1.51")]);
    assert_back_and_shown(&w, "B away").await;
    // The hub may release A before A hears that B is away.
    assert!(w.a.agent.wait(T, |s| s.online_peers.is_empty()).await);
    w.b.net.set([ip("10.10.15.11")]);
    wait_reachable(&w.a, w.idb).await;

    // The hub releases control (B disabled in the dashboard).
    cross(&w, 300).await;
    parked(&w);
    assert_eq!(patch_node(&w.hub, w.idb, json!({ "enabled": false })).await, 204);
    assert_back_and_shown(&w, "hub release").await;
    assert!(w.a.agent.wait(T, |s| s.online_peers.is_empty()).await);
    assert_eq!(patch_node(&w.hub, w.idb, json!({ "enabled": true })).await, 204);
    wait_reachable(&w.a, w.idb).await;

    // A graceful stop of A's agent (tray restart, flow-agent stop).
    cross(&w, 300).await;
    parked(&w);
    let Pair { hub, a, b, ida, idb } = w;
    let (a_sim, a_dir) = (a.sim.clone(), a.state_dir.clone());
    a.agent.shutdown().await;
    assert!(!a_sim.capturing() && !a_sim.cursor_hidden(), "stop gives devices and cursor back");

    // A restarted agent first undoes hiding a crashed run left behind.
    fn left_hidden(sim: &SimControl) {
        sim.state.lock().unwrap().cursor_hidden = true;
    }
    let a = start_agent(
        &hub,
        AgentSpec { state_dir: Some(a_dir), prepare: Some(left_hidden), ..AgentSpec::new(&hub.join_token()) },
    )
    .await;
    assert!(!a.sim.cursor_hidden(), "restored at start");
    assert_eq!(a.sim.state.lock().unwrap().restores, 1);
    assert!(a.agent.wait(T, |s| s.connected && s.node_id == Some(ida)).await, "same machine");
    wait_reachable(&a, idb).await;
    let w = Pair { hub, a, b, ida, idb };

    // The controlled machine disappears (agent stopped, link lost).
    wait_park(&w.a, CursorPark::CenterHide).await;
    cross(&w, 300).await;
    parked(&w);
    let Pair { a, b, .. } = w;
    b.agent.shutdown().await;
    assert!(a.agent.wait(Duration::from_secs(10), |s| s.controlling.is_none()).await, "disconnect returns control");
    assert!(!a.sim.capturing() && !a.sim.cursor_hidden(), "disconnect: cursor shown");
    assert_eq!(a.sim.pointer(), CENTRE);
}
