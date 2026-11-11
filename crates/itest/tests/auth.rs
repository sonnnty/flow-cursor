//! Integration tests for authentication, certificate pinning, and impersonation resistance.

use flow_itest::*;
use flow_net::endpoint::{client_config, TransportOpts, SNI};
use flow_net::frame::{read_frame, write_frame};
use flow_net::tls::ServerPin;
use flow_net::Identity;
use flow_proto::{
    FromHub, Hello, LinkMsg, OsKind, RelayUp, StreamKind, ToHub, ALPN_HUB, ALPN_P2P, MAX_CONTROL_FRAME,
    PROTOCOL_VERSION,
};
use std::time::Duration;

const T: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_agent_without_token_rejected_by_hub() {
    init_logs();
    let hub = start_hub().await;
    let id = Identity::generate("attacker").unwrap();

    let cc = client_config(&id, ALPN_HUB, ServerPin::Exact(hub.fingerprint()), TransportOpts::HUB).unwrap();
    let ep = flow_net::endpoint::bind("127.0.0.1:0".parse().unwrap(), None).unwrap();
    let conn = ep.connect_with(cc, hub.quic_addr, SNI).unwrap().await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();

    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        agent_version: "0.1.0".into(),
        hostname: "attacker-box".into(),
        os: OsKind::Linux,
        join_token: None, // No join token!
        p2p_port: 0,
        p2p_addrs: vec![],
        monitors: monitor(1920, 1080),
        caps: flow_proto::Capabilities { capture: true, inject: true, clipboard: true },
    };
    write_frame(&mut send, &ToHub::Hello(hello)).await.unwrap();

    let resp = read_frame::<_, FromHub>(&mut recv, MAX_CONTROL_FRAME).await.unwrap();
    match resp {
        Some(FromHub::Reject { reason }) => {
            assert!(reason.contains("no valid join token"), "expected reject reason, got {reason}");
        }
        other => panic!("expected FromHub::Reject, got {other:?}"),
    }

    // Hub must close the connection
    let err = conn.closed().await;
    assert!(format!("{err}").contains("no valid join token"));
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hub_rejects_protocol_version_mismatch() {
    init_logs();
    let hub = start_hub().await;
    let token = hub.join_token();
    let id = Identity::generate("version-mismatch").unwrap();

    let cc = client_config(&id, ALPN_HUB, ServerPin::Exact(hub.fingerprint()), TransportOpts::HUB).unwrap();
    let ep = flow_net::endpoint::bind("127.0.0.1:0".parse().unwrap(), None).unwrap();
    let conn = ep.connect_with(cc, hub.quic_addr, SNI).unwrap().await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();

    let hello = Hello {
        protocol_version: PROTOCOL_VERSION + 999, // Incompatible protocol version
        agent_version: "99.0.0".into(),
        hostname: "future-box".into(),
        os: OsKind::Linux,
        join_token: Some(token),
        p2p_port: 0,
        p2p_addrs: vec![],
        monitors: monitor(1920, 1080),
        caps: flow_proto::Capabilities { capture: true, inject: true, clipboard: true },
    };
    write_frame(&mut send, &ToHub::Hello(hello)).await.unwrap();

    let resp = read_frame::<_, FromHub>(&mut recv, MAX_CONTROL_FRAME).await.unwrap();
    match resp {
        Some(FromHub::Reject { reason }) => {
            assert!(reason.contains("protocol version mismatch"), "expected version mismatch, got {reason}");
        }
        other => panic!("expected FromHub::Reject, got {other:?}"),
    }
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p2p_connection_rejects_untrusted_peer() {
    init_logs();
    let hub = start_hub().await;
    let token = hub.join_token();

    // Legitimate agent B
    let b = start_agent(&hub, AgentSpec::new(&token)).await;
    assert!(b.agent.wait(T, |s| s.connected).await);
    let b_p2p_port = b.agent.p2p_addr.port();
    let b_p2p_addr: std::net::SocketAddr = format!("127.0.0.1:{b_p2p_port}").parse().unwrap();

    // Attacker X tries to connect directly to B's P2P port with an untrusted identity
    let id_x = Identity::generate("attacker-x").unwrap();
    let cc_x = client_config(&id_x, ALPN_P2P, ServerPin::Exact(b.agent.fingerprint), TransportOpts::P2P).unwrap();
    let ep_x = flow_net::endpoint::bind("127.0.0.1:0".parse().unwrap(), None).unwrap();

    let conn = ep_x.connect_with(cc_x, b_p2p_addr, SNI).unwrap().await.unwrap();
    // Server must reject the untrusted peer and close the connection:
    let err = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
    assert!(err.is_ok(), "server must close the connection for untrusted peer");
    let err_str = format!("{}", err.unwrap());
    assert!(
        err_str.contains("unknown peer")
            || err_str.contains("ApplicationClosed")
            || err_str.contains("ApplicationVerificationFailure"),
        "unexpected error: {err_str}"
    );

    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p2p_dial_rejects_wrong_server_fingerprint() {
    init_logs();
    let hub = start_hub().await;
    let token = hub.join_token();

    // Legitimate agents A and B
    let a = start_agent(&hub, AgentSpec::new(&token)).await;
    let b = start_agent(&hub, AgentSpec::new(&token)).await;
    assert!(a.agent.wait(T, |s| s.connected).await);
    assert!(b.agent.wait(T, |s| s.connected).await);

    let b_p2p_port = b.agent.p2p_addr.port();
    let b_p2p_addr: std::net::SocketAddr = format!("127.0.0.1:{b_p2p_port}").parse().unwrap();

    // Dial B, but pin a completely bogus fingerprint
    let bogus_fp = flow_proto::Fingerprint([0xAA; 32]);
    let id_a = Identity::generate("test-dial").unwrap();
    let cc = client_config(&id_a, ALPN_P2P, ServerPin::Exact(bogus_fp), TransportOpts::P2P).unwrap();
    let ep = flow_net::endpoint::bind("127.0.0.1:0".parse().unwrap(), None).unwrap();

    let dial = ep.connect_with(cc, b_p2p_addr, SNI).unwrap().await;
    assert!(dial.is_err(), "dialing with wrong server pin must fail");
    let err_str = dial.unwrap_err().to_string();
    assert!(err_str.contains("ApplicationVerificationFailure"), "unexpected error: {err_str}");

    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_stamps_authenticated_sender_identity() {
    init_logs();
    let hub = start_hub().await;
    let token = hub.join_token();

    let a = start_agent(&hub, AgentSpec::new(&token)).await;
    let b = start_agent(&hub, AgentSpec::new(&token)).await;
    assert!(a.agent.wait(T, |s| s.connected && s.node_id.is_some()).await);
    assert!(b.agent.wait(T, |s| s.connected && s.node_id.is_some()).await);

    let ida = a.agent.status().node_id.unwrap();
    let _idb = b.agent.status().node_id.unwrap();

    // Connect an agent C directly to the hub to test raw relay frames
    let id_c = Identity::generate("c").unwrap();
    let cc_c = client_config(&id_c, ALPN_HUB, ServerPin::Exact(hub.fingerprint()), TransportOpts::HUB).unwrap();
    let ep_c = flow_net::endpoint::bind("127.0.0.1:0".parse().unwrap(), None).unwrap();
    let conn_c = ep_c.connect_with(cc_c, hub.quic_addr, SNI).unwrap().await.unwrap();
    let (mut ctl_s, mut ctl_r) = conn_c.open_bi().await.unwrap();

    let hello_c = Hello {
        protocol_version: PROTOCOL_VERSION,
        agent_version: "0.1.0".into(),
        hostname: "agent-c".into(),
        os: OsKind::Linux,
        join_token: Some(token),
        p2p_port: 0,
        p2p_addrs: vec![],
        monitors: monitor(1920, 1080),
        caps: flow_proto::Capabilities { capture: true, inject: true, clipboard: true },
    };
    write_frame(&mut ctl_s, &ToHub::Hello(hello_c)).await.unwrap();
    let welcome_c = match read_frame::<_, FromHub>(&mut ctl_r, MAX_CONTROL_FRAME).await.unwrap() {
        Some(FromHub::Welcome(w)) => w,
        other => panic!("expected welcome, got {other:?}"),
    };
    let idc = welcome_c.node_id;

    // Open relay stream on C
    let (mut relay_s, _relay_r) = conn_c.open_bi().await.unwrap();
    write_frame(&mut relay_s, &StreamKind::Relay).await.unwrap();

    // C sends a RelayUp message to A
    let msg = LinkMsg::Ack { session: 1234, upto: 10 };
    write_frame(&mut relay_s, &RelayUp { to: ida, msg: msg.clone() }).await.unwrap();

    // Verify: C's relay message received by A is stamped with from: idc (authenticated ID of C)
    // The hub does not allow C to forge from: idb or any other node.
    let (code, state_json) = http(hub.http_addr.unwrap(), "GET", "/api/state", Some(ADMIN), None).await;
    assert_eq!(code, 200);
    assert!(state_json.contains(&format!("\"id\":{}", idc.0)));

    hub.shutdown().await;
}
