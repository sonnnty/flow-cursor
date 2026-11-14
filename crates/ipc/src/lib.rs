//! Local IPC between a running `flow-agent` and the programs that show its
//! state (the tray icon and the status window).
//!
//! Transport: a Unix socket in the user's runtime directory (mode 0600 in a
//! 0700 directory, peers checked with `SO_PEERCRED`) or, on Windows, a named
//! pipe whose DACL only admits the current user. See [`endpoint`].
//!
//! Framing: one JSON object per line (UTF-8, `\n`-terminated, at most
//! [`MAX_LINE`] bytes). Every message carries `"v"`, the protocol version
//! ([`IPC_VERSION`]); a server answers a request of another version with an
//! [`Reply::Error`] and keeps the connection open. Within a version, fields
//! are only ever added (all snapshot fields default when missing), so an
//! older tray keeps working with a newer agent and vice versa.
//!
//! A connection carries requests and their replies in order. After
//! [`Command::Subscribe`] the server pushes a [`Reply::Status`] whenever the
//! state changes and at least every [`SUBSCRIBE_HEARTBEAT_MS`].

use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};

pub mod client;
pub mod detach;
pub mod endpoint;
#[cfg(windows)]
pub mod windows;

pub use client::Client;

/// Version of the messages below. Bump on incompatible changes only.
pub const IPC_VERSION: u32 = 1;
/// Longest accepted line (a snapshot is a few KiB; logs up to ~200 KiB).
pub const MAX_LINE: usize = 1 << 20;
/// A subscription sends a snapshot at least this often (path RTTs change
/// without a state change; also lets clients detect a hung agent).
pub const SUBSCRIBE_HEARTBEAT_MS: u64 = 2000;
/// Default number of log lines returned by [`Command::Logs`].
pub const DEFAULT_LOG_LINES: usize = 200;

/// A request from a client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// One [`Reply::Status`].
    Status,
    /// [`Reply::Status`] now and on every change until the client hangs up.
    Subscribe,
    /// The most recent log lines of the agent ([`Reply::Logs`]).
    Logs {
        #[serde(default = "default_log_lines")]
        max: usize,
    },
    /// Stop sharing like `away`: no edge crossing, refuse being controlled,
    /// no clipboard. Not persisted: a restarted agent shares again.
    Pause,
    Resume,
    /// Restart the agent process (re-reads its configuration).
    Restart,
    /// Store a join token (and optionally the hub address) like
    /// `flow-agent join`, then restart to enrol.
    Join {
        token: String,
        #[serde(default)]
        hub: Option<String>,
    },
    /// Stop the agent.
    Shutdown,
    /// Install the build the hub carries as soon as this machine is idle
    /// (any update policy; also retries a build that failed before).
    UpdateNow,
}

fn default_log_lines() -> usize {
    DEFAULT_LOG_LINES
}

/// A reply or pushed update from the agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Status { status: Box<Snapshot> },
    Logs { lines: Vec<String> },
    Ok { message: String },
    Error { message: String },
}

/// Wire envelope: the version next to the payload.
#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    v: u32,
    #[serde(flatten)]
    body: T,
}

/// Everything the tray and the status window show about the agent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    /// Full build identity (`0.2.0+142.g1a2b3c4d5e`, see `flow-version`).
    pub agent_version: String,
    /// `linux-x86_64`, `windows-x86_64`.
    pub platform: String,
    pub pid: u32,
    /// Input backend in use (wayland, x11, windows, sim).
    pub backend: String,
    pub config_file: String,
    pub state_dir: String,
    /// File the agent logs to (None: stdout / the systemd journal).
    pub log_file: Option<String>,

    /// Configured hub address (None: mDNS discovery).
    pub hub_address: Option<String>,
    /// Last hub address that worked (also set with mDNS discovery).
    pub hub_last_address: Option<String>,
    pub hub_connected: bool,
    /// Why the last connection attempt failed (while not connected).
    pub hub_error: Option<String>,
    /// Pinned hub certificate fingerprint (hex).
    pub hub_fingerprint: Option<String>,
    /// Hub dashboard (`http://<hub host>:7880`), when the hub host is known.
    pub dashboard_url: Option<String>,

    pub node_id: Option<u32>,
    pub node_name: Option<String>,
    /// A join token is stored and waits to be used.
    pub pending_join: bool,
    /// The hub revoked this machine; it needs a new join token.
    pub revoked: bool,
    /// The hub refused the connection (reason).
    pub rejected: Option<String>,

    /// Sharing paused by the user ([`Command::Pause`]).
    pub paused: bool,
    /// Not on one of the allowed networks.
    pub away: bool,
    /// Allowed networks in effect (empty: no restriction).
    pub allowed_networks: Vec<String>,
    /// This machine's interface addresses (what `away` is decided from).
    pub local_addrs: Vec<String>,

    /// Machine this one's keyboard and mouse currently drive.
    pub controlling: Option<PeerRef>,
    /// Machines currently driving this one.
    pub controlled_by: Vec<PeerRef>,
    /// The pointer is held at an edge waiting for the crossing key.
    pub holding_edge: bool,
    pub peers: Vec<PeerStatus>,

    /// Crossing key in effect (empty: cross freely).
    pub crossing_key: String,
    /// Emergency hotkey in effect.
    pub emergency_hotkey: String,

    /// What the input backend can do on this machine.
    pub can_capture: bool,
    pub can_inject: bool,
    pub can_clipboard: bool,
    /// Other machines can control this one now (sharing is on and the
    /// backend can inject; on GNOME its remote control session is open).
    pub controllable: bool,
    /// The agent wants the backend's remote control session open (it may
    /// still be opening).
    pub session_wanted: bool,
    /// Set when the backend's remote control session is closed on purpose
    /// (so `can_inject` and, on GNOME, `can_clipboard` are false and that
    /// is no fault): the reason. None while the session is open or wanted,
    /// on backends without such a session, and from agents that predate
    /// the field (a missing inject capability is then a problem).
    pub session_idle: Option<SessionIdle>,
    /// Human-readable problems that break sharing (missing permissions,
    /// …): the tray shows the error icon. Never lists a capability that is
    /// missing only because the session is closed on purpose
    /// ([`Snapshot::session_idle`]).
    pub problems: Vec<String>,
    /// Lesser limitations (no clipboard access): listed, no error icon.
    pub warnings: Vec<String>,

    /// Self-update from the hub.
    pub update: UpdateInfo,

    /// The compositor's capture session (GNOME/Wayland InputCapture);
    /// None on backends without one and from older agents.
    pub capture: Option<CaptureInfo>,
    /// How the remote control session is made on GNOME/Wayland: `mutter
    /// direct` or `portal`. None on other backends and from older agents.
    pub remote_control_path: Option<String>,
}

/// State of the compositor's capture session: whether pointer barriers are
/// in place at the edges that lead to other machines.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureInfo {
    /// How the session is reached: `mutter direct` (no permission dialog)
    /// or `portal`. None from older agents and without a session.
    pub path: Option<String>,
    /// `enabled`, `no edges`, `opening` (waiting for the portal or its
    /// permission dialog), `re-creating`, `lost`, `refused`, `unavailable`,
    /// `not enabled`.
    pub state: String,
    /// Pointer barriers in place / refused by the compositor.
    pub barriers: u32,
    pub barriers_refused: u32,
    /// Zone set (monitor configuration serial) the barriers were set for.
    pub zone_set: Option<u32>,
    /// Capture sessions created since the agent started.
    pub sessions: u32,
    pub session_age_s: Option<u64>,
    /// Seconds since a barrier last fired.
    pub last_activation_s: Option<u64>,
    /// The last change and how many seconds ago.
    pub last_change: Option<String>,
    pub last_change_s: Option<u64>,
}

/// Why the remote control session is closed on purpose (not a fault).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionIdle {
    /// Sharing paused by the user.
    Paused,
    /// Not on an allowed network.
    Away,
    /// No full session with the hub (not connected, grace period over).
    Disconnected,
    /// Connected to a hub of another protocol, only to update.
    UpdateOnly,
    /// Any other deliberate reason (revoked, agent stopping), or one a
    /// newer agent reports and this program does not know.
    #[serde(other)]
    Other,
}

/// Self-update state of the agent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateInfo {
    /// `auto`, `notify` or `off` (empty until the hub sent its policy).
    pub policy: String,
    /// `current`, `available`, `downloading`, `ready` (waits until this
    /// machine is idle), `installing` or `unsupported`.
    pub state: String,
    /// The hub's build when it differs from this one (`0.2.1+150.gabc`).
    pub available: Option<String>,
    /// Download progress in percent.
    pub progress: Option<u8>,
    /// Why this agent cannot update itself (`unsupported`).
    pub detail: Option<String>,
    /// The last update that failed: `<build>: <reason>`.
    pub failed: Option<String>,
    /// That failure put the previous version back.
    pub failed_rolled_back: bool,
    /// Connected to a hub of another protocol, only to update (its reason).
    pub update_only: Option<String>,
    /// "Update now" can do something.
    pub can_update_now: bool,
    /// Build of the `flow-tray` binary the last update or rollback
    /// installed; a tray of another build restarts itself.
    pub tray_build: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PeerRef {
    pub id: u32,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PeerStatus {
    pub id: u32,
    pub name: String,
    pub online: bool,
    pub enabled: bool,
    pub away: bool,
    /// "p2p", "relay" or "down"; None when there is no link (offline).
    pub path: Option<String>,
    pub rtt_ms: Option<f64>,
}

impl Snapshot {
    /// Display name of a node id (peer name, or `node-<id>`).
    pub fn peer_name(&self, id: u32) -> String {
        self.peers
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("node-{id}"))
    }

    /// Enrolled at a hub (or about to be with a stored token).
    pub fn joined(&self) -> bool {
        self.node_id.is_some() && !self.revoked
    }
}

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// One line of JSON (with the trailing newline) for `body`.
pub fn encode_line<T: Serialize>(body: &T) -> Vec<u8> {
    let mut v = serde_json::to_vec(&Envelope { v: IPC_VERSION, body }).expect("IPC messages serialize");
    v.push(b'\n');
    v
}

/// Why a request line was not accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// Not JSON / not an object.
    Malformed(String),
    /// Another protocol version.
    Version(u32),
    /// Valid envelope, unknown or invalid command.
    Command(String),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestError::Malformed(e) => write!(f, "malformed request: {e}"),
            RequestError::Version(v) => {
                write!(f, "unsupported IPC version {v} (this agent speaks version {IPC_VERSION})")
            }
            RequestError::Command(e) => write!(f, "invalid command: {e}"),
        }
    }
}

/// Parses one request line (server side).
pub fn parse_request(line: &str) -> Result<Command, RequestError> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(|e| RequestError::Malformed(e.to_string()))?;
    let v = value
        .get("v")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| RequestError::Malformed("missing version field \"v\"".into()))?;
    if v != u64::from(IPC_VERSION) {
        return Err(RequestError::Version(v.min(u64::from(u32::MAX)) as u32));
    }
    serde_json::from_value::<Envelope<Command>>(value).map(|e| e.body).map_err(|e| RequestError::Command(e.to_string()))
}

/// Parses one reply line (client side).
pub fn parse_reply(line: &str) -> io::Result<Reply> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(io_err)?;
    match value.get("v").and_then(|v| v.as_u64()) {
        Some(v) if v == u64::from(IPC_VERSION) => {}
        Some(v) => return Err(io_err(format!("agent speaks IPC version {v}, this program version {IPC_VERSION}"))),
        None => return Err(io_err("reply without version field")),
    }
    serde_json::from_value::<Envelope<Reply>>(value).map(|e| e.body).map_err(io_err)
}

/// Reads one line of at most [`MAX_LINE`] bytes. `Ok(None)` at end of
/// stream; an over-long line is an error (the stream is then unusable).
pub fn read_line<R: BufRead>(r: &mut R) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = <&mut R as io::Read>::take(r, MAX_LINE as u64 + 1).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        if buf.len() > MAX_LINE {
            return Err(io_err("IPC line too long"));
        }
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "IPC line cut off"));
    }
    buf.pop();
    String::from_utf8(buf).map(Some).map_err(io_err)
}

/// Writes one message line and flushes.
pub fn write_msg<T: Serialize>(w: &mut impl Write, body: &T) -> io::Result<()> {
    w.write_all(&encode_line(body))?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line<T: Serialize>(t: &T) -> String {
        String::from_utf8(encode_line(t)).unwrap().trim_end().to_string()
    }

    #[test]
    fn commands_roundtrip_and_wire_format() {
        let cmds = [
            Command::Status,
            Command::Subscribe,
            Command::Logs { max: 10 },
            Command::Pause,
            Command::Resume,
            Command::Restart,
            Command::Join { token: "FC1-abc".into(), hub: Some("192.168.1.10".into()) },
            Command::Join { token: "FC1-abc".into(), hub: None },
            Command::Shutdown,
            Command::UpdateNow,
        ];
        for c in cmds {
            assert_eq!(parse_request(&line(&c)).unwrap(), c);
        }
        // The documented wire format (other clients may hand-write it).
        assert_eq!(line(&Command::Pause), r#"{"v":1,"cmd":"pause"}"#);
        assert_eq!(line(&Command::UpdateNow), r#"{"v":1,"cmd":"update_now"}"#);
        assert_eq!(parse_request(r#"{"v":1,"cmd":"logs"}"#).unwrap(), Command::Logs { max: DEFAULT_LOG_LINES });
        assert_eq!(
            parse_request(r#"{"cmd":"join","token":"FC1-x","v":1}"#).unwrap(),
            Command::Join { token: "FC1-x".into(), hub: None }
        );
    }

    #[test]
    fn bad_requests_are_classified() {
        assert!(matches!(parse_request("not json"), Err(RequestError::Malformed(_))));
        assert!(matches!(parse_request(r#"{"cmd":"status"}"#), Err(RequestError::Malformed(_))));
        assert_eq!(parse_request(r#"{"v":2,"cmd":"status"}"#), Err(RequestError::Version(2)));
        assert!(matches!(parse_request(r#"{"v":1,"cmd":"format_disk"}"#), Err(RequestError::Command(_))));
        assert!(matches!(parse_request(r#"{"v":1,"cmd":"join"}"#), Err(RequestError::Command(_))));
        assert!(RequestError::Version(2).to_string().contains("version 1"));
    }

    #[test]
    fn replies_roundtrip_and_tolerate_unknown_fields() {
        let snap = Snapshot {
            agent_version: "0.1.0".into(),
            node_id: Some(3),
            node_name: Some("laptop".into()),
            peers: vec![PeerStatus {
                id: 4,
                name: "pc".into(),
                online: true,
                enabled: true,
                path: Some("p2p".into()),
                rtt_ms: Some(0.8),
                ..Default::default()
            }],
            controlling: Some(PeerRef { id: 4, name: "pc".into() }),
            ..Default::default()
        };
        let replies = [
            Reply::Status { status: Box::new(snap.clone()) },
            Reply::Logs { lines: vec!["a".into(), "b".into()] },
            Reply::Ok { message: "paused".into() },
            Reply::Error { message: "nope".into() },
        ];
        for r in replies {
            assert_eq!(parse_reply(&line(&r)).unwrap(), r);
        }
        // A newer agent adds fields; an older one omits some.
        let r = parse_reply(r#"{"v":1,"type":"status","status":{"node_id":7,"future_field":true},"extra":1}"#).unwrap();
        let Reply::Status { status } = r else { panic!("{r:?}") };
        assert_eq!(status.node_id, Some(7));
        assert!(status.peers.is_empty());
        assert!(parse_reply(r#"{"v":9,"type":"ok","message":""}"#).is_err());
        assert_eq!(snap.peer_name(4), "pc");
        assert_eq!(snap.peer_name(9), "node-9");
    }

    #[test]
    fn session_idle_is_compatible_both_ways() {
        // An agent without the fields: no idle reason (the old meaning of a
        // missing inject capability: a problem).
        let r = parse_reply(r#"{"v":1,"type":"status","status":{"can_inject":false}}"#).unwrap();
        let Reply::Status { status } = r else { panic!("{r:?}") };
        assert_eq!(status.session_idle, None);
        assert!(!status.session_wanted && !status.controllable);
        // The wire names, and a reason this program does not know.
        let s = Snapshot { session_idle: Some(SessionIdle::UpdateOnly), ..Default::default() };
        assert!(line(&Reply::Status { status: Box::new(s) }).contains(r#""session_idle":"update_only""#));
        let r = parse_reply(r#"{"v":1,"type":"status","status":{"session_idle":"on_fire"}}"#).unwrap();
        let Reply::Status { status } = r else { panic!("{r:?}") };
        assert_eq!(status.session_idle, Some(SessionIdle::Other));
    }

    #[test]
    fn line_reader_limits() {
        let mut r = io::Cursor::new(b"one\ntwo\n".to_vec());
        assert_eq!(read_line(&mut r).unwrap().as_deref(), Some("one"));
        assert_eq!(read_line(&mut r).unwrap().as_deref(), Some("two"));
        assert_eq!(read_line(&mut r).unwrap(), None);
        let mut long = vec![b'x'; MAX_LINE + 10];
        long.push(b'\n');
        assert!(read_line(&mut io::Cursor::new(long)).is_err());
        assert!(read_line(&mut io::Cursor::new(b"partial".to_vec())).is_err());
        let mut out = Vec::new();
        write_msg(&mut out, &Command::Status).unwrap();
        assert_eq!(out, b"{\"v\":1,\"cmd\":\"status\"}\n");
    }
}
