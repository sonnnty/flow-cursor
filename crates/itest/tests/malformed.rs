//! Integration tests for malformed packets, oversized frames, corrupted datagrams, and protocol edge cases.

use flow_itest::*;
use flow_net::endpoint::{client_config, TransportOpts, SNI};
use flow_net::frame::write_raw;
use flow_net::tls::ServerPin;
use flow_net::token::JoinToken;
use flow_net::Identity;
use flow_proto::{Fingerprint, ALPN_HUB, ALPN_P2P, MAX_CONTROL_FRAME};
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hub_and_agent_handle_corrupted_datagrams() {
    init_logs();
    let hub = start_hub().await;
    let token = hub.join_token();

    let a = start_agent(&hub, AgentSpec::new(&token)).await;
    assert!(a.agent.wait(T, |s| s.connected).await);

    // Send garbage datagrams to the hub
    let id = Identity::generate("datagram-flooder").unwrap();
    let cc = client_config(&id, ALPN_HUB, ServerPin::Exact(hub.fingerprint()), TransportOpts::HUB).unwrap();
    let ep = flow_net::endpoint::bind("127.0.0.1:0".parse().unwrap(), None).unwrap();
    let conn = ep.connect_with(cc, hub.quic_addr, SNI).unwrap().await.unwrap();

    for _ in 0..50 {
        let _ = conn.send_datagram(vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x11, 0x22].into());
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Send garbage datagrams to agent's P2P listener
    let p2p_addr: std::net::SocketAddr = format!("127.0.0.1:{}", a.agent.p2p_addr.port()).parse().unwrap();
    let cc_p2p = client_config(&id, ALPN_P2P, ServerPin::Exact(a.agent.fingerprint), TransportOpts::P2P).unwrap();
    if let Ok(p2p_conn) = ep.connect_with(cc_p2p, p2p_addr, SNI).unwrap().await {
        for _ in 0..50 {
            let _ = p2p_conn.send_datagram(vec![0xCA, 0xFE, 0xBA, 0xBE].into());
        }
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Both hub and agent must continue operating normally
    assert!(a.agent.status().connected);
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
