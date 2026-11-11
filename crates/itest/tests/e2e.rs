//! End-to-end: hub + simulated agents on localhost, real QUIC everywhere.

use flow_agent::backend::LocalEvent;
use flow_itest::*;
use flow_keymap::keys;
use flow_net::link::Action;
use flow_net::testutil::{free_udp_port, UdpProxy};
use flow_net::token::JoinToken;
use flow_proto::{Button, ClipContent, NodeId, PathKind};
use flow_server::HubHandle;
use std::time::Duration;

const T: Duration = Duration::from_secs(5);
const KEY_A: u32 = keys::A;
const KEY_B: u32 = keys::A + 1;

fn key(usage: u32, down: bool) -> LocalEvent {
    LocalEvent::Key { usage, down }
}

fn set_probe_interval(hub: &HubHandle, ms: u32) {
    let mut st = hub.hub.lock();
    st.p.settings.probe_interval_ms = ms;
    st.broadcast_settings();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_requires_valid_token() {
    init_logs();
    let hub = start_hub().await;
    let token = hub.join_token();

    let a = start_agent(&hub, AgentSpec::new(&token)).await;
    assert!(a.agent.wait(T, |s| s.connected && s.node_id.is_some()).await, "valid token enrols");
    let id = a.agent.status().node_id.unwrap();
    assert_eq!(hub.hub.lock().p.nodes.len(), 1);

    // Right hub fingerprint, wrong secret → rejected by the hub.
    let bad = JoinToken::new(JoinToken::random_secret(), &hub.fingerprint()).encode();
    let b = start_agent(&hub, AgentSpec::new(&bad)).await;
    assert!(b.agent.wait(T, |s| s.rejected.is_some()).await, "bad token is rejected");
    assert!(!b.agent.status().connected);

    // Token for another hub → TLS pin fails, the secret is never sent.
    let other = flow_net::Identity::generate("other-hub").unwrap();
    let foreign = JoinToken::new(hub.hub.lock().p.join_secret_bytes(), &other.fingerprint).encode();
    let c = start_agent(&hub, AgentSpec::new(&foreign)).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!c.agent.status().connected, "agent must not connect to a hub whose fingerprint differs");
    assert_eq!(hub.hub.lock().p.nodes.len(), 1, "no enrolment with bad tokens");

    // Restarting the enrolled agent works without a token (certificate is the credential).
    let dir = a.state_dir.clone();
    a.agent.shutdown().await;
    let a2 = start_agent(&hub, AgentSpec { token: None, state_dir: Some(dir), ..AgentSpec::new("") }).await;
    assert!(a2.agent.wait(T, |s| s.connected).await);
    assert_eq!(a2.agent.status().node_id, Some(id), "same identity, same node");
    assert_eq!(hub.hub.lock().p.nodes.len(), 1);

    // Dashboard API needs the admin token.
    let web = hub.http_addr.unwrap();
    assert_eq!(http(web, "GET", "/api/state", None, None).await.0, 401);
    assert_eq!(http(web, "GET", "/api/state", Some("nope"), None).await.0, 401);
    let (code, body) = http(web, "GET", "/api/state", Some(ADMIN), None).await;
    assert_eq!(code, 200);
    assert!(body.contains(&hub.join_token()));

    // Regenerating the token: the old one stops working for new agents.
    let (code, _) = http(web, "POST", "/api/join-token/regenerate", Some(ADMIN), None).await;
    assert_eq!(code, 200);
    let d = start_agent(&hub, AgentSpec::new(&token)).await;
    assert!(d.agent.wait(T, |s| s.rejected.is_some()).await, "old token rejected after regeneration");
    let e = start_agent(&hub, AgentSpec::new(&hub.join_token())).await;
    assert!(e.agent.wait(T, |s| s.connected).await, "new token works");

    // Revocation: the node is removed, the agent learns it and does not
    // silently re-enrol with the token from its config.
    let eid = e.agent.status().node_id.unwrap();
    let (code, _) = http(web, "DELETE", &format!("/api/nodes/{}", eid.0), Some(ADMIN), None).await;
    assert_eq!(code, 204);
    assert!(e.agent.wait(T, |s| s.revoked).await);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!hub.hub.lock().p.nodes.contains_key(&eid.0));
    hub.shutdown().await;
}

/// Three machines side by side: A | B | C. B's direct (P2P) address goes
/// through a UDP proxy that the test can block.
struct World {
    hub: HubHandle,
    a: TestAgent,
    b: TestAgent,
    c: TestAgent,
    ids: [NodeId; 3],
    proxy: UdpProxy,
}

async fn world() -> World {
    init_logs();
    let hub = start_hub().await;
    set_probe_interval(&hub, 300);
    let token = hub.join_token();
    let a = start_agent(&hub, AgentSpec::new(&token)).await;
    assert!(a.agent.wait(T, |s| s.connected).await);
    let b_port = free_udp_port();
    let proxy = UdpProxy::start(format!("127.0.0.1:{b_port}").parse().unwrap()).await.unwrap();
    let b = start_agent(
        &hub,
        AgentSpec {
            p2p_port: b_port,
            advertise: vec![proxy.addr],
            monitors: monitor(2560, 1440),
            ..AgentSpec::new(&token)
        },
    )
    .await;
    assert!(b.agent.wait(T, |s| s.connected).await);
    let c = start_agent(&hub, AgentSpec::new(&token)).await;
    assert!(c.agent.wait(T, |s| s.connected).await);
    let ids = [a.agent.status().node_id.unwrap(), b.agent.status().node_id.unwrap(), c.agent.status().node_id.unwrap()];
    for x in [&a, &b, &c] {
        assert!(x.agent.wait(T, |s| s.online_peers.len() == 2 && s.layout_nodes == 3).await, "everyone sees everyone");
    }
    World { hub, a, b, c, ids, proxy }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn focus_input_failover_and_clipboard() {
    let w = world().await;
    let [ida, idb, idc] = w.ids;

    // Layout: A (1920x1080) | B (2560x1440) | C (1920x1080), placed in join order.
    let edges = w.a.sim.edges();
    assert_eq!(edges.len(), 1, "{edges:?}");
    assert_eq!(edges[0].to, idb);

    // P2P is used by default.
    assert!(w.a.agent.wait_link_path(idb, PathKind::P2p, T).await, "A→B direct");
    assert!(w.a.agent.wait_link_path(idc, PathKind::P2p, T).await, "A→C direct");

    // --- edge crossing A → B, input arrives at B only -------------------
    w.a.sim.emit(LocalEvent::EdgeHit { side: flow_layout::Side::Right, x: 1919, y: 540 });
    assert!(w.a.agent.wait(T, |s| s.controlling == Some(idb)).await);
    assert!(w.a.sim.capturing());
    w.a.sim.emit(key(KEY_A, true));
    w.a.sim.emit(key(KEY_A, false));
    w.a.sim.emit(LocalEvent::Button { button: Button::Left, down: true });
    w.a.sim.emit(LocalEvent::Button { button: Button::Left, down: false });
    w.a.sim.emit(LocalEvent::Motion { dx: 10.0, dy: 0.0 });
    let got = collect_until(&w.b.sim, T, |v| v.iter().any(|a| matches!(a, Action::MoveAbs { .. }))).await;
    assert_eq!(
        got,
        vec![
            // 50 % down A's edge → 50 % down B's edge (proportional mapping).
            Action::Entered { x: 0, y: 720 },
            Action::Key { usage: KEY_A, down: true },
            Action::Key { usage: KEY_A, down: false },
            Action::Button { button: Button::Left, down: true },
            Action::Button { button: Button::Left, down: false },
            Action::MoveAbs { x: 10, y: 720, dx: 10, dy: 0 },
        ]
    );
    assert!(w.b.agent.wait(T, |s| s.controlled_by == vec![ida]).await);
    assert!(w.c.sim.take_injected().is_empty(), "C got nothing");
    assert!(w.a.sim.take_injected().is_empty(), "A injects nothing into itself");
    let hub_focus = w.hub.hub.lock().focus.get(&ida).copied();
    assert!(eventually(T, || w.hub.hub.lock().focus.get(&ida) == Some(&idb)).await, "hub tracks focus ({hub_focus:?})");

    // --- B → C while Shift is held: release-all on the switch ------------
    w.a.sim.emit(key(keys::LEFT_SHIFT, true));
    w.a.sim.emit(LocalEvent::Motion { dx: 3000.0, dy: 0.0 });
    assert!(w.a.agent.wait(T, |s| s.controlling == Some(idc)).await);
    let got_b = collect_until(&w.b.sim, T, |v| v.contains(&Action::Left)).await;
    assert_eq!(
        got_b,
        vec![
            Action::Key { usage: keys::LEFT_SHIFT, down: true },
            Action::Key { usage: keys::LEFT_SHIFT, down: false },
            Action::Left
        ]
    );
    let got_c = collect_until(&w.c.sim, T, |v| !v.is_empty()).await;
    assert_eq!(got_c, vec![Action::Entered { x: 0, y: 540 }]);
    // Shift is still physically held but was never pressed on C: its
    // release must not be forwarded.
    w.a.sim.emit(key(keys::LEFT_SHIFT, false));

    // --- back through B to A: pointer warps to the entry point ----------
    w.a.sim.emit(LocalEvent::Motion { dx: -50.0, dy: 0.0 });
    assert!(w.a.agent.wait(T, |s| s.controlling == Some(idb)).await);
    w.a.sim.emit(LocalEvent::Motion { dx: -5000.0, dy: 0.0 });
    assert!(w.a.agent.wait(T, |s| s.controlling.is_none()).await);
    assert!(!w.a.sim.capturing());
    assert_eq!(w.a.sim.last_warp(), Some((1919, 540)));
    let got_c = collect_until(&w.c.sim, T, |v| v.contains(&Action::Left)).await;
    assert_eq!(got_c, vec![Action::Left], "C: shift-up never forwarded, just Left");
    assert!(eventually(T, || !w.hub.hub.lock().focus.contains_key(&ida)).await);
    w.b.sim.take_injected();

    // --- P2P blocked → relay, no loss, no duplicates; then back to P2P ---
    w.proxy.set_blocked(true);
    w.a.sim.emit(LocalEvent::EdgeHit { side: flow_layout::Side::Right, x: 1919, y: 540 });
    assert!(w.a.agent.wait(T, |s| s.controlling == Some(idb)).await);
    for i in 0..5 {
        w.a.sim.emit(key(KEY_B, true));
        w.a.sim.emit(key(KEY_B, false));
        if i == 2 {
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }
    assert!(w.a.agent.wait_link_path(idb, PathKind::Relay, T).await, "falls back to relay");
    let mut expected = vec![Action::Entered { x: 0, y: 720 }];
    for _ in 0..5 {
        expected.push(Action::Key { usage: KEY_B, down: true });
        expected.push(Action::Key { usage: KEY_B, down: false });
    }
    let got = collect_until(&w.b.sim, T, |v| v.len() >= expected.len()).await;
    assert_eq!(got, expected, "every event exactly once over the relay");
    assert!(
        eventually(Duration::from_secs(6), || w
            .hub
            .hub
            .lock()
            .pairs
            .get(&(ida, idb))
            .is_some_and(|p| p.path == PathKind::Relay))
        .await,
        "dashboard shows the relay path"
    );
    // Held key across the return switch stays consistent.
    w.a.sim.emit(key(KEY_A, true));
    w.proxy.set_blocked(false);
    assert!(w.a.agent.wait_link_path(idb, PathKind::P2p, Duration::from_secs(10)).await, "returns to P2P");
    w.a.sim.emit(key(KEY_A, false));
    let got = collect_until(&w.b.sim, T, |v| v.len() >= 2).await;
    assert_eq!(got, vec![Action::Key { usage: KEY_A, down: true }, Action::Key { usage: KEY_A, down: false }]);
    let st = w.a.agent.link_status(idb).unwrap();
    assert!(st.p2p_rtt_us.is_some() && st.relay_rtt_us.is_some(), "{st:?}");
    assert_eq!(st.unacked, 0);

    // --- emergency hotkey -------------------------------------------------
    w.a.sim.emit(key(keys::LEFT_CTRL, true));
    w.a.sim.emit(key(keys::LEFT_ALT, true));
    w.a.sim.emit(key(keys::RIGHT_SHIFT, true));
    w.a.sim.emit(key(keys::ESCAPE, true));
    assert!(w.a.agent.wait(T, |s| s.controlling.is_none()).await, "hotkey returns control");
    assert!(!w.a.sim.capturing());
    let got = collect_until(&w.b.sim, T, |v| v.contains(&Action::Left)).await;
    assert!(!got.iter().any(|a| matches!(a, Action::Key { usage, .. } if *usage == keys::ESCAPE)), "{got:?}");
    for k in [keys::LEFT_CTRL, keys::LEFT_ALT, keys::RIGHT_SHIFT] {
        let downs = got.iter().filter(|a| **a == Action::Key { usage: k, down: true }).count();
        let ups = got.iter().filter(|a| **a == Action::Key { usage: k, down: false }).count();
        assert_eq!((downs, ups), (1, 1), "modifier {k:#x} pressed and released on B: {got:?}");
    }

    // --- clipboard -------------------------------------------------------
    w.a.sim.copy(ClipContent::Text("hello from A".into()));
    let want = Some(ClipContent::Text("hello from A".into()));
    assert!(eventually(T, || w.b.sim.clipboard() == want && w.c.sim.clipboard() == want).await, "text synced");
    let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
    w.c.sim.copy(ClipContent::Image { png: png.clone() });
    let want = Some(ClipContent::Image { png });
    assert!(eventually(T, || w.a.sim.clipboard() == want && w.b.sim.clipboard() == want).await, "image synced");
    // Images disabled in the dashboard settings → not synced any more.
    let web = w.hub.http_addr.unwrap();
    let (_, s) = http(web, "GET", "/api/settings", Some(ADMIN), None).await;
    let mut s: serde_json::Value = serde_json::from_str(&s).unwrap();
    s["clipboard"]["images"] = false.into();
    let (code, _) = http(web, "PUT", "/api/settings", Some(ADMIN), Some(&s.to_string())).await;
    assert_eq!(code, 200);
    assert!(w.b.agent.wait(T, |st| st.settings.as_ref().is_some_and(|s| !s.clipboard.images)).await, "pushed live");
    w.c.sim.copy(ClipContent::Image { png: vec![9, 9, 9] });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_ne!(w.a.sim.clipboard(), Some(ClipContent::Image { png: vec![9, 9, 9] }));

    // --- forced relay from the settings page ------------------------------
    s["path_policy"] = "force_relay".into();
    http(web, "PUT", "/api/settings", Some(ADMIN), Some(&s.to_string())).await;
    assert!(w.a.agent.wait_link_path(idb, PathKind::Relay, T).await, "force relay applied live");

    // --- disabling a node removes it from the layout ---------------------
    let (code, _) =
        http(web, "PATCH", &format!("/api/nodes/{}", idc.0), Some(ADMIN), Some(r#"{"enabled":false}"#)).await;
    assert_eq!(code, 204);
    assert!(w.a.agent.wait(T, |s| s.layout_nodes == 2 && s.online_peers.len() == 1).await);
    w.hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p2p_keeps_working_without_hub_and_target_loss_releases() {
    let w = world().await;
    let [ida, idb, _idc] = w.ids;
    assert!(w.a.agent.wait_link_path(idb, PathKind::P2p, T).await);

    // A controls B and holds a key; then B goes away → A returns home.
    w.a.sim.emit(LocalEvent::EdgeHit { side: flow_layout::Side::Right, x: 1919, y: 100 });
    assert!(w.a.agent.wait(T, |s| s.controlling == Some(idb)).await);
    w.a.sim.emit(key(KEY_A, true));
    let got = collect_until(&w.b.sim, T, |v| v.len() >= 2).await;
    assert_eq!(got[1], Action::Key { usage: KEY_A, down: true });

    // Kill the hub: the existing direct link keeps working.
    w.hub.shutdown().await;
    assert!(w.a.agent.wait(T, |s| !s.connected).await);
    w.a.sim.emit(key(KEY_A, false));
    let got = collect_until(&w.b.sim, T, |v| !v.is_empty()).await;
    assert_eq!(got, vec![Action::Key { usage: KEY_A, down: false }], "P2P works while the hub is down");

    // B holds a key from A, then A disappears: B's watchdog releases it.
    w.a.sim.emit(key(KEY_B, true));
    let got = collect_until(&w.b.sim, T, |v| !v.is_empty()).await;
    assert_eq!(got, vec![Action::Key { usage: KEY_B, down: true }]);
    w.a.agent.shutdown().await;
    let got = collect_until(&w.b.sim, Duration::from_secs(6), |v| v.contains(&Action::Left)).await;
    assert_eq!(got, vec![Action::Key { usage: KEY_B, down: false }, Action::Left], "heartbeat timeout releases");
    assert!(w.b.agent.wait(T, |s| s.controlled_by.is_empty()).await);
    let _ = ida;
}
