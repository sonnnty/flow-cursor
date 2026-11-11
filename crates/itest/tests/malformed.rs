//! Integration tests for malformed packets, oversized frames, corrupted datagrams, and protocol edge cases.

use flow_agent::backend::LocalEvent;
use flow_itest::*;
use flow_keymap::keys;
use flow_net::endpoint::{client_config, TransportOpts, SNI};
use flow_net::frame::write_raw;
use flow_net::link::Action;
use flow_net::tls::ServerPin;
use flow_net::token::JoinToken;
use flow_net::Identity;
use flow_proto::{
    Fingerprint, LinkDatagram, NodeId, PathKind, RelayDatagramDown, RelayDatagramUp, ALPN_HUB, ALPN_P2P,
    MAX_CONTROL_FRAME,
};
use std::time::Duration;

const T: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hub_handles_oversized_control_frame() {
    init_logs();
    let hub = start_hub().await;
    let id = Identity::generate("oversized").unwrap();

    let cc = client_config(&id, ALPN_HUB, ServerPin::Exact(hub.fingerprint()), TransportOpts::HUB).unwrap();
    let ep = flow_net::endpoint::bind("127.0.0.1:0".parse().unwrap(), None).unwrap();
    let conn = ep.connect_with(cc, hub.quic_addr, SNI).unwrap().await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();

    // Send a frame length that exceeds MAX_CONTROL_FRAME (e.g. 5 MB)
    let bad_len = (MAX_CONTROL_FRAME as u32) + 5 * 1024 * 1024;
    let _ = send.write(&bad_len.to_le_bytes()).await;

    // Hub must reject the frame and close the stream/connection without panic
    let mut buf = [0u8; 16];
    let res = recv.read(&mut buf).await;
    assert!(
        res.is_err() || res.unwrap().is_none_or(|n| n == 0),
        "hub must close stream upon receiving oversized frame"
    );

    // Hub is still healthy and accepts subsequent valid connections
    let good_agent = start_agent(&hub, AgentSpec::new(&hub.join_token())).await;
    assert!(good_agent.agent.wait(T, |s| s.connected).await, "hub remains healthy after oversized frame attack");

    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hub_handles_corrupted_frame_payload() {
    init_logs();
    let hub = start_hub().await;
    let id = Identity::generate("corrupted-payload").unwrap();

    let cc = client_config(&id, ALPN_HUB, ServerPin::Exact(hub.fingerprint()), TransportOpts::HUB).unwrap();
    let ep = flow_net::endpoint::bind("127.0.0.1:0".parse().unwrap(), None).unwrap();
    let conn = ep.connect_with(cc, hub.quic_addr, SNI).unwrap().await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();

    // Send valid frame length but completely non-postcard garbage payload
    let garbage = vec![0xFF, 0xFE, 0xFD, 0xFC, 0xAA, 0xBB, 0xCC, 0xDD];
    let _ = write_raw(&mut send, &garbage).await;

    // Hub must fail decoding and close the stream
    let mut buf = [0u8; 16];
    let res = recv.read(&mut buf).await;
    assert!(res.is_err() || res.unwrap().is_none_or(|n| n == 0), "hub must close stream on decode error");

    // Hub remains operational
    let good_agent = start_agent(&hub, AgentSpec::new(&hub.join_token())).await;
    assert!(good_agent.agent.wait(T, |s| s.connected).await);

    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hub_handles_truncated_frame_then_eof() {
    init_logs();
    let hub = start_hub().await;
    let id = Identity::generate("truncated-frame").unwrap();

    let cc = client_config(&id, ALPN_HUB, ServerPin::Exact(hub.fingerprint()), TransportOpts::HUB).unwrap();
    let ep = flow_net::endpoint::bind("127.0.0.1:0".parse().unwrap(), None).unwrap();
    let conn = ep.connect_with(cc, hub.quic_addr, SNI).unwrap().await.unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();

    // Length says 100 bytes, but we send 5 bytes and close
    let _ = send.write(&100u32.to_le_bytes()).await;
    let _ = send.write(&[1, 2, 3, 4, 5]).await;
    let _ = send.finish();

    // Verify connection ends without hanging the server
    tokio::time::sleep(Duration::from_millis(300)).await;

    let good_agent = start_agent(&hub, AgentSpec::new(&hub.join_token())).await;
    assert!(good_agent.agent.wait(T, |s| s.connected).await);

    hub.shutdown().await;
}

/// Sends `ping` until a matching pong arrives (datagrams may be dropped).
async fn ping_until_pong<T>(
    mut send: impl FnMut(LinkDatagram),
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<T>,
    is_pong: impl Fn(&T, u64) -> bool,
    nonce: u64,
) -> bool {
    for _ in 0..50 {
        send(LinkDatagram::Ping { session: 0, nonce });
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        while let Ok(Some(x)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            if is_pong(&x, nonce) {
                return true;
            }
        }
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hub_and_agent_handle_corrupted_datagrams() {
    init_logs();
    let hub = start_hub().await;
    let token = hub.join_token();

    // Layout in join order: A | B | R. A controls B; R is a registered,
    // authenticated node that sends garbage to the hub and to B.
    let a = start_agent(&hub, AgentSpec::new(&token)).await;
    let b = start_agent(&hub, AgentSpec::new(&token)).await;
    assert!(a.agent.wait(T, |s| s.connected).await);
    assert!(b.agent.wait(T, |s| s.connected).await);
    let idb = b.agent.status().node_id.unwrap();
    let r = RawNode::join(&hub, &token, "datagram-flooder").await;
    assert!(b.agent.wait(T, |s| s.online_peers.len() == 2).await, "B knows R as a peer");
    let mut relayed_to_r = r.datagrams();

    // R's datagrams reach B's parser through the hub: a Ping is answered.
    let relay_dg = |dg| {
        let _ = r.conn.send_datagram(flow_proto::encode(&RelayDatagramUp { to: idb, dg }).into());
    };
    let relay_pong = |d: &RelayDatagramDown, n| d.from == idb && d.dg == LinkDatagram::Pong { nonce: n };
    assert!(ping_until_pong(relay_dg, &mut relayed_to_r, relay_pong, 1).await, "relay path from R to B works");

    // ... and directly: B's P2P listener accepts R (a known peer) and
    // answers its pings, so garbage on this connection reaches the parser.
    let p2p_addr: std::net::SocketAddr = format!("127.0.0.1:{}", b.agent.p2p_addr.port()).parse().unwrap();
    let cc_p2p =
        client_config(&r.identity, ALPN_P2P, ServerPin::Exact(b.agent.fingerprint), TransportOpts::P2P).unwrap();
    let p2p = r.endpoint.connect_with(cc_p2p, p2p_addr, SNI).unwrap().await.expect("B accepts R");
    let (p2p_tx, mut p2p_rx) = tokio::sync::mpsc::unbounded_channel();
    {
        let p2p = p2p.clone();
        tokio::spawn(async move {
            while let Ok(d) = p2p.read_datagram().await {
                if let Ok(d) = flow_proto::decode::<LinkDatagram>(&d) {
                    let _ = p2p_tx.send(d);
                }
            }
        });
    }
    let direct_dg = |dg| {
        let _ = p2p.send_datagram(flow_proto::encode(&dg).into());
    };
    let direct_pong = |d: &LinkDatagram, n| *d == LinkDatagram::Pong { nonce: n };
    assert!(ping_until_pong(direct_dg, &mut p2p_rx, direct_pong, 2).await, "direct path from R to B works");

    // Garbage, from the registered node.
    let valid = flow_proto::encode(&RelayDatagramUp { to: idb, dg: LinkDatagram::Pong { nonce: 9 } });
    let mut trailing = valid.clone();
    trailing.extend_from_slice(&[0xff; 7]);
    let corrupted: Vec<Vec<u8>> = vec![
        vec![],
        vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x11, 0x22],
        vec![0xff; 64],
        valid[..valid.len() - 1].to_vec(), // truncated
        trailing,
        valid.iter().map(|b| b ^ 0x5a).collect(), // bit-flipped
    ];
    for _ in 0..20 {
        for g in &corrupted {
            let _ = r.conn.send_datagram(g.clone().into());
            let _ = p2p.send_datagram(g.clone().into());
        }
        // Well-formed but bogus: motion for a session B never saw, huge
        // coordinates; datagrams for unknown nodes and for R itself.
        let bogus = LinkDatagram::Motion(flow_proto::Motion {
            session: 0xdead,
            mseq: u64::MAX,
            after_seq: 0,
            x: i32::MAX,
            y: i32::MIN,
            dx: i32::MIN,
            dy: i32::MAX,
        });
        relay_dg(bogus);
        direct_dg(bogus);
        for to in [NodeId(9999), r.id] {
            let _ = r.conn.send_datagram(flow_proto::encode(&RelayDatagramUp { to, dg: bogus }).into());
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The hub and B still answer R on both paths, and nothing was injected.
    assert!(ping_until_pong(relay_dg, &mut relayed_to_r, relay_pong, 3).await, "hub relay still works");
    assert!(ping_until_pong(direct_dg, &mut p2p_rx, direct_pong, 4).await, "B's P2P listener still works");
    assert!(p2p.close_reason().is_none(), "B keeps the connection open");
    assert!(b.sim.take_injected().is_empty(), "garbage must not be injected");
    assert!(a.agent.status().connected && b.agent.status().connected);

    // A still controls B, directly and through the hub relay.
    a.sim.emit(LocalEvent::EdgeHit { side: flow_layout::Side::Right, x: 1919, y: 540 });
    assert!(a.agent.wait(T, |s| s.controlling == Some(idb)).await);
    a.sim.emit(LocalEvent::Key { usage: keys::A, down: true });
    a.sim.emit(LocalEvent::Key { usage: keys::A, down: false });
    let got = collect_until(&b.sim, T, |v| v.len() >= 3).await;
    assert_eq!(
        got,
        vec![
            Action::Entered { x: 0, y: 540 },
            Action::Key { usage: keys::A, down: true },
            Action::Key { usage: keys::A, down: false },
        ]
    );
    let web = hub.http_addr.unwrap();
    let (_, s) = http(web, "GET", "/api/settings", Some(ADMIN), None).await;
    let mut s: serde_json::Value = serde_json::from_str(&s).unwrap();
    s["path_policy"] = "force_relay".into();
    assert_eq!(http(web, "PUT", "/api/settings", Some(ADMIN), Some(&s.to_string())).await.0, 200);
    assert!(a.agent.wait_link_path(idb, PathKind::Relay, T).await);
    a.sim.emit(LocalEvent::Key { usage: keys::A + 1, down: true });
    a.sim.emit(LocalEvent::Key { usage: keys::A + 1, down: false });
    let got = collect_until(&b.sim, T, |v| v.len() >= 2).await;
    assert_eq!(
        got,
        vec![Action::Key { usage: keys::A + 1, down: true }, Action::Key { usage: keys::A + 1, down: false }],
        "relayed through the hub"
    );
    hub.shutdown().await;
}

#[test]
fn token_parsing_resilience() {
    // Malformed tokens that should fail parsing cleanly
    let bad_tokens = [
        "",
        "   ",
        "-",
        "FC1",
        "FC1-",
        "FC1--",
        "FC2-ABC-DEF",
        "FC1-INVALIDBASE32!!!-1234",
        "FC1-AAAA-BBBB-CCCC",
        "FC1-TOO_SHORT-PREFIX",
        "FC1-AAAAAAAAAAAAAAAAAAAAAAAA-BBBBBBBBBBBBBBBBBBBBBBBB", // too long
    ];
    for bad in bad_tokens {
        assert_eq!(JoinToken::parse(bad), None, "token '{bad}' should be rejected");
    }

    // Valid token with arbitrary whitespace and lowercase
    let fp = Fingerprint([0x3c; 32]);
    let tok = JoinToken::new([0x77; 16], &fp);
    let encoded = tok.encode();
    assert_eq!(JoinToken::parse(&encoded), Some(tok.clone()));
    assert_eq!(JoinToken::parse(&format!(" \t\n{} \r\n", encoded.to_lowercase())), Some(tok));
}
