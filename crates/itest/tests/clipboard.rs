//! End-to-end integration tests for clipboard synchronization, limits, and gating.

use flow_itest::*;
use flow_proto::ClipContent;
use std::time::Duration;

const T: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_multinode_sync_and_echo_suppression() {
    init_logs();
    let hub = start_hub().await;
    let token = hub.join_token();

    let a = start_agent(&hub, AgentSpec::new(&token)).await;
    let b = start_agent(&hub, AgentSpec::new(&token)).await;
    let c = start_agent(&hub, AgentSpec::new(&token)).await;

    for x in [&a, &b, &c] {
        assert!(x.agent.wait(T, |s| s.connected && s.online_peers.len() == 2).await);
    }

    // 1. A copies text -> B and C receive it, A does not receive duplicate/echo
    let msg1 = ClipContent::Text("multinode test from A".into());
    a.sim.copy(msg1.clone());

    let want1 = Some(msg1);
    assert!(eventually(T, || b.sim.clipboard() == want1 && c.sim.clipboard() == want1).await);
    // A's sim clipboard was set by A, and hub must not echo it back
    assert_eq!(a.sim.clipboard(), want1);

    // 2. B copies PNG image -> A and C receive it
    let png_bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
    let msg2 = ClipContent::Image { png: png_bytes };
    b.sim.copy(msg2.clone());

    let want2 = Some(msg2);
    assert!(eventually(T, || a.sim.clipboard() == want2 && c.sim.clipboard() == want2).await);
    assert_eq!(b.sim.clipboard(), want2);

    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_limits_and_gating_via_hub_settings() {
    init_logs();
    let hub = start_hub().await;
    let web = hub.http_addr.unwrap();
    let token = hub.join_token();

    let a = start_agent(&hub, AgentSpec::new(&token)).await;
    let b = start_agent(&hub, AgentSpec::new(&token)).await;
    assert!(a.agent.wait(T, |s| s.connected).await);
    assert!(b.agent.wait(T, |s| s.connected).await);

    // 1. Text size limit: set max_text_bytes = 10 via settings API
    let (_, s_val) = http(web, "GET", "/api/settings", Some(ADMIN), None).await;
    let mut s: serde_json::Value = serde_json::from_str(&s_val).unwrap();
    s["clipboard"]["max_text_bytes"] = 10.into();
    let (code, _) = http(web, "PUT", "/api/settings", Some(ADMIN), Some(&s.to_string())).await;
    assert_eq!(code, 200);

    // Wait for settings to push to agents
    assert!(b.agent.wait(T, |st| st.settings.as_ref().is_some_and(|s| s.clipboard.max_text_bytes == 10)).await);

    // Under limit (8 bytes) -> synced
    a.sim.copy(ClipContent::Text("short_ok".into()));
    assert!(eventually(T, || b.sim.clipboard() == Some(ClipContent::Text("short_ok".into()))).await);

    // Over limit (20 bytes) -> dropped, B retains previous clipboard
    a.sim.copy(ClipContent::Text("this is too long to sync".into()));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(b.sim.clipboard(), Some(ClipContent::Text("short_ok".into())));

    // 2. Disable text entirely
    s["clipboard"]["text"] = false.into();
    let (code, _) = http(web, "PUT", "/api/settings", Some(ADMIN), Some(&s.to_string())).await;
    assert_eq!(code, 200);
    assert!(b.agent.wait(T, |st| st.settings.as_ref().is_some_and(|s| !s.clipboard.text)).await);

    a.sim.copy(ClipContent::Text("tiny".into()));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(b.sim.clipboard(), Some(ClipContent::Text("short_ok".into())));

    // 3. Disable clipboard entirely
    s["clipboard"]["enabled"] = false.into();
    s["clipboard"]["images"] = true.into();
    let (code, _) = http(web, "PUT", "/api/settings", Some(ADMIN), Some(&s.to_string())).await;
    assert_eq!(code, 200);
    assert!(b.agent.wait(T, |st| st.settings.as_ref().is_some_and(|s| !s.clipboard.enabled)).await);

    a.sim.copy(ClipContent::Image { png: vec![0x89, b'P', b'N', b'G', 1] });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(b.sim.clipboard(), Some(ClipContent::Text("short_ok".into())));

    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clipboard_disabled_node_excluded() {
    init_logs();
    let hub = start_hub().await;
    let web = hub.http_addr.unwrap();
    let token = hub.join_token();

    let a = start_agent(&hub, AgentSpec::new(&token)).await;
    let b = start_agent(&hub, AgentSpec::new(&token)).await;
    assert!(a.agent.wait(T, |s| s.connected && s.node_id.is_some()).await);
    assert!(b.agent.wait(T, |s| s.connected && s.node_id.is_some()).await);

    let idb = b.agent.status().node_id.unwrap();

    // Disable node B
    let (code, _) =
        http(web, "PATCH", &format!("/api/nodes/{}", idb.0), Some(ADMIN), Some(r#"{"enabled":false}"#)).await;
    assert_eq!(code, 204);

    // Node A copies text -> disabled node B must not receive it
    a.sim.copy(ClipContent::Text("for enabled nodes only".into()));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(b.sim.clipboard(), None);

    // Node B copies text -> hub drops it because sender is disabled
    b.sim.copy(ClipContent::Text("from disabled node".into()));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_ne!(a.sim.clipboard(), Some(ClipContent::Text("from disabled node".into())));

    hub.shutdown().await;
}
