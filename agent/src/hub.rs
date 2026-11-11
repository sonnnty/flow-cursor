//! Connection to the hub: enrolment/authentication, control stream, relay
//! (stream + datagrams) and clipboard stream. Reconnects with backoff.

use crate::backend::Backend;
use crate::config::{resolve_hub, AgentConfig, AgentState};
use anyhow::{anyhow, bail, Context, Result};
use flow_net::endpoint::{client_config, TransportOpts, SNI};
use flow_net::frame::{read_frame, write_frame};
use flow_net::link::p2p::{LinkReceiver, LinkSink};
use flow_net::link::{LinkHandle, RelayTransport};
use flow_net::quinn::{Connection, Endpoint};
use flow_net::tls::{peer_fingerprint, ServerPin};
use flow_net::token::JoinToken;
use flow_net::Identity;
use flow_proto::{
    AgentSettings, ClipContent, ClipboardMsg, Fingerprint, FocusEntry, FromHub, Hello, Layout, LinkDatagram, LinkMsg,
    NodeId, OsKind, PeerInfo, RelayDatagramDown, RelayDatagramUp, RelayDown, RelayUp, StreamKind, ToHub, Welcome,
    ALPN_HUB, MAX_CLIPBOARD_FRAME, MAX_CONTROL_FRAME, PROTOCOL_VERSION,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// What the hub tells the agent core.
#[derive(Debug)]
pub enum HubEvent {
    Connected(Box<Welcome>),
    Disconnected,
    Layout(Layout),
    Peers(Vec<PeerInfo>),
    Settings(AgentSettings),
    Focus(Vec<FocusEntry>),
    ReleaseControl,
    Revoked,
    Rejected(String),
    /// A connection attempt or session ended with this error.
    Error(String),
    Clipboard {
        from: Option<NodeId>,
        content: ClipContent,
    },
}

/// Relay through the hub, usable by link actors.
#[derive(Default)]
pub struct HubRelay {
    inner: Mutex<Option<RelayInner>>,
}

struct RelayInner {
    conn: Connection,
    tx: mpsc::UnboundedSender<RelayUp>,
}

impl HubRelay {
    fn set(&self, v: Option<RelayInner>) {
        *self.inner.lock().expect("poisoned") = v;
    }
}

impl RelayTransport for HubRelay {
    fn send(&self, to: NodeId, msg: LinkMsg) -> bool {
        match &*self.inner.lock().expect("poisoned") {
            Some(i) => i.tx.send(RelayUp { to, msg }).is_ok(),
            None => false,
        }
    }

    fn send_datagram(&self, to: NodeId, dg: LinkDatagram) -> bool {
        match &*self.inner.lock().expect("poisoned") {
            Some(i) => i.conn.send_datagram(flow_proto::encode(&RelayDatagramUp { to, dg }).into()).is_ok(),
            None => false,
        }
    }

    fn is_up(&self) -> bool {
        self.inner.lock().expect("poisoned").as_ref().is_some_and(|i| i.conn.close_reason().is_none())
    }
}

pub type Links = Arc<Mutex<HashMap<NodeId, LinkHandle>>>;

pub struct HubClient {
    pub endpoint: Endpoint,
    pub identity: Identity,
    pub cfg: AgentConfig,
    pub state_dir: PathBuf,
    pub state: Arc<Mutex<AgentState>>,
    pub relay: Arc<HubRelay>,
    pub receiver: Arc<LinkReceiver>,
    pub links: Links,
    pub backend: Arc<dyn Backend>,
    pub events: mpsc::UnboundedSender<HubEvent>,
    pub p2p_port: u16,
}

pub struct HubChannels {
    pub to_hub: mpsc::UnboundedReceiver<ToHub>,
    pub clip: mpsc::UnboundedReceiver<ClipContent>,
}

enum End {
    Retry(Duration),
}

/// Reconnect backoff: starts here, doubles per failed attempt up to
/// [`BACKOFF_MAX`].
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(10);
/// A session that lasted this long counts as healthy: when it drops, the
/// backoff starts small again instead of continuing from where an earlier
/// outage left it.
const HEALTHY_SESSION: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attempt {
    /// The session ended in a way that asks for a fixed delay.
    Retry(Duration),
    /// Connecting failed or the session broke; `healthy` = it had been up
    /// for at least [`HEALTHY_SESSION`].
    Failed { healthy: bool },
}

/// (delay before the next attempt, new backoff).
fn next_backoff(backoff: Duration, a: Attempt) -> (Duration, Duration) {
    match a {
        Attempt::Retry(d) => (d, backoff),
        Attempt::Failed { healthy: true } => (BACKOFF_MIN, BACKOFF_MIN),
        Attempt::Failed { healthy: false } => {
            let b = (backoff * 2).min(BACKOFF_MAX);
            (b, b)
        }
    }
}

impl HubClient {
    pub async fn run(self, mut ch: HubChannels) {
        let mut backoff = BACKOFF_MIN;
        loop {
            let mut session_start = None;
            let attempt = match self.connect_once(&mut ch, &mut session_start).await {
                Ok(End::Retry(d)) => Attempt::Retry(d),
                Err(e) => {
                    warn!("hub connection: {e:#}");
                    let _ = self.events.send(HubEvent::Error(format!("{e:#}")));
                    Attempt::Failed { healthy: session_start.is_some_and(|t: Instant| t.elapsed() >= HEALTHY_SESSION) }
                }
            };
            let delay;
            (delay, backoff) = next_backoff(backoff, attempt);
            self.relay.set(None);
            let _ = self.events.send(HubEvent::Disconnected);
            // Drop what was queued while disconnected.
            while ch.to_hub.try_recv().is_ok() {}
            while ch.clip.try_recv().is_ok() {}
            tokio::time::sleep(delay).await;
        }
    }

    fn pin_and_token(&self) -> Result<(ServerPin, Option<String>)> {
        let st = self.state.lock().expect("poisoned");
        let token = if st.node_id.is_some() {
            None
        } else if let Some(t) = &st.pending_join_token {
            Some(t.clone())
        } else if !st.revoked {
            self.cfg.join_token.clone()
        } else {
            None
        };
        let fp = self
            .cfg
            .hub_fingerprint
            .as_deref()
            .or(st.hub_fingerprint.as_deref())
            .map(|s| Fingerprint::from_hex(s).ok_or_else(|| anyhow!("hub_fingerprint is not 64 hex digits")))
            .transpose()?;
        let pin = match (fp, &token) {
            (Some(fp), _) => ServerPin::Exact(fp),
            (None, Some(t)) => {
                let jt = JoinToken::parse(t).ok_or_else(|| anyhow!("join token is malformed"))?;
                ServerPin::Prefix(jt.hub_fp_prefix.to_vec())
            }
            (None, None) if st.revoked => {
                bail!("this agent was removed from the hub; run `flow-agent join <token>` to join again")
            }
            (None, None) => {
                bail!("not joined yet; run `flow-agent join <token>` (token is shown in the hub dashboard)")
            }
        };
        Ok((pin, token))
    }

    async fn hub_addr(&self, pin: &ServerPin) -> Result<SocketAddr> {
        if let Some(h) = self.cfg.hub.as_deref().filter(|h| !h.trim().is_empty()) {
            return resolve_hub(h).await;
        }
        let found = tokio::task::spawn_blocking(|| flow_net::mdns::discover(Duration::from_secs(3)))
            .await
            .context("mDNS task")?
            .unwrap_or_default();
        // Prefer an advertisement matching our pin; the TLS pin is what
        // actually authenticates the hub.
        let best = found.iter().find(|a| a.fingerprint.as_ref().is_some_and(|f| pin.matches(f))).or(found.first());
        if let Some(a) = best.and_then(|a| a.addrs.first()) {
            info!(addr = %a, "found hub via mDNS");
            return Ok(*a);
        }
        let last = self.state.lock().expect("poisoned").hub_addr.clone();
        match last {
            Some(a) => resolve_hub(&a).await,
            None => bail!("no hub address configured and none found via mDNS"),
        }
    }

    /// One connection. `session_start` is set once the hub welcomed us.
    async fn connect_once(&self, ch: &mut HubChannels, session_start: &mut Option<Instant>) -> Result<End> {
        let (pin, token) = self.pin_and_token()?;
        let addr = self.hub_addr(&pin).await?;
        let cc = client_config(&self.identity, ALPN_HUB, pin, TransportOpts::HUB)?;
        let conn = tokio::time::timeout(Duration::from_secs(5), self.endpoint.connect_with(cc, addr, SNI)?)
            .await
            .map_err(|_| anyhow!("timeout connecting to hub at {addr}"))?
            .with_context(|| format!("connecting to hub at {addr} (wrong address or hub fingerprint?)"))?;
        let hub_fp = peer_fingerprint(&conn).ok_or_else(|| anyhow!("hub sent no certificate"))?;
        let (mut send, mut recv) = conn.open_bi().await?;
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            join_token: token.clone(),
            hostname: hostname::get().ok().and_then(|h| h.into_string().ok()).unwrap_or_else(|| "unknown".into()),
            os: OsKind::current(),
            monitors: self.backend.monitors(),
            caps: self.backend.caps(),
            p2p_port: self.p2p_port,
            p2p_addrs: self.cfg.p2p_advertise.clone(),
        };
        write_frame(&mut send, &ToHub::Hello(hello)).await?;
        let welcome =
            match tokio::time::timeout(Duration::from_secs(10), read_frame::<_, FromHub>(&mut recv, MAX_CONTROL_FRAME))
                .await
                .map_err(|_| anyhow!("hub did not answer"))??
            {
                Some(FromHub::Welcome(w)) => w,
                Some(FromHub::Reject { reason }) => {
                    error!("hub rejected this agent: {reason}");
                    let _ = self.events.send(HubEvent::Rejected(reason));
                    return Ok(End::Retry(Duration::from_secs(30)));
                }
                other => bail!("unexpected hub reply: {other:?}"),
            };
        {
            let mut st = self.state.lock().expect("poisoned");
            st.node_id = Some(welcome.node_id.0);
            st.hub_fingerprint = Some(hub_fp.to_hex());
            st.hub_addr = Some(addr.to_string());
            st.pending_join_token = None;
            st.revoked = false;
            if let Err(e) = st.save(&self.state_dir) {
                warn!("saving agent state: {e:#}");
            }
        }
        *session_start = Some(Instant::now());
        if token.is_some() {
            info!(node = %welcome.node_id, name = %welcome.name, hub_fingerprint = %hub_fp, "joined hub; hub fingerprint pinned");
        }
        info!(node = %welcome.node_id, %addr, "connected to hub");

        // Relay stream.
        let (mut rs, mut rr) = conn.open_bi().await?;
        write_frame(&mut rs, &StreamKind::Relay).await?;
        let (relay_tx, mut relay_rx) = mpsc::unbounded_channel::<RelayUp>();
        let mut tasks = Vec::new();
        tasks.push(tokio::spawn(async move {
            while let Some(m) = relay_rx.recv().await {
                if write_frame(&mut rs, &m).await.is_err() {
                    break;
                }
            }
        }));
        {
            let receiver = self.receiver.clone();
            let links = self.links.clone();
            let relay = self.relay.clone();
            tasks.push(tokio::spawn(async move {
                while let Ok(Some(down)) = read_frame::<_, RelayDown>(&mut rr, MAX_CONTROL_FRAME).await {
                    dispatch_relay(&receiver, &links, &relay, down);
                }
            }));
        }
        {
            let receiver = self.receiver.clone();
            let links = self.links.clone();
            let relay = self.relay.clone();
            let c = conn.clone();
            tasks.push(tokio::spawn(async move {
                while let Ok(b) = c.read_datagram().await {
                    if let Ok(d) = flow_proto::decode::<RelayDatagramDown>(&b) {
                        dispatch_relay_dg(&receiver, &links, &relay, d);
                    }
                }
            }));
        }
        // Clipboard stream.
        let (mut cs, mut cr) = conn.open_bi().await?;
        write_frame(&mut cs, &StreamKind::Clipboard).await?;
        let (clip_tx, mut clip_rx) = mpsc::unbounded_channel::<ClipboardMsg>();
        tasks.push(tokio::spawn(async move {
            while let Some(m) = clip_rx.recv().await {
                if write_frame(&mut cs, &m).await.is_err() {
                    break;
                }
            }
        }));
        {
            let ev = self.events.clone();
            tasks.push(tokio::spawn(async move {
                while let Ok(Some(ClipboardMsg::Update { from, content })) =
                    read_frame::<_, ClipboardMsg>(&mut cr, MAX_CLIPBOARD_FRAME).await
                {
                    let _ = ev.send(HubEvent::Clipboard { from, content });
                }
            }));
        }
        // Control writer.
        let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<ToHub>();
        tasks.push(tokio::spawn(async move {
            while let Some(m) = ctl_rx.recv().await {
                if write_frame(&mut send, &m).await.is_err() {
                    break;
                }
            }
        }));

        self.relay.set(Some(RelayInner { conn: conn.clone(), tx: relay_tx }));
        let _ = self.events.send(HubEvent::Connected(welcome));

        let result = loop {
            tokio::select! {
                f = read_frame::<_, FromHub>(&mut recv, MAX_CONTROL_FRAME) => match f {
                    Ok(Some(m)) => {
                        let ev = match m {
                            FromHub::Layout(l) => HubEvent::Layout(l),
                            FromHub::Peers(p) => HubEvent::Peers(p),
                            FromHub::Settings(s) => HubEvent::Settings(s),
                            FromHub::Focus(f) => HubEvent::Focus(f),
                            FromHub::ReleaseControl => HubEvent::ReleaseControl,
                            FromHub::Ping { nonce } => { let _ = ctl_tx.send(ToHub::Pong { nonce }); continue; }
                            FromHub::Pong { .. } | FromHub::Welcome(_) => continue,
                            FromHub::Reject { reason } => HubEvent::Rejected(reason),
                            FromHub::Revoked => {
                                self.on_revoked();
                                break Ok(End::Retry(Duration::from_secs(60)));
                            }
                        };
                        let _ = self.events.send(ev);
                    }
                    Ok(None) => break Err(anyhow!("hub closed the connection")),
                    Err(e) => break Err(anyhow!("hub control stream: {e}")),
                },
                m = ch.to_hub.recv() => match m {
                    Some(m) => { let _ = ctl_tx.send(m); }
                    None => break Ok(End::Retry(Duration::from_secs(3600))),
                },
                c = ch.clip.recv() => if let Some(content) = c {
                    debug!(?content, "sending clipboard to hub");
                    let _ = clip_tx.send(ClipboardMsg::Update { from: None, content });
                },
                reason = conn.closed() => break Err(anyhow!("hub connection closed: {reason}")),
            }
        };
        // The hub signals revocation in-band and with the close code; the
        // close may win the race against the frame.
        if result.is_err() {
            // Give a pending close (with its reason code) a moment to arrive.
            let _ = tokio::time::timeout(Duration::from_millis(500), conn.closed()).await;
        }
        let result = match conn.close_reason() {
            Some(flow_net::quinn::ConnectionError::ApplicationClosed(c))
                if c.error_code == flow_proto::CLOSE_REVOKED.into() =>
            {
                self.on_revoked();
                Ok(End::Retry(Duration::from_secs(60)))
            }
            _ => result,
        };
        self.relay.set(None);
        for t in tasks {
            t.abort();
        }
        conn.close(0u32.into(), b"bye");
        info!("disconnected from hub");
        result
    }
}

impl HubClient {
    fn on_revoked(&self) {
        let mut st = self.state.lock().expect("poisoned");
        if st.revoked {
            return;
        }
        warn!("the hub removed this agent (credentials revoked)");
        st.node_id = None;
        st.revoked = true;
        st.pending_join_token = None;
        let _ = st.save(&self.state_dir);
        drop(st);
        let _ = self.events.send(HubEvent::Revoked);
    }
}

pub fn dispatch_relay(receiver: &LinkReceiver, links: &Links, relay: &HubRelay, down: RelayDown) {
    match down.msg {
        LinkMsg::Event { .. } => {
            if let Some(reply) = receiver.on_msg(down.from, down.msg) {
                relay.send(down.from, reply);
            }
        }
        LinkMsg::Ack { .. } => {
            if let Some(l) = links.lock().expect("poisoned").get(&down.from) {
                l.deliver(down.msg);
            }
        }
    }
}

pub fn dispatch_relay_dg(receiver: &LinkReceiver, links: &Links, relay: &HubRelay, d: RelayDatagramDown) {
    match d.dg {
        LinkDatagram::Pong { .. } => {
            if let Some(l) = links.lock().expect("poisoned").get(&d.from) {
                l.deliver_datagram(d.dg);
            }
        }
        _ => {
            if let Some(reply) = receiver.on_datagram(d.from, d.dg) {
                relay.send_datagram(d.from, reply);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_resets_after_a_healthy_session() {
        let mut b = BACKOFF_MIN;
        let mut delays = Vec::new();
        for _ in 0..7 {
            let d;
            (d, b) = next_backoff(b, Attempt::Failed { healthy: false });
            delays.push(d.as_millis());
        }
        assert_eq!(delays, [1000, 2000, 4000, 8000, 10000, 10000, 10000]);
        // A long healthy session dropped: start small again.
        let (d, b2) = next_backoff(b, Attempt::Failed { healthy: true });
        assert_eq!((d, b2), (BACKOFF_MIN, BACKOFF_MIN));
        // Next failure doubles from the small value, not from 10 s.
        assert_eq!(next_backoff(b2, Attempt::Failed { healthy: false }).0, Duration::from_secs(1));
        // A short-lived session that keeps dropping keeps backing off.
        assert_eq!(next_backoff(b, Attempt::Failed { healthy: false }).0, BACKOFF_MAX);
        // Explicit retry delays do not touch the backoff.
        assert_eq!(next_backoff(b2, Attempt::Retry(Duration::from_secs(30))), (Duration::from_secs(30), b2));
    }
}
