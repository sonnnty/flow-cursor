//! One agent connection: authentication/enrolment, control stream, relay
//! stream, clipboard stream and relay datagrams.

use crate::state::{unix_now, AgentMeta, ClipMeta, LegacyAgent, NodeRecord, Online, PairStat, UpdateOnly};
use crate::Hub;
use anyhow::{anyhow, bail, Result};
use flow_net::frame::{read_frame, read_raw, write_frame};
use flow_net::quinn::{self, Connection};
use flow_net::tls::peer_fingerprint;
use flow_net::token::JoinToken;
use flow_proto::hello::{Hello, HelloPeek, LegacyReject};
use flow_proto::update::{UpdateFromHub, UpdateToHub};
use flow_proto::{
    Accepted, BuildId, ClipboardMsg, Fingerprint, FromHub, HelloFrame, HelloReply, NodeId, OsKind, Register,
    RelayDatagramDown, RelayDatagramUp, RelayDown, RelayUp, SessionMode, StreamKind, ToHub, Welcome,
    FIRST_UPDATABLE_PROTOCOL, MAX_CLIPBOARD_FRAME, MAX_CONTROL_FRAME, PROTOCOL_VERSION,
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

/// The OS of a platform key (`linux-x86_64` → Linux).
fn os_of(platform: &str) -> OsKind {
    match platform.split('-').next() {
        Some("linux") => OsKind::Linux,
        Some("windows") => OsKind::Windows,
        _ => OsKind::Other,
    }
}

async fn handle_conn(hub: Arc<Hub>, conn: Connection) -> Result<()> {
    let remote = conn.remote_address();
    let fp = peer_fingerprint(&conn).ok_or_else(|| anyhow!("no client certificate"))?;
    let (mut send, mut recv) =
        tokio::time::timeout(Duration::from_secs(10), conn.accept_bi()).await.map_err(|_| anyhow!("no hello"))??;
    let raw = read_raw(&mut recv, MAX_CONTROL_FRAME).await?.ok_or_else(|| anyhow!("no hello"))?;
    let peek = HelloPeek::read(&raw).ok_or_else(|| anyhow!("first message was not a hello"))?;
    if peek.protocol_version < FIRST_UPDATABLE_PROTOCOL {
        legacy_agent(&hub, &conn, &fp, &peek, &mut send).await;
        return Ok(());
    }
    let HelloFrame::Hello(hello) = flow_proto::decode::<HelloFrame>(&raw)?;
    // Same protocol: the session part decodes. Any other protocol (older or
    // newer) only gets to update itself to this hub's build.
    let register = if hello.protocol_version == PROTOCOL_VERSION {
        match flow_proto::decode::<Register>(&hello.session) {
            Ok(r) => Some(r),
            Err(e) => bail!("malformed hello from an agent of this protocol: {e}"),
        }
    } else {
        None
    };

    let node = match authenticate(&hub, &fp, &hello, register.as_ref()) {
        Ok(id) => id,
        Err(reason) => {
            warn!(%remote, "rejecting agent: {reason}");
            // Slow down token guessing.
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = write_frame(&mut send, &HelloReply::Rejected { reason: reason.clone() }).await;
            let _ = send.finish();
            tokio::time::sleep(Duration::from_millis(200)).await;
            conn.close(1u32.into(), reason.as_bytes());
            return Ok(());
        }
    };
    let meta = AgentMeta {
        build: hello.agent.clone(),
        platform: hello.platform.clone(),
        protocol: hello.protocol_version,
        report: None,
    };
    match register {
        Some(reg) => full_session(hub, conn, node, meta, reg, send, recv).await,
        None => update_only_session(hub, conn, node, meta, send, recv).await,
    }
}

/// An agent from before the update handshake: tell it (in its own reject
/// format) that it has to be updated by hand once, and show it in the
/// dashboard.
async fn legacy_agent(hub: &Hub, conn: &Connection, fp: &Fingerprint, peek: &HelloPeek, send: &mut quinn::SendStream) {
    let reason = format!(
        "this hub runs FlowCursor {} (protocol {PROTOCOL_VERSION}); this agent ({}, protocol {}) is too old to update \
         itself: install FlowCursor {} on this machine by hand once, later versions update themselves from the hub",
        flow_version::FULL,
        peek.agent_version,
        peek.protocol_version,
        flow_version::VERSION,
    );
    {
        let mut st = hub.lock();
        if let Some(id) = st.p.node_by_fp(fp).map(|n| n.id) {
            st.legacy.insert(
                id,
                LegacyAgent { version: peek.agent_version.clone(), protocol: peek.protocol_version, at: unix_now() },
            );
        }
    }
    warn!(remote = %conn.remote_address(), agent = %peek.agent_version, protocol = peek.protocol_version,
        "agent too old to update itself; it must be updated by hand");
    let _ = write_frame(send, &LegacyReject::Reject { reason: reason.clone() }).await;
    let _ = send.finish();
    tokio::time::sleep(Duration::from_millis(200)).await;
    conn.close(1u32.into(), reason.as_bytes());
}

fn accepted(hub: &Hub, node: NodeId, mode: SessionMode) -> HelloReply {
    let name = hub.lock().p.nodes.get(&node.0).map(|n| n.name.clone()).unwrap_or_default();
    HelloReply::Accepted(Accepted {
        node_id: node,
        name,
        hub: BuildId::current(),
        hub_protocol: PROTOCOL_VERSION,
        mode,
    })
}

async fn full_session(
    hub: Arc<Hub>,
    conn: Connection,
    node: NodeId,
    meta: AgentMeta,
    reg: Register,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> Result<()> {
    let remote = conn.remote_address();
    write_frame(&mut send, &accepted(&hub, node, SessionMode::Full)).await?;
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<FromHub>();
    let conn_id;
    {
        let mut st = hub.lock();
        conn_id = st.next_conn_id;
        st.next_conn_id += 1;
        let p2p_addrs = if reg.p2p_addrs.is_empty() {
            vec![std::net::SocketAddr::new(remote.ip(), reg.p2p_port)]
        } else {
            reg.p2p_addrs
        };
        if let Some(old) = st.online.remove(&node) {
            old.conn.close(2u32.into(), b"replaced by a new connection");
        }
        if let Some(old) = st.updating.remove(&node) {
            old.conn.close(2u32.into(), b"replaced by a new connection");
        }
        st.legacy.remove(&node);
        let build = meta.build.clone();
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
                meta,
                since: unix_now(),
                away: false,
                local_addrs: Vec::new(),
            },
        );
        let rec = st.p.nodes.get(&node.0).expect("authenticated node exists");
        let welcome = Welcome {
            node_id: node,
            name: rec.name.clone(),
            hub_version: flow_version::FULL.to_string(),
            settings: st.settings_for(node),
            layout: st.p.layout(),
            peers: st.peers_for(node),
            focus: st.focus_entries(),
        };
        let _ = ctl_tx.send(FromHub::Welcome(Box::new(welcome)));
        let _ = ctl_tx.send(FromHub::Update(UpdateFromHub::Offer(st.offer_for(node))));
        st.broadcast_peers();
        st.broadcast_layout();
        info!(%node, name = %rec.name, %remote, agent = %build, "agent connected");
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

/// An agent of another protocol: it gets the update offer and may fetch
/// files, nothing else. Frames it sends that this hub cannot decode (a
/// later protocol's additions) are skipped.
async fn update_only_session(
    hub: Arc<Hub>,
    conn: Connection,
    node: NodeId,
    meta: AgentMeta,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> Result<()> {
    let remote = conn.remote_address();
    let reason = format!(
        "agent {} speaks protocol {}, this hub ({}) protocol {PROTOCOL_VERSION}: it has to update to the hub's build \
         before it can share input",
        meta.build,
        meta.protocol,
        flow_version::FULL
    );
    write_frame(&mut send, &accepted(&hub, node, SessionMode::UpdateOnly { reason: reason.clone() })).await?;
    let (tx, mut rx) = mpsc::unbounded_channel::<UpdateFromHub>();
    let conn_id;
    {
        let mut st = hub.lock();
        conn_id = st.next_conn_id;
        st.next_conn_id += 1;
        if let Some(old) = st.online.remove(&node) {
            old.conn.close(2u32.into(), b"replaced by a new connection");
            st.focus.remove(&node);
            st.release_focus_on(node);
            st.broadcast_peers();
            st.broadcast_focus();
        }
        if let Some(old) = st.updating.remove(&node) {
            old.conn.close(2u32.into(), b"replaced by a new connection");
        }
        st.legacy.remove(&node);
        info!(%node, %remote, agent = %meta.build, protocol = meta.protocol, "agent connected in update-only mode");
        st.updating.insert(
            node,
            UpdateOnly { conn_id, conn: conn.clone(), tx: tx.clone(), remote, meta, since: unix_now(), reason },
        );
        let _ = tx.send(UpdateFromHub::Offer(st.offer_for(node)));
    }
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if write_frame(&mut send, &m).await.is_err() {
                break;
            }
        }
    });
    let streams = {
        let hub = hub.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            while let Ok((s, mut r)) = conn.accept_bi().await {
                let hub = hub.clone();
                tokio::spawn(async move {
                    // Only downloads in this mode.
                    if let Ok(Some(StreamKind::Fetch)) = read_frame::<_, StreamKind>(&mut r, 64).await {
                        let builds = hub.lock().builds.clone();
                        if let Err(e) = crate::updates::serve_fetch(builds, node, s, r).await {
                            debug!(%node, "fetch stream ended: {e:#}");
                        }
                    }
                });
            }
        })
    };
    let result = async {
        loop {
            let Some(raw) = read_raw(&mut recv, MAX_CONTROL_FRAME).await? else { return Ok(()) };
            match flow_proto::decode::<UpdateToHub>(&raw) {
                Ok(m) => on_update_msg(&hub, node, m),
                Err(e) => debug!(%node, "skipping an update frame this hub does not understand: {e}"),
            }
        }
    }
    .await;
    writer.abort();
    streams.abort();
    {
        let mut st = hub.lock();
        if st.updating.get(&node).is_some_and(|u| u.conn_id == conn_id) {
            st.updating.remove(&node);
            if let Some(rec) = st.p.nodes.get_mut(&node.0) {
                rec.last_seen = unix_now();
            }
            st.save();
            info!(%node, "update-only agent disconnected");
        }
    }
    result
}

/// An update report from `node` (either session kind).
fn on_update_msg(hub: &Hub, node: NodeId, m: UpdateToHub) {
    let mut st = hub.lock();
    let UpdateToHub::Report(r) = m;
    if let Some(f) = &r.failed {
        let known = st.agent_meta(node).and_then(|m| m.report.as_ref()).and_then(|r| r.failed.as_ref());
        if known != Some(f) {
            warn!(%node, target = %f.target, rolled_back = f.rolled_back, "agent update failed: {}", f.reason);
        }
    }
    let phase = st.agent_meta(node).and_then(|m| m.report.as_ref()).map(|r| r.phase.name());
    if phase != Some(r.phase.name()) {
        info!(%node, phase = r.phase.name(), "agent update state");
    }
    if let Some(meta) = st.agent_meta_mut(node) {
        meta.report = Some(r);
    }
}

/// Input may be relayed between two enabled nodes that are not away.
fn relay_allowed(st: &crate::state::HubState, from: NodeId, to: NodeId) -> bool {
    let ok = |id: NodeId| st.p.nodes.get(&id.0).is_some_and(|n| n.enabled) && !st.is_away(id);
    from != to && ok(from) && ok(to)
}

/// Known certificate → its node. Unknown certificate → enrol if the join
/// token is valid.
fn authenticate(hub: &Hub, fp: &Fingerprint, hello: &Hello, reg: Option<&Register>) -> Result<NodeId, String> {
    let mut st = hub.lock();
    if let Some(rec) = st.p.node_by_fp(fp) {
        let id = rec.id;
        let rec = st.p.nodes.get_mut(&id.0).expect("exists");
        rec.hostname = hello.hostname.clone();
        rec.os = os_of(&hello.platform);
        rec.platform = hello.platform.clone();
        rec.agent = Some(hello.agent.clone());
        rec.last_seen = unix_now();
        if let Some(reg) = reg {
            rec.os = reg.os;
            rec.caps = reg.caps;
            if !reg.monitors.is_empty() {
                rec.monitors = reg.monitors.clone();
            }
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
    // An agent of another protocol may enrol too (it then updates first);
    // monitors and capabilities arrive with its first full session.
    let monitors = reg.map(|r| r.monitors.clone()).unwrap_or_default();
    let id = NodeId(st.p.next_id);
    st.p.next_id += 1;
    let (x, y) = st.p.place_new(&monitors);
    let name = st.p.unique_name(&hello.hostname);
    info!(node = %id, %name, fingerprint = %fp, "enrolled new agent");
    st.p.nodes.insert(
        id.0,
        NodeRecord {
            id,
            name,
            hostname: hello.hostname.clone(),
            fingerprint: fp.to_hex(),
            os: reg.map(|r| r.os).unwrap_or_else(|| os_of(&hello.platform)),
            enabled: true,
            mouse_speed: 1.0,
            x,
            y,
            monitors,
            caps: reg.map(|r| r.caps).unwrap_or_default(),
            created: unix_now(),
            last_seen: unix_now(),
            allowed_networks: None,
            crossing_key: None,
            cursor_park: None,
            update_policy: None,
            agent: Some(hello.agent.clone()),
            platform: hello.platform.clone(),
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
        // A revoked node is told so and disconnected by the revoking task;
        // ignore whatever it still sends until then.
        if !st.p.nodes.contains_key(&node.0) {
            continue;
        }
        match msg {
            ToHub::Update(m) => {
                drop(st);
                on_update_msg(hub, node, m);
            }
            ToHub::Ping { nonce } => st.send_to(node, FromHub::Pong { nonce }),
            ToHub::Pong { .. } => {}
            ToHub::NetStatus { away, addrs } => {
                let Some(o) = st.online.get_mut(&node) else { continue };
                let changed = o.away != away;
                o.away = away;
                o.local_addrs = addrs;
                if changed {
                    let addrs: Vec<String> = o.local_addrs.iter().map(|a| a.to_string()).collect();
                    if away {
                        info!(%node, ?addrs, "node is away (not on an allowed network)");
                        // Agents release on their own too; this covers
                        // sources that have not heard about it yet.
                        st.release_focus_on(node);
                        st.focus.remove(&node);
                        st.broadcast_focus();
                    } else {
                        info!(%node, ?addrs, "node is back on an allowed network");
                    }
                    st.broadcast_peers();
                }
            }
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
        StreamKind::Fetch => {
            let builds = hub.lock().builds.clone();
            crate::updates::serve_fetch(builds, node, send, recv).await
        }
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
            if !enabled || st.is_away(node) || !content.allowed_by(&st.p.settings.clipboard) {
                debug!(%node, ?content, "clipboard update dropped by settings");
                continue;
            }
            st.last_clip = Some(ClipMeta { from: node, kind: content.kind(), bytes: content.len(), at: unix_now() });
            let mut n = 0;
            for (id, o) in st.online.iter() {
                let target_enabled = st.p.nodes.get(&id.0).is_some_and(|n| n.enabled);
                if *id != node && target_enabled && !o.away {
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
