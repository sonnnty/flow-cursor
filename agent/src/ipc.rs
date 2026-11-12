//! Local IPC server: lets the tray icon and the status window (`flow-tray`)
//! read the agent's state and control it. Protocol and endpoint are in the
//! `flow-ipc` crate; this module serves them.
//!
//! Access is limited to the user running the agent: on Unix the socket is
//! 0600 inside a 0700 directory owned by the user and every peer's uid is
//! checked (`SO_PEERCRED`); on Windows the pipe's DACL only admits the user.
//!
//! Commands that end the running agent (restart, join, shutdown) are
//! answered and then handed to the binary as a [`Control`]: it owns the
//! process and decides how to restart (re-exec on Unix, a detached new
//! process on Windows).

use crate::config::AgentConfig;
use crate::core::{AgentObserver, AgentStatus};
use crate::logbuf::LogBuffer;
use crate::update::UpdateStatus;
use flow_ipc::{encode_line, parse_request, Command, PeerRef, PeerStatus, Reply, RequestError, Snapshot, UpdateInfo};
use flow_net::token::JoinToken;
use flow_proto::update::UpdatePhase;
use flow_proto::NodeId;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Requests that the binary carries out after the reply went out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    Restart,
    /// Store the token (and hub address) like `flow-agent join`, then restart.
    Join {
        token: String,
        hub: Option<String>,
    },
    Shutdown,
    /// An update was installed: restart into it.
    Update(crate::update::RestartRequest),
}

/// Static facts about this agent process for the snapshot.
#[derive(Clone, Debug, Default)]
pub struct AgentInfo {
    pub config_file: PathBuf,
    pub log_file: Option<PathBuf>,
    pub cfg: AgentConfig,
}

/// Everything a connection handler needs.
#[derive(Clone)]
pub struct IpcContext {
    pub agent: AgentObserver,
    pub info: Arc<AgentInfo>,
    pub logs: LogBuffer,
    pub control: mpsc::UnboundedSender<Control>,
}

/// Why the endpoint could not be opened.
#[derive(Debug)]
pub enum BindError {
    /// Another agent of this user answers on the endpoint.
    AlreadyRunning(PathBuf),
    Io(io::Error),
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindError::AlreadyRunning(p) => {
                write!(f, "another flow-agent of this user is already running (IPC endpoint {})", p.display())
            }
            BindError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BindError {}

impl From<io::Error> for BindError {
    fn from(e: io::Error) -> Self {
        BindError::Io(e)
    }
}

// ----------------------------------------------------------------------
// snapshot
// ----------------------------------------------------------------------

/// The status snapshot sent to clients.
pub fn snapshot(ctx: &IpcContext) -> Snapshot {
    let st: AgentStatus = ctx.agent.status();
    let state = ctx.agent.agent_state();
    let cfg = &ctx.info.cfg;
    let backend = ctx.agent.backend();
    let caps = backend.caps();
    let settings = st.settings.clone().unwrap_or_default();
    let name_of = |id: NodeId| {
        st.peers.iter().find(|p| p.id == id).map(|p| p.name.clone()).unwrap_or_else(|| format!("node-{}", id.0))
    };
    let peer_ref = |id: NodeId| PeerRef { id: id.0, name: name_of(id) };

    let peers = st
        .peers
        .iter()
        .map(|p| {
            let link = if p.online { ctx.agent.link_status(p.id) } else { None };
            PeerStatus {
                id: p.id.0,
                name: p.name.clone(),
                online: p.online,
                enabled: p.enabled,
                away: p.away,
                path: link.as_ref().map(|l| l.path.to_string()),
                rtt_ms: link.and_then(|l| l.rtt_us).map(|us| f64::from(us) / 1000.0),
            }
        })
        .collect();

    // A session that is closed on purpose (away, paused, not connected,
    // update-only) is why inject and clipboard are missing then: not a
    // problem. One that should be open and is not (refused, portal missing,
    // failed to open) is.
    let report = crate::controllable::session_report(
        backend.session_support(),
        st.session_idle,
        caps.inject,
        caps.clipboard,
        st.session_wanted_since.map(|t| t.elapsed()),
    );
    let mut problems = Vec::new();
    if !caps.capture {
        problems.push("This machine cannot capture its mouse and keyboard: it cannot control other machines.".into());
    }
    if report.inject_problem {
        problems.push("This machine cannot inject input: other machines cannot control it.".into());
    }
    problems.extend(crate::problems::list());
    let mut warnings = Vec::new();
    if report.clipboard_warning {
        warnings.push("No clipboard access: clipboard sharing is off on this machine.".into());
    }

    let hub_address = cfg.hub.clone().filter(|h| !h.trim().is_empty());
    let dashboard_url = hub_address.as_deref().or(state.hub_addr.as_deref()).and_then(dashboard_url);
    let hotkey = cfg
        .emergency_hotkey
        .clone()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| settings.emergency_hotkey.clone());

    Snapshot {
        agent_version: flow_version::FULL.to_string(),
        platform: flow_version::PLATFORM.to_string(),
        pid: std::process::id(),
        backend: backend.name().to_string(),
        config_file: ctx.info.config_file.display().to_string(),
        state_dir: cfg.state_dir().display().to_string(),
        log_file: ctx.info.log_file.as_ref().map(|p| p.display().to_string()),
        hub_address,
        hub_last_address: state.hub_addr.clone(),
        hub_connected: st.connected,
        hub_error: if st.connected { None } else { st.hub_error.clone() },
        hub_fingerprint: cfg.hub_fingerprint.clone().or(state.hub_fingerprint.clone()),
        dashboard_url,
        node_id: st.node_id.map(|n| n.0).or(state.node_id),
        node_name: st.node_name.clone(),
        pending_join: state.pending_join_token.is_some() || (state.node_id.is_none() && cfg.join_token.is_some()),
        revoked: st.revoked || state.revoked,
        rejected: st.rejected.clone(),
        paused: st.paused,
        away: st.away,
        allowed_networks: settings.allowed_networks.clone(),
        local_addrs: st.local_addrs.iter().map(|a| a.to_string()).collect(),
        controlling: st.controlling.map(peer_ref),
        controlled_by: st.controlled_by.iter().map(|&id| peer_ref(id)).collect(),
        holding_edge: st.holding_edge,
        peers,
        crossing_key: settings.crossing_key.clone(),
        emergency_hotkey: hotkey,
        can_capture: caps.capture,
        can_inject: caps.inject,
        can_clipboard: caps.clipboard,
        controllable: st.controllable,
        session_wanted: st.session_wanted,
        session_idle: st.session_idle,
        problems,
        warnings,
        update: update_info(&ctx.agent.updater.status()),
    }
}

/// The updater's state as the tray shows it.
pub fn update_info(u: &UpdateStatus) -> UpdateInfo {
    let target = match &u.phase {
        UpdatePhase::Available { target }
        | UpdatePhase::Downloading { target, .. }
        | UpdatePhase::Ready { target }
        | UpdatePhase::Installing { target } => Some(target.full()),
        UpdatePhase::Current | UpdatePhase::Unsupported { .. } => None,
    };
    let progress = match &u.phase {
        UpdatePhase::Downloading { done, total, .. } if *total > 0 => Some((done * 100 / total).min(100) as u8),
        _ => None,
    };
    UpdateInfo {
        policy: u.policy.map(|p| p.as_str().to_string()).unwrap_or_default(),
        state: u.phase.name().to_string(),
        can_update_now: matches!(u.phase, UpdatePhase::Available { .. }),
        available: target,
        progress,
        detail: match &u.phase {
            UpdatePhase::Unsupported { reason } => Some(reason.clone()),
            _ => None,
        },
        failed: u.failed.as_ref().map(|f| format!("{}: {}", f.target, f.reason)),
        failed_rolled_back: u.failed.as_ref().is_some_and(|f| f.rolled_back),
        update_only: u.update_only.clone(),
        tray_build: u.tray_build.clone(),
    }
}

/// `http://<hub host>:7880` from a hub address (`host`, `host:port`,
/// `ip`, `[v6]:port`).
pub fn dashboard_url(hub: &str) -> Option<String> {
    let hub = hub.trim();
    if hub.is_empty() {
        return None;
    }
    let host = if let Ok(a) = hub.parse::<std::net::SocketAddr>() {
        a.ip().to_string()
    } else if let Ok(ip) = hub.parse::<std::net::IpAddr>() {
        ip.to_string()
    } else if let Some(rest) = hub.strip_prefix('[') {
        rest.split(']').next()?.to_string()
    } else {
        match hub.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => h.to_string(),
            _ => hub.to_string(),
        }
    };
    let host = if host.contains(':') { format!("[{host}]") } else { host };
    Some(format!("http://{host}:{}", flow_proto::DEFAULT_HTTP_PORT))
}

// ----------------------------------------------------------------------
// connection handling (transport independent)
// ----------------------------------------------------------------------

/// Reads one request line of at most [`flow_ipc::MAX_LINE`] bytes.
async fn read_request<R: AsyncRead + Unpin>(r: &mut BufReader<R>) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = (&mut *r).take(flow_ipc::MAX_LINE as u64 + 1).read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "IPC request too long or cut off"));
    }
    buf.pop();
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

async fn send<W: AsyncWrite + Unpin>(w: &mut W, reply: &Reply) -> io::Result<()> {
    w.write_all(&encode_line(reply)).await?;
    w.flush().await
}

fn status_reply(ctx: &IpcContext) -> Reply {
    Reply::Status { status: Box::new(snapshot(ctx)) }
}

/// Serves one client until it hangs up.
pub async fn handle_connection<S>(ctx: IpcContext, stream: S) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (r, mut w) = tokio::io::split(stream);
    let mut r = BufReader::new(r);
    while let Some(line) = read_request(&mut r).await? {
        let cmd = match parse_request(&line) {
            Ok(c) => c,
            Err(e @ (RequestError::Version(_) | RequestError::Command(_) | RequestError::Malformed(_))) => {
                debug!("IPC: {e}");
                send(&mut w, &Reply::Error { message: e.to_string() }).await?;
                continue;
            }
        };
        let reply = match cmd {
            Command::Status => status_reply(&ctx),
            Command::Subscribe => return subscribe(ctx, r, w).await,
            Command::Logs { max } => Reply::Logs { lines: ctx.logs.tail(max) },
            Command::Pause => {
                ctx.agent.set_paused(true);
                Reply::Ok { message: "sharing paused".into() }
            }
            Command::Resume => {
                ctx.agent.set_paused(false);
                Reply::Ok { message: "sharing resumed".into() }
            }
            Command::UpdateNow => {
                let u = ctx.agent.updater.status();
                if matches!(u.phase, UpdatePhase::Available { .. } | UpdatePhase::Ready { .. }) {
                    info!("update requested over IPC");
                    ctx.agent.updater.update_now();
                    Reply::Ok { message: "updating as soon as this machine is idle".into() }
                } else {
                    let why = match &u.phase {
                        UpdatePhase::Unsupported { reason } => reason.clone(),
                        UpdatePhase::Current => "already running the hub's build".into(),
                        p => format!("an update is already {}", p.name()),
                    };
                    Reply::Error { message: format!("nothing to update: {why}") }
                }
            }
            Command::Restart => {
                info!("restart requested over IPC");
                let _ = ctx.control.send(Control::Restart);
                Reply::Ok { message: "restarting".into() }
            }
            Command::Join { token, hub } => match JoinToken::parse(token.trim()) {
                None => Reply::Error { message: "this does not look like a FlowCursor join token (FC1-…)".into() },
                Some(t) => {
                    let hub = hub.map(|h| h.trim().to_string()).filter(|h| !h.is_empty());
                    info!(hub = ?hub, "join requested over IPC");
                    let _ = ctx.control.send(Control::Join { token: t.encode(), hub });
                    Reply::Ok { message: "token stored, restarting to join".into() }
                }
            },
            Command::Shutdown => {
                info!("shutdown requested over IPC");
                let _ = ctx.control.send(Control::Shutdown);
                Reply::Ok { message: "stopping".into() }
            }
        };
        send(&mut w, &reply).await?;
    }
    Ok(())
}

/// Pushes a snapshot now, on every state change (coalesced to at most one
/// per 100 ms) and at least every [`flow_ipc::SUBSCRIBE_HEARTBEAT_MS`],
/// until the client hangs up.
async fn subscribe<R, W>(ctx: IpcContext, mut r: BufReader<R>, mut w: W) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut changes = ctx.agent.watch();
    let mut heartbeat = tokio::time::interval(Duration::from_millis(flow_ipc::SUBSCRIBE_HEARTBEAT_MS));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last: Option<Snapshot> = None;
    let mut sink = Vec::new();
    loop {
        let heartbeat_due = tokio::select! {
            _ = heartbeat.tick() => true,
            c = changes.changed() => {
                if c.is_err() {
                    // The agent core stopped (restart in progress).
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                changes.borrow_and_update();
                false
            }
            // Anything the client sends after subscribing is ignored; EOF
            // ends the subscription.
            n = r.read_buf(&mut sink) => {
                if n? == 0 {
                    return Ok(());
                }
                sink.clear();
                continue;
            }
        };
        let snap = snapshot(&ctx);
        if !heartbeat_due && last.as_ref() == Some(&snap) {
            continue;
        }
        send(&mut w, &Reply::Status { status: Box::new(snap.clone()) }).await?;
        last = Some(snap);
    }
}

// ----------------------------------------------------------------------
// transports
// ----------------------------------------------------------------------

/// A bound endpoint; serves clients until dropped (Unix: removes the
/// socket file on drop).
pub struct IpcServer {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl IpcServer {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stops accepting, closes the endpoint and waits until it is released
    /// (connections already accepted keep running until their client
    /// hangs up or the process exits).
    pub async fn close(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.task.abort();
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Opens the endpoint at `path` and serves `ctx` on it.
pub fn serve(path: &Path, ctx: IpcContext) -> Result<IpcServer, BindError> {
    let task = platform::bind_and_serve(path, ctx)?;
    info!(endpoint = %path.display(), "local IPC endpoint up");
    Ok(IpcServer { path: path.to_path_buf(), task })
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use tokio::net::UnixListener;

    /// Peers of another uid are refused (root included: it can read the
    /// agent's files anyway, but has no business driving it).
    pub fn peer_allowed(peer_uid: u32, me: u32) -> bool {
        peer_uid == me
    }

    pub fn bind_and_serve(path: &Path, ctx: IpcContext) -> Result<tokio::task::JoinHandle<()>, BindError> {
        let dir = path.parent().ok_or_else(|| io::Error::other("IPC endpoint has no parent directory"))?;
        flow_ipc::endpoint::ensure_private_dir(dir)?;
        match std::fs::symlink_metadata(path) {
            Ok(md) if md.file_type().is_socket() => {
                if std::os::unix::net::UnixStream::connect(path).is_ok() {
                    return Err(BindError::AlreadyRunning(path.to_path_buf()));
                }
                // Left behind by an agent that did not exit cleanly.
                std::fs::remove_file(path)?;
            }
            Ok(_) => {
                return Err(BindError::Io(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", path.display()),
                )))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let me = flow_ipc::endpoint::current_uid();
        Ok(tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("IPC accept: {e}");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                };
                match stream.peer_cred() {
                    Ok(c) if peer_allowed(c.uid(), me) => {}
                    Ok(c) => {
                        warn!(uid = c.uid(), "IPC connection from another user refused");
                        continue;
                    }
                    Err(e) => {
                        warn!("IPC peer credentials: {e}");
                        continue;
                    }
                }
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(ctx, stream).await {
                        debug!("IPC connection: {e}");
                    }
                });
            }
        }))
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    const ERROR_ACCESS_DENIED: i32 = 5;

    fn create(path: &Path, sa: &mut flow_ipc::windows::OwnerOnly, first: bool) -> io::Result<NamedPipeServer> {
        let mut o = ServerOptions::new();
        o.first_pipe_instance(first).reject_remote_clients(true);
        // SAFETY: `sa` points to valid SECURITY_ATTRIBUTES that outlive
        // the call (the descriptor is owned by `OwnerOnly`).
        unsafe { o.create_with_security_attributes_raw(path, sa.as_ptr()) }
    }

    pub fn bind_and_serve(path: &Path, ctx: IpcContext) -> Result<tokio::task::JoinHandle<()>, BindError> {
        let mut sa = flow_ipc::windows::OwnerOnly::new()?;
        // The first instance fails with ACCESS_DENIED when the name exists:
        // another agent (or someone squatting the name).
        let first = match create(path, &mut sa, true) {
            Ok(s) => s,
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                return Err(BindError::AlreadyRunning(path.to_path_buf()))
            }
            Err(e) => return Err(e.into()),
        };
        let path = path.to_path_buf();
        Ok(tokio::spawn(async move {
            let mut server = first;
            loop {
                if let Err(e) = server.connect().await {
                    warn!("IPC pipe connect: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                let next = loop {
                    match create(&path, &mut sa, false) {
                        Ok(s) => break s,
                        Err(e) => {
                            warn!("creating the next IPC pipe instance: {e}");
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                    }
                };
                let client = std::mem::replace(&mut server, next);
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(ctx, client).await {
                        debug!("IPC connection: {e}");
                    }
                });
            }
        }))
    }
}

#[cfg(unix)]
pub use platform::peer_allowed;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_urls() {
        assert_eq!(dashboard_url("192.168.1.10").as_deref(), Some("http://192.168.1.10:7880"));
        assert_eq!(dashboard_url("192.168.1.10:7878").as_deref(), Some("http://192.168.1.10:7880"));
        assert_eq!(dashboard_url("hub.lan").as_deref(), Some("http://hub.lan:7880"));
        assert_eq!(dashboard_url("hub.lan:9000").as_deref(), Some("http://hub.lan:7880"));
        assert_eq!(dashboard_url("[fd00::1]:7878").as_deref(), Some("http://[fd00::1]:7880"));
        assert_eq!(dashboard_url("fd00::1").as_deref(), Some("http://[fd00::1]:7880"));
        assert_eq!(dashboard_url("  "), None);
    }

    #[cfg(unix)]
    #[test]
    fn only_the_same_uid_may_connect() {
        assert!(peer_allowed(1000, 1000));
        assert!(!peer_allowed(0, 1000), "not even root");
        assert!(!peer_allowed(1001, 1000));
    }
}
