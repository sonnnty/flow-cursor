//! Integration tests for authentication, certificate pinning, and impersonation resistance.

use flow_itest::*;
use flow_net::endpoint::{client_config, TransportOpts, SNI};
use flow_net::frame::{read_frame, write_frame, write_raw};
use flow_net::tls::ServerPin;
use flow_net::Identity;
use flow_proto::{
    FromHub, Hello, LinkDatagram, LinkMsg, OsKind, RelayDatagramDown, RelayDatagramUp, RelayDown, RelayUp, ToHub,
    ALPN_HUB, ALPN_P2P, MAX_CONTROL_FRAME, PROTOCOL_VERSION,
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

/// Waits a little for anything else on `rx` and returns it.
async fn rest<T>(rx: &mut tokio::sync::mpsc::UnboundedReceiver<T>) -> Vec<T> {
    let mut out = Vec::new();
    while let Ok(Some(x)) = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
        out.push(x);
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_stamps_authenticated_sender_identity() {
    init_logs();
    let hub = start_hub().await;
    let token = hub.join_token();
    let a = RawNode::join(&hub, &token, "agent-a").await;
    let b = RawNode::join(&hub, &token, "agent-b").await;
    let c = RawNode::join(&hub, &token, "agent-c").await;
    let (ida, idb, idc) = (a.id, b.id, c.id);
    assert!(ida != idb && idb != idc && ida != idc);
    let (_a_send, mut at_a) = a.relay().await;
    let (_b_send, mut at_b) = b.relay().await;
    let mut dg_at_a = a.datagrams();
    let mut dg_at_b = b.datagrams();
    let (mut c_send, _at_c) = c.relay().await;
    let probe = |to| RelayUp { to, msg: LinkMsg::Ack { session: 0, upto: 0 } };
    assert_eq!(relay_probe(&mut c_send, &probe(ida), &mut at_a).await.from, idc);
    assert_eq!(relay_probe(&mut c_send, &probe(idb), &mut at_b).await.from, idc);

    // A relayed frame from C reaches A stamped with C's authenticated id.
    let msg = LinkMsg::Ack { session: 1234, upto: 10 };
    write_frame(&mut c_send, &RelayUp { to: ida, msg: msg.clone() }).await.unwrap();
    let got = loop {
        let d = tokio::time::timeout(T, at_a.recv()).await.expect("relay frame reaches A").unwrap();
        if d.msg != probe(ida).msg {
            break d;
        }
    };
    assert_eq!(got, RelayDown { from: idc, msg });

    // Forgery 1: C sends a hub→agent frame claiming to come from B. RelayUp
    // has no sender field, so the hub reads B's id as the destination and
    // still stamps the frame with C's id.
    let forged = LinkMsg::Ack { session: 666, upto: 1 };
    write_raw(&mut c_send, &flow_proto::encode(&RelayDown { from: idb, msg: forged.clone() })).await.unwrap();
    let got = tokio::time::timeout(T, async {
        loop {
            let d = at_b.recv().await.unwrap();
            if d.msg == forged {
                return d;
            }
        }
    })
    .await
    .expect("the forged frame is routed as a plain RelayUp to B");
    assert_eq!(got.from, idc, "the hub must stamp the connection's identity, not the payload's");

    // Forgery 2 (datagrams): the same trick on the relay datagram path.
    let forged_dg = LinkDatagram::Ping { session: 666, nonce: 1 };
    let honest_dg = LinkDatagram::Ping { session: 1234, nonce: 2 };
    for _ in 0..3 {
        c.conn.send_datagram(flow_proto::encode(&RelayDatagramDown { from: idb, dg: forged_dg }).into()).unwrap();
        c.conn.send_datagram(flow_proto::encode(&RelayDatagramUp { to: ida, dg: honest_dg }).into()).unwrap();
    }

    // Forgery 3: a RelayUp to A with B's id appended as a "sender" field.
    // The hub rejects the frame (trailing bytes) and ends C's relay stream.
    let mut payload = flow_proto::encode(&RelayUp { to: ida, msg: LinkMsg::Ack { session: 667, upto: 1 } });
    payload.extend(flow_proto::encode(&idb));
    write_raw(&mut c_send, &payload).await.unwrap();

    // C can open a new relay stream and keeps being stamped as C.
    let (mut c_send2, _at_c2) = c.relay().await;
    let marker = RelayUp { to: ida, msg: LinkMsg::Ack { session: 999, upto: 0 } };
    let mut seen_at_a = vec![relay_probe(&mut c_send2, &marker, &mut at_a).await];
    seen_at_a.extend(rest(&mut at_a).await);

    // A never saw anything attributed to B, and never saw the forged frames.
    for d in &seen_at_a {
        assert_eq!(d.from, idc, "frame at A not stamped as C: {d:?}");
        assert!(!matches!(d.msg, LinkMsg::Ack { session: 666 | 667, .. }), "forged frame reached A: {d:?}");
    }
    let dgs_at_a = rest(&mut dg_at_a).await;
    assert!(dgs_at_a.iter().any(|d| d.dg == honest_dg), "honest relay datagram must reach A: {dgs_at_a:?}");
    for d in &dgs_at_a {
        assert_eq!(*d, RelayDatagramDown { from: idc, dg: honest_dg });
    }
    let dgs_at_b = rest(&mut dg_at_b).await;
    assert!(dgs_at_b.iter().any(|d| d.dg == forged_dg), "forged datagram is routed to B: {dgs_at_b:?}");
    for d in &dgs_at_b {
        assert_eq!(*d, RelayDatagramDown { from: idc, dg: forged_dg });
    }
    for d in rest(&mut at_b).await {
        assert_eq!(d.from, idc, "frame at B not stamped as C: {d:?}");
    }

    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_admin_token_falls_back_to_the_persisted_one() {
    init_logs();
    // What deploy/compose.yaml passes when FLOW_ADMIN_TOKEN is not set.
    let hub = flow_server::start(flow_server::HubConfig {
        data_dir: temp_dir("hub-empty-token"),
        quic_addr: "127.0.0.1:0".parse().unwrap(),
        http_addr: Some("127.0.0.1:0".parse().unwrap()),
        admin_token: Some(String::new()),
        mdns: false,
        name: "itest".into(),
    })
    .await
    .unwrap();
    // Persisted means flow-server prints it at startup.
    assert_eq!(hub.hub.admin_token_source, flow_server::AdminTokenSource::Persisted);
    let token = hub.hub.admin_token.clone();
    assert_eq!(token, hub.hub.lock().p.admin_token);
    assert!(token.len() >= 32);
    let addr = hub.http_addr.unwrap();
    assert_eq!(http(addr, "GET", "/api/state", Some(&token), None).await.0, 200);
    assert_eq!(http(addr, "GET", "/api/state", Some(""), None).await.0, 401);
    assert_eq!(http(addr, "GET", "/api/state", None, None).await.0, 401);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dashboard_page_and_icons_need_no_token() {
    init_logs();
    let hub = start_hub().await;
    let web = hub.http_addr.expect("dashboard enabled");

    let (code, page) = http(web, "GET", "/", None, None).await;
    assert_eq!(code, 200);
    for icon in ["/favicon.svg", "/favicon-32.png", "/apple-touch-icon.png"] {
        assert!(page.contains(&format!("href=\"{icon}\"")), "page does not link {icon}");
        let (code, body) = http(web, "GET", icon, None, None).await;
        assert_eq!(code, 200, "{icon} must not be behind the dashboard token");
        assert!(!body.is_empty());
    }
    let (code, svg) = http(web, "GET", "/favicon.svg", None, None).await;
    assert!(code == 200 && svg.contains("<svg"));
    let (code, png) = http(web, "GET", "/favicon.ico", None, None).await;
    assert!(code == 200 && png.contains("PNG"), "favicon.ico serves the PNG");
    // The API stays protected.
    let (code, _) = http(web, "GET", "/api/state", None, None).await;
    assert_eq!(code, 401);
    hub.shutdown().await;
}
