//! One agent connection: authentication/enrolment, control stream, relay
//! stream, clipboard stream and relay datagrams.

use crate::state::{unix_now, ClipMeta, NodeRecord, Online, PairStat};
use crate::Hub;
use anyhow::{anyhow, bail, Result};
use flow_net::frame::{read_frame, write_frame};
use flow_net::quinn::{self, Connection};
use flow_net::tls::peer_fingerprint;
use flow_net::token::JoinToken;
use flow_proto::{
    ClipboardMsg, Fingerprint, FromHub, Hello, NodeId, RelayDatagramDown, RelayDatagramUp, RelayDown, RelayUp,
    StreamKind, ToHub, Welcome, MAX_CLIPBOARD_FRAME, MAX_CONTROL_FRAME, PROTOCOL_VERSION,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

pub async fn handle_incoming(hub: Arc<Hub>, incoming: quinn::Incoming) {
    let remote = incoming.remote_address();
    let conn = match incoming.await {
        Ok(c) => c,
        Err(e) => {
            debug!(%remote, "handshake failed: {e}");
            return;
        }
    };
    if let Err(e) = handle_conn(hub, conn.clone()).await {
        debug!(%remote, "connection ended: {e:#}");
    }
}

async fn handle_conn(hub: Arc<Hub>, conn: Connection) -> Result<()> {
    let remote = conn.remote_address();
    let fp = peer_fingerprint(&conn).ok_or_else(|| anyhow!("no client certificate"))?;
    let (mut send, mut recv) =
        tokio::time::timeout(Duration::from_secs(10), conn.accept_bi()).await.map_err(|_| anyhow!("no hello"))??;
    let hello = match read_frame::<_, ToHub>(&mut recv, MAX_CONTROL_FRAME).await? {
        Some(ToHub::Hello(h)) => h,
        _ => bail!("first message was not Hello"),
    };

    let node = match authenticate(&hub, &fp, &hello) {
        Ok(id) => id,
        Err(reason) => {
            warn!(%remote, "rejecting agent: {reason}");
            // Slow down token guessing.
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = write_frame(&mut send, &FromHub::Reject { reason: reason.clone() }).await;
            let _ = send.finish();
            tokio::time::sleep(Duration::from_millis(200)).await;
            conn.close(1u32.into(), reason.as_bytes());
            return Ok(());
        }
    };

    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<FromHub>();
    let conn_id;
    {
        let mut st = hub.lock();
        conn_id = st.next_conn_id;
        st.next_conn_id += 1;
        let p2p_addrs = if hello.p2p_addrs.is_empty() {
            vec![std::net::SocketAddr::new(remote.ip(), hello.p2p_port)]
        } else {
            hello.p2p_addrs.clone()
        };
        if let Some(old) = st.online.remove(&node) {
            old.conn.close(2u32.into(), b"replaced by a new connection");
        }
        st.online.insert(
            node,
            Online {
                conn_id,
                conn: conn.clone(),
                ctl: ctl_tx.clone(),
                relay: None,
                clip: None,
                remote,
                p2p_addrs,
                agent_version: hello.agent_version.clone(),
                since: unix_now(),
            },
        );
        let rec = st.p.nodes.get(&node.0).expect("authenticated node exists");
        let welcome = Welcome {
            node_id: node,
            name: rec.name.clone(),
            hub_version: env!("CARGO_PKG_VERSION").to_string(),
            settings: st.p.settings.clone(),
            layout: st.p.layout(),
            peers: st.peers_for(node),
            focus: st.focus_entries(),
        };
        let _ = ctl_tx.send(FromHub::Welcome(Box::new(welcome)));
        st.broadcast_peers();
        st.broadcast_layout();
        info!(%node, name = %rec.name, %remote, "agent connected");
    }

    // Control writer.
    let writer = tokio::spawn(async move {
        while let Some(m) = ctl_rx.recv().await {
            if write_frame(&mut send, &m).await.is_err() {
                break;
            }
        }
    });

    // Extra streams and datagrams.
    let streams = {
        let hub = hub.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            while let Ok((s, r)) = conn.accept_bi().await {
                tokio::spawn(handle_stream(hub.clone(), node, conn_id, s, r));
            }
        })
    };
    let datagrams = {
        let hub = hub.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            while let Ok(b) = conn.read_datagram().await {
                let Ok(up) = flow_proto::decode::<RelayDatagramUp>(&b) else { continue };
                let target = {
                    let st = hub.lock();
                    if !relay_allowed(&st, node, up.to) {
                        continue;
                    }
                    st.online.get(&up.to).map(|o| o.conn.clone())
                };
                if let Some(t) = target {
                    let _ = t.send_datagram(flow_proto::encode(&RelayDatagramDown { from: node, dg: up.dg }).into());
                }
            }
        })
    };

    // Control reader.
    let result = control_loop(&hub, node, &mut recv).await;

    writer.abort();
    streams.abort();
    datagrams.abort();
    {
        let mut st = hub.lock();
        if st.online.get(&node).is_some_and(|o| o.conn_id == conn_id) {
            st.online.remove(&node);
            if let Some(rec) = st.p.nodes.get_mut(&node.0) {
                rec.last_seen = unix_now();
            }
            st.save();
            st.focus.remove(&node);
            st.release_focus_on(node);
            st.pairs.retain(|(a, b), _| *a != node && *b != node);
            st.broadcast_peers();
            st.broadcast_focus();
            info!(%node, "agent disconnected");
        }
    }
    result
}

fn relay_allowed(st: &crate::state::HubState, from: NodeId, to: NodeId) -> bool {
    let en = |id: NodeId| st.p.nodes.get(&id.0).is_some_and(|n| n.enabled);
    from != to && en(from) && en(to)
}

/// Known certificate → its node. Unknown certificate → enrol if the join
/// token is valid.
fn authenticate(hub: &Hub, fp: &Fingerprint, hello: &Hello) -> Result<NodeId, String> {
    if hello.protocol_version != PROTOCOL_VERSION {
        return Err(format!(
            "protocol version mismatch: agent {} vs hub {}; install the same FlowCursor version on hub and agents",
            hello.protocol_version, PROTOCOL_VERSION
        ));
    }
    let mut st = hub.lock();
    if let Some(rec) = st.p.node_by_fp(fp) {
        let id = rec.id;
        let rec = st.p.nodes.get_mut(&id.0).expect("exists");
        rec.hostname = hello.hostname.clone();
        rec.os = hello.os;
        rec.caps = hello.caps;
        rec.last_seen = unix_now();
        if !hello.monitors.is_empty() {
            rec.monitors = hello.monitors.clone();
        }
        st.save();
        return Ok(id);
    }
    let token =
        hello.join_token.as_deref().and_then(JoinToken::parse).ok_or("unknown agent and no valid join token")?;
    if token.hub_fp_prefix[..] != fp_prefix(&hub.identity.fingerprint)[..]
        || !token.secret_matches(&st.p.join_secret_bytes())
    {
        return Err("invalid join token".into());
    }
    let id = NodeId(st.p.next_id);
    st.p.next_id += 1;
    let (x, y) = st.p.place_new(&hello.monitors);
    let name = st.p.unique_name(&hello.hostname);
    info!(node = %id, %name, fingerprint = %fp, "enrolled new agent");
    st.p.nodes.insert(
        id.0,
        NodeRecord {
            id,
            name,
            hostname: hello.hostname.clone(),
            fingerprint: fp.to_hex(),
            os: hello.os,
            enabled: true,
            mouse_speed: 1.0,
            x,
            y,
            monitors: hello.monitors.clone(),
            caps: hello.caps,
            created: unix_now(),
            last_seen: unix_now(),
        },
    );
    st.save();
    Ok(id)
}

fn fp_prefix(fp: &Fingerprint) -> [u8; 16] {
    let mut p = [0u8; 16];
    p.copy_from_slice(&fp.0[..16]);
    p
}

async fn control_loop(hub: &Arc<Hub>, node: NodeId, recv: &mut quinn::RecvStream) -> Result<()> {
    loop {
        let msg = match read_frame::<_, ToHub>(recv, MAX_CONTROL_FRAME).await? {
            Some(m) => m,
            None => return Ok(()),
        };
        let mut st = hub.lock();
        // A node revoked while connected loses everything.
        if !st.p.nodes.contains_key(&node.0) {
            bail!("node was revoked");
        }
        match msg {
            ToHub::Hello(_) => {}
            ToHub::Ping { nonce } => st.send_to(node, FromHub::Pong { nonce }),
            ToHub::Pong { .. } => {}
            ToHub::UpdateMonitors(m) => {
                if let Some(rec) = st.p.nodes.get_mut(&node.0) {
                    if rec.monitors != m {
                        rec.monitors = m;
                        st.save();
                        st.broadcast_layout();
                    }
                }
            }
            ToHub::FocusChanged { target } => {
                let changed = match target {
                    Some(t) if t != node => st.focus.insert(node, t) != Some(t),
                    _ => st.focus.remove(&node).is_some(),
                };
                if changed {
                    info!(source = %node, target = ?target.map(|t| t.to_string()), "focus changed");
                    st.broadcast_focus();
                }
            }
            ToHub::PathReports(reports) => {
                for r in reports {
                    let prev = st.pairs.get(&(node, r.peer)).map(|p| p.path);
                    if prev.is_some_and(|p| p != r.path) {
                        info!(source = %node, target = %r.peer, from = ?prev, to = %r.path, rtt_us = ?r.rtt_us, "path changed");
                    }
                    st.pairs.insert(
                        (node, r.peer),
                        PairStat {
                            path: r.path,
                            rtt_us: r.rtt_us,
                            p2p_rtt_us: r.p2p_rtt_us,
                            relay_rtt_us: r.relay_rtt_us,
                            updated: Some(Instant::now()),
                        },
                    );
                }
            }
        }
    }
}

async fn handle_stream(
    hub: Arc<Hub>,
    node: NodeId,
    conn_id: u64,
    send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) {
    let kind = match read_frame::<_, StreamKind>(&mut recv, 64).await {
        Ok(Some(k)) => k,
        _ => return,
    };
    let r = match kind {
        StreamKind::Relay => relay_stream(&hub, node, conn_id, send, recv).await,
        StreamKind::Clipboard => clipboard_stream(&hub, node, conn_id, send, recv).await,
    };
    if let Err(e) = r {
        debug!(%node, ?kind, "stream ended: {e:#}");
    }
}

async fn relay_stream(
    hub: &Arc<Hub>,
    node: NodeId,
    conn_id: u64,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<RelayDown>();
    {
        let mut st = hub.lock();
        match st.online.get_mut(&node) {
            Some(o) if o.conn_id == conn_id => o.relay = Some(tx),
            _ => return Ok(()),
        }
    }
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if write_frame(&mut send, &m).await.is_err() {
                break;
            }
        }
    });
    let res = async {
        while let Some(up) = read_frame::<_, RelayUp>(&mut recv, MAX_CONTROL_FRAME).await? {
            let st = hub.lock();
            if !relay_allowed(&st, node, up.to) {
                continue;
            }
            if let Some(t) = st.online.get(&up.to).and_then(|o| o.relay.as_ref()) {
                let _ = t.send(RelayDown { from: node, msg: up.msg });
            }
        }
        Ok(())
    }
    .await;
    writer.abort();
    res
}

async fn clipboard_stream(
    hub: &Arc<Hub>,
    node: NodeId,
    conn_id: u64,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<ClipboardMsg>();
    {
        let mut st = hub.lock();
        match st.online.get_mut(&node) {
            Some(o) if o.conn_id == conn_id => o.clip = Some(tx),
            _ => return Ok(()),
        }
    }
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if write_frame(&mut send, &m).await.is_err() {
                break;
            }
        }
    });
    let res = async {
        while let Some(ClipboardMsg::Update { content, .. }) =
            read_frame::<_, ClipboardMsg>(&mut recv, MAX_CLIPBOARD_FRAME).await?
        {
            let mut st = hub.lock();
            let enabled = st.p.nodes.get(&node.0).is_some_and(|n| n.enabled);
            if !enabled || !content.allowed_by(&st.p.settings.clipboard) {
                debug!(%node, ?content, "clipboard update dropped by settings");
                continue;
            }
            st.last_clip = Some(ClipMeta { from: node, kind: content.kind(), bytes: content.len(), at: unix_now() });
            let mut n = 0;
            for (id, o) in st.online.iter() {
                let target_enabled = st.p.nodes.get(&id.0).is_some_and(|n| n.enabled);
                if *id != node && target_enabled {
                    if let Some(c) = &o.clip {
                        let _ = c.send(ClipboardMsg::Update { from: Some(node), content: content.clone() });
                        n += 1;
                    }
                }
            }
            debug!(%node, ?content, recipients = n, "clipboard broadcast");
        }
        Ok(())
    }
    .await;
    writer.abort();
    res
}
