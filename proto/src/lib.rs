//! FlowCursor wire protocol.
//!
//! Every message that crosses the network is defined here and encoded with
//! [`postcard`] (compact varint serde format). All peers are built from the
//! same workspace, so instead of protobuf's field-level evolution we use an
//! explicit [`PROTOCOL_VERSION`] that is checked in the very first message of
//! every connection; a mismatch produces a clear "upgrade" error.
//!
//! Streams and datagrams (all inside QUIC/TLS 1.3, never plaintext):
//!
//! * agent → hub connection (ALPN [`ALPN_HUB`]), all streams opened by the agent:
//!   * control stream: [`ToHub`] / [`FromHub`] frames, first frame is [`ToHub::Hello`]
//!   * relay stream: [`RelayUp`] / [`RelayDown`] frames (input relayed through the hub)
//!   * clipboard stream: [`ClipboardMsg`] frames (bulk data, kept apart so it
//!     never delays key events)
//!   * QUIC datagrams: [`RelayDatagramUp`] / [`RelayDatagramDown`] (mouse motion, pings)
//! * agent → agent connection (ALPN [`ALPN_P2P`]), opened by the sending agent:
//!   * one bidirectional stream of [`LinkMsg`] frames
//!   * QUIC datagrams of [`LinkDatagram`]

use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::{IpAddr, SocketAddr};

pub mod clip;
pub mod net;
pub mod settings;

pub use clip::{ClipContent, ClipKind, ClipboardMsg};
pub use settings::{AgentSettings, ClipboardSettings, EdgeSettings, HubSettings, PathPolicy};

/// Bumped whenever any message in this crate changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 2;
/// ALPN for agent ↔ hub connections.
pub const ALPN_HUB: &[u8] = b"flowcursor-hub/1";
/// ALPN for agent ↔ agent (P2P) connections.
pub const ALPN_P2P: &[u8] = b"flowcursor-p2p/1";
/// mDNS service type advertised by the hub.
pub const MDNS_SERVICE: &str = "_flowcursor._udp.local.";
/// Default UDP port of the hub (QUIC).
pub const DEFAULT_HUB_PORT: u16 = 7878;
/// Default UDP port of an agent's P2P listener (QUIC).
pub const DEFAULT_P2P_PORT: u16 = 7879;
/// QUIC application close code the hub uses when it revokes an agent.
pub const CLOSE_REVOKED: u32 = 3;
/// Max size of a control/link frame.
pub const MAX_CONTROL_FRAME: usize = 1024 * 1024;
/// Max size of a clipboard frame (hard cap; the configured limit is lower).
pub const MAX_CLIPBOARD_FRAME: usize = 64 * 1024 * 1024;

/// Identifier assigned by the hub when a node is enrolled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub u32);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "node-{}", self.0)
    }
}

/// SHA-256 of a certificate (DER). Identity of hub and agents.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// Accepts 64 hex digits, optionally separated by `:`.
    pub fn from_hex(s: &str) -> Option<Self> {
        let clean: String = s.chars().filter(|c| *c != ':' && !c.is_whitespace()).collect();
        if clean.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Fingerprint(out))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({}…)", &self.to_hex()[..16])
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Axis-aligned rectangle in integer pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64 && x < self.right() as f64 && y >= self.y as f64 && y < self.bottom() as f64
    }
    pub fn translate(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }
    pub fn union(&self, o: &Rect) -> Rect {
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        let r = self.right().max(o.right());
        let b = self.bottom().max(o.bottom());
        Rect::new(x, y, r - x, b - y)
    }
}

/// One monitor of a node, in the node's own desktop coordinate space
/// (the space its OS uses for absolute pointer positions: physical pixels on
/// Windows, logical pixels on GNOME/Wayland).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Monitor {
    pub name: String,
    pub rect: Rect,
    #[serde(default)]
    pub primary: bool,
}

/// Position of one node in the shared layout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeLayout {
    pub id: NodeId,
    /// Offset of the node's desktop origin in the global layout.
    pub x: i32,
    pub y: i32,
    pub monitors: Vec<Monitor>,
}

/// The whole screen arrangement (only enabled nodes are included).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Layout {
    pub nodes: Vec<NodeLayout>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OsKind {
    Linux,
    Windows,
    Other,
}

impl OsKind {
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            OsKind::Linux
        } else if cfg!(windows) {
            OsKind::Windows
        } else {
            OsKind::Other
        }
    }
}

/// What a node can do (depends on the backend that is running).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Capabilities {
    pub capture: bool,
    pub inject: bool,
    pub clipboard: bool,
}

/// Everything an agent needs to know about another node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: NodeId,
    pub name: String,
    /// Certificate fingerprint the peer must present on P2P connections.
    pub fingerprint: Fingerprint,
    /// Addresses where the peer's P2P listener can be reached.
    pub addrs: Vec<SocketAddr>,
    pub online: bool,
    pub enabled: bool,
    /// Pointer speed multiplier applied to motion sent *to* this node.
    pub mouse_speed: f32,
    pub caps: Capabilities,
    /// The peer is connected but not on one of its allowed networks: it
    /// neither controls nor is controlled, and edges to it do not cross.
    pub away: bool,
    /// The peer's effective crossing key (empty = none): it must be held to
    /// move the pointer out of that peer's screen.
    pub crossing_key: String,
}

/// Network path currently used for a source → target link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathKind {
    P2p,
    Relay,
    Down,
}

impl fmt::Display for PathKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PathKind::P2p => "p2p",
            PathKind::Relay => "relay",
            PathKind::Down => "down",
        })
    }
}

/// Which node is being driven by which source's physical devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusEntry {
    pub source: NodeId,
    pub target: NodeId,
}

// ---------------------------------------------------------------------------
// agent ↔ hub control stream
// ---------------------------------------------------------------------------

/// First message on the control stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub agent_version: String,
    /// Required only for the first enrollment of a new agent certificate.
    pub join_token: Option<String>,
    pub hostname: String,
    pub os: OsKind,
    pub monitors: Vec<Monitor>,
    pub caps: Capabilities,
    /// UDP port of the agent's P2P listener.
    pub p2p_port: u16,
    /// Explicit P2P addresses to advertise. If empty the hub uses the
    /// address it sees the agent connecting from plus `p2p_port`.
    pub p2p_addrs: Vec<SocketAddr>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    pub node_id: NodeId,
    pub name: String,
    pub hub_version: String,
    pub settings: AgentSettings,
    pub layout: Layout,
    pub peers: Vec<PeerInfo>,
    pub focus: Vec<FocusEntry>,
}

/// Per-pair path report sent by agents for the dashboard.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PathReport {
    pub peer: NodeId,
    pub path: PathKind,
    /// Smoothed round-trip time in microseconds, if known.
    pub rtt_us: Option<u32>,
    pub p2p_rtt_us: Option<u32>,
    pub relay_rtt_us: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToHub {
    Hello(Hello),
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    UpdateMonitors(Vec<Monitor>),
    /// This node's physical devices now drive `target` (None = local again).
    FocusChanged {
        target: Option<NodeId>,
    },
    PathReports(Vec<PathReport>),
    /// Result of the agent's allowed-network check and the interface
    /// addresses it was made from (sent on connect and on every change).
    NetStatus {
        away: bool,
        addrs: Vec<IpAddr>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FromHub {
    Welcome(Box<Welcome>),
    Reject {
        reason: String,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    Layout(Layout),
    Peers(Vec<PeerInfo>),
    Settings(AgentSettings),
    Focus(Vec<FocusEntry>),
    /// The hub asks this source to give up remote control (e.g. the target
    /// was disabled).
    ReleaseControl,
    /// This node's credentials were revoked; the agent must re-join.
    Revoked,
}

/// First frame on every additional agent-opened stream to the hub.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamKind {
    Relay,
    Clipboard,
}

// ---------------------------------------------------------------------------
// input link (agent → agent, directly or relayed through the hub)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl Button {
    pub const ALL: [Button; 5] = [Button::Left, Button::Right, Button::Middle, Button::Back, Button::Forward];
}

/// Reliable, ordered input events. `usage` is a canonical HID usage
/// (`page << 16 | id`, see `flow-keymap`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Pointer enters the target at its local desktop coordinates.
    Enter {
        x: i32,
        y: i32,
    },
    /// Pointer leaves the target; the receiver releases everything held.
    Leave,
    Key {
        usage: u32,
        down: bool,
    },
    Button {
        button: Button,
        down: bool,
    },
    /// Wheel in 1/120 notch units (positive = up / right).
    Wheel {
        dx: i32,
        dy: i32,
    },
    /// Authoritative set of keys/buttons the sender believes are held.
    /// Sent after every path switch; the receiver releases anything else.
    StateSync {
        keys: Vec<u32>,
        buttons: Vec<Button>,
    },
}

/// Pointer motion (unreliable datagram). Carries the absolute target
/// position so a lost datagram is corrected by the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Motion {
    pub session: u64,
    /// Monotonic per session; older datagrams are dropped.
    pub mseq: u64,
    /// Highest reliable `seq` sent before this motion; the receiver ignores
    /// the motion until it has applied that event (so motion never overtakes
    /// an Enter or a button press).
    pub after_seq: u64,
    pub x: i32,
    pub y: i32,
    pub dx: i32,
    pub dy: i32,
}

/// Reliable frames on a link stream (and inside relay frames).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkMsg {
    /// Sender → receiver. `base` = oldest sequence number the sender still
    /// keeps (everything below has been acknowledged), used by a receiver
    /// that does not know the session yet.
    Event { session: u64, seq: u64, base: u64, ev: InputEvent },
    /// Receiver → sender: everything up to and including `upto` applied.
    Ack { session: u64, upto: u64 },
}

/// Unreliable frames (QUIC datagrams), directly or relayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkDatagram {
    Motion(Motion),
    /// Liveness/RTT probe from sender to receiver; `session` lets a
    /// receiver keep a controlled session alive while the user is idle.
    Ping {
        session: u64,
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
}

/// Agent → hub on the relay stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayUp {
    pub to: NodeId,
    pub msg: LinkMsg,
}

/// Hub → agent on the relay stream; `from` is the authenticated sender.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayDown {
    pub from: NodeId,
    pub msg: LinkMsg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayDatagramUp {
    pub to: NodeId,
    pub dg: LinkDatagram,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayDatagramDown {
    pub from: NodeId,
    pub dg: LinkDatagram,
}

// ---------------------------------------------------------------------------
// encoding
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("decode error: {0}")]
    Decode(#[from] postcard::Error),
    #[error("{0} trailing bytes after message")]
    Trailing(usize),
}

pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    postcard::to_stdvec(msg).expect("postcard encoding into a Vec cannot fail")
}

/// Decodes exactly one message; trailing bytes are an error.
pub fn decode<'a, T: Deserialize<'a>>(buf: &'a [u8]) -> Result<T, CodecError> {
    let (msg, rest) = postcard::take_from_bytes(buf)?;
    if !rest.is_empty() {
        return Err(CodecError::Trailing(rest.len()));
    }
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_hex_roundtrip() {
        let fp = Fingerprint([0xab; 32]);
        let hex = fp.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(Fingerprint::from_hex(&hex), Some(fp));
        let colon: Vec<String> = fp.0.iter().map(|b| format!("{b:02X}")).collect();
        assert_eq!(Fingerprint::from_hex(&colon.join(":")), Some(fp));
        assert_eq!(Fingerprint::from_hex("abc"), None);
        assert_eq!(Fingerprint::from_hex(&"zz".repeat(32)), None);
    }

    #[test]
    fn link_messages_are_compact() {
        let ev = LinkMsg::Event {
            session: 0x1234_5678_9abc,
            seq: 42,
            base: 40,
            ev: InputEvent::Key { usage: 0x0007_0004, down: true },
        };
        let buf = encode(&ev);
        assert!(buf.len() < 24, "key event is {} bytes", buf.len());
        assert_eq!(decode::<LinkMsg>(&buf).unwrap(), ev);

        let m = LinkDatagram::Motion(Motion { session: 1, mseq: 9, after_seq: 3, x: 1920, y: 540, dx: -3, dy: 2 });
        let buf = encode(&m);
        assert!(buf.len() < 20, "motion is {} bytes", buf.len());
        assert_eq!(decode::<LinkDatagram>(&buf).unwrap(), m);
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        for bad in [&[][..], &[0xff, 0xff, 0xff][..], &[0x7f][..], &[9, 9, 9, 9, 9, 9][..]] {
            assert!(decode::<LinkMsg>(bad).is_err());
            assert!(decode::<FromHub>(bad).is_err());
        }
        // Valid message followed by garbage.
        let mut buf = encode(&ToHub::Ping { nonce: 1 });
        buf.push(0);
        assert!(decode::<ToHub>(&buf).is_err());
        // Truncated valid message.
        let buf = encode(&ToHub::Ping { nonce: u64::MAX });
        assert!(decode::<ToHub>(&buf[..buf.len() - 1]).is_err());
    }

    #[test]
    fn rect_geometry() {
        let r = Rect::new(0, 0, 100, 50);
        assert!(r.contains(0.0, 0.0));
        assert!(r.contains(99.9, 49.9));
        assert!(!r.contains(100.0, 10.0));
        assert_eq!(r.union(&Rect::new(-10, 20, 20, 50)), Rect::new(-10, 0, 110, 70));
    }
}
