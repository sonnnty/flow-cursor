//! Input links between agents.
//!
//! A *link* carries input from one source node to one target node. The
//! sender side ([`sender`]) owns the path decision:
//!
//! * **P2P**: a QUIC connection dialed by the source directly to the
//!   target's P2P listener (mutually pinned certificates),
//! * **relay**: the same messages wrapped in relay frames on the source's
//!   hub connection; the hub forwards them on the target's hub connection.
//!
//! Reliable events carry `(session, seq)`. The sender keeps every event until
//! the receiver acknowledges it and resends all unacknowledged events when
//! it switches paths; the receiver ([`receiver`]) applies each sequence
//! number exactly once and in order, so a switch neither loses nor
//! duplicates key or button events. After a switch the sender also sends a
//! `StateSync` with the keys/buttons it believes are held, which releases
//! anything the receiver holds that the sender does not (and is a no-op
//! when nothing went wrong). Motion uses unreliable datagrams with absolute
//! positions, so losing one is harmless.

pub mod p2p;
pub mod receiver;
pub mod sender;

use flow_proto::{Fingerprint, LinkDatagram, LinkMsg, NodeId, PathPolicy};
use std::net::SocketAddr;
use std::time::Duration;

pub use p2p::{serve_p2p, PeerDirectory};
pub use receiver::{Action, ReceiverState, HEARTBEAT_TIMEOUT};
pub use sender::{spawn_link, LinkHandle, LinkStatus};

/// Sends link messages through the hub.
pub trait RelayTransport: Send + Sync + 'static {
    /// Queues a reliable message for `to`. False if the hub is unreachable.
    fn send(&self, to: NodeId, msg: LinkMsg) -> bool;
    /// Sends an unreliable datagram for `to`.
    fn send_datagram(&self, to: NodeId, dg: LinkDatagram) -> bool;
    fn is_up(&self) -> bool;
}

/// Where and how to reach a peer directly.
#[derive(Clone, Debug, PartialEq)]
pub struct PeerTarget {
    pub id: NodeId,
    pub fingerprint: Fingerprint,
    pub addrs: Vec<SocketAddr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LinkConfig {
    pub policy: PathPolicy,
    /// Ping interval on the active path while this link is in use.
    pub ping_active: Duration,
    /// Ping interval while idle.
    pub ping_idle: Duration,
    /// An unacknowledged event or a missing pong older than this marks the
    /// direct path as failed.
    pub fail_timeout: Duration,
    /// While relayed, how often the direct path is probed.
    pub probe_interval: Duration,
    /// Consecutive successful probes needed to return to P2P.
    pub recover_after: u32,
    pub dial_timeout: Duration,
    /// Unacknowledged events kept before the session is reset.
    pub max_unacked: usize,
}

impl Default for LinkConfig {
    fn default() -> Self {
        LinkConfig {
            policy: PathPolicy::PreferP2p,
            ping_active: Duration::from_millis(200),
            ping_idle: Duration::from_millis(1000),
            fail_timeout: Duration::from_millis(700),
            probe_interval: Duration::from_millis(2000),
            recover_after: 2,
            dial_timeout: Duration::from_millis(1500),
            max_unacked: 4096,
        }
    }
}

impl LinkConfig {
    pub fn from_settings(s: &flow_proto::HubSettings) -> Self {
        LinkConfig {
            policy: s.path_policy,
            probe_interval: Duration::from_millis(s.probe_interval_ms.clamp(100, 60_000) as u64),
            ..Default::default()
        }
    }
}
