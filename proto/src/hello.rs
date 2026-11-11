//! Connection handshake on the control stream, **frozen since protocol 4**.
//!
//! Hubs and agents of different protocol versions must still understand
//! each other far enough for the agent to update itself to the hub's build.
//! Everything in this module and in [`crate::update`] therefore keeps its
//! postcard layout forever:
//!
//! * structs never gain, lose or reorder fields (new information goes into
//!   the version-specific [`Hello::session`] bytes, or into `ext`);
//! * enums only gain variants at the end, and a receiver skips a frame it
//!   cannot decode instead of dropping the connection.
//!
//! The exchange:
//!
//! 1. agent → hub: [`HelloFrame::Hello`]. Its first two postcard fields (enum
//!    tag 0, `protocol_version`) sit where protocols 1–3 put `ToHub::Hello`'s
//!    tag and version, so a hub can tell every agent generation apart with
//!    [`HelloPeek`].
//! 2. hub → agent: [`HelloReply`]. `Accepted` carries the [`SessionMode`]:
//!    * [`SessionMode::Full`] (same protocol): the next frame is the
//!      protocol's `FromHub::Welcome` and the normal session follows;
//!    * [`SessionMode::UpdateOnly`] (any other protocol ≥ 4): the control
//!      stream carries only [`crate::update::UpdateFromHub`] /
//!      [`crate::update::UpdateToHub`] frames and the agent may open
//!      [`crate::StreamKind::Fetch`] streams. No input is routed for it.
//!
//! Agents of protocols 1–3 predate this handshake: the hub answers them with
//! [`LegacyReject`] (their `FromHub::Reject` layout) explaining that they must
//! be updated by hand once.

use crate::NodeId;
use serde::{Deserialize, Serialize};

/// Oldest protocol that speaks this handshake and can update itself.
pub const FIRST_UPDATABLE_PROTOCOL: u32 = 4;

/// Version and build of a hub or agent (`flow-version`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct BuildId {
    /// Workspace version, e.g. `0.2.0`.
    pub version: String,
    /// Build id, e.g. `142.g1a2b3c4d5e` (`dev` for plain cargo builds).
    pub build: String,
}

impl BuildId {
    pub fn new(version: impl Into<String>, build: impl Into<String>) -> Self {
        BuildId { version: version.into(), build: build.into() }
    }

    /// The build of this binary.
    pub fn current() -> Self {
        BuildId::new(flow_version::VERSION, flow_version::BUILD)
    }

    /// `version+build`.
    pub fn full(&self) -> String {
        flow_version::full(&self.version, &self.build)
    }
}

impl std::fmt::Display for BuildId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}+{}", self.version, self.build)
    }
}

/// First frame on the control stream (frozen).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum HelloFrame {
    Hello(Hello),
}

/// Who is connecting (frozen).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub agent: BuildId,
    /// `linux-x86_64`, `windows-x86_64` (`flow_version::PLATFORM`).
    pub platform: String,
    pub hostname: String,
    /// Required only for the first enrolment of a new agent certificate.
    pub join_token: Option<String>,
    /// postcard of the `Register` message of `protocol_version` (monitors,
    /// capabilities, P2P port…); only a hub of the same protocol decodes it.
    pub session: Vec<u8>,
    /// Extension fields (name, value) for later protocols; unknown names
    /// are ignored.
    pub ext: Vec<(String, String)>,
}

/// The hub's answer to [`Hello`] (frozen).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum HelloReply {
    Accepted(Accepted),
    Rejected { reason: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Accepted {
    pub node_id: NodeId,
    pub name: String,
    pub hub: BuildId,
    pub hub_protocol: u32,
    pub mode: SessionMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionMode {
    /// Same protocol: the full session follows.
    Full,
    /// Different protocol: only updates; `reason` is for people.
    UpdateOnly { reason: String },
}

/// The prefix every agent's first control frame shares, from protocol 1
/// on: enum tag (0 = Hello), protocol version and agent version string.
/// Decode it with `postcard::take_from_bytes` (the rest is ignored).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct HelloPeek {
    pub tag: u32,
    pub protocol_version: u32,
    /// Protocols 1–3: `Hello::agent_version`; 4+: `BuildId::version`.
    pub agent_version: String,
}

impl HelloPeek {
    /// Reads the prefix of a first control frame.
    pub fn read(frame: &[u8]) -> Option<HelloPeek> {
        postcard::take_from_bytes::<HelloPeek>(frame).ok().map(|(p, _)| p).filter(|p| p.tag == 0)
    }
}

/// What a hub answers an agent of protocol 1–3: their `FromHub::Reject`
/// (enum tag 1, reason string), which those agents show as "hub rejected
/// this agent: …". Only `Reject` is ever sent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LegacyReject {
    /// Placeholder for the old `FromHub::Welcome` (tag 0), never sent.
    #[doc(hidden)]
    Unused,
    Reject {
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, encode};

    fn hello(v: u32) -> HelloFrame {
        HelloFrame::Hello(Hello {
            protocol_version: v,
            agent: BuildId::new("0.2.0", "7.gabc"),
            platform: "linux-x86_64".into(),
            hostname: "laptop".into(),
            join_token: None,
            session: vec![1, 2, 3],
            ext: vec![],
        })
    }

    /// The protocol-3 `Hello` layout, as agents from before the handshake
    /// send it (only the leading fields matter).
    #[derive(Serialize)]
    enum ToHubV3 {
        Hello { protocol_version: u32, agent_version: String, join_token: Option<String>, hostname: String },
    }

    /// The protocol-3 `FromHub` layout of the first two variants.
    #[derive(Deserialize, Debug, PartialEq)]
    enum FromHubV3 {
        Welcome(u32),
        Reject { reason: String },
    }

    #[test]
    fn every_generation_is_recognised_by_its_prefix() {
        let old = encode(&ToHubV3::Hello {
            protocol_version: 3,
            agent_version: "0.1.0".into(),
            join_token: None,
            hostname: "pc".into(),
        });
        let p = HelloPeek::read(&old).unwrap();
        assert_eq!((p.protocol_version, p.agent_version.as_str()), (3, "0.1.0"));
        let new = encode(&hello(4));
        let p = HelloPeek::read(&new).unwrap();
        assert_eq!((p.protocol_version, p.agent_version.as_str()), (4, "0.2.0"));
        // A later protocol keeps the prefix and the frozen layout.
        let later = encode(&hello(9));
        assert_eq!(HelloPeek::read(&later).unwrap().protocol_version, 9);
        assert_eq!(decode::<HelloFrame>(&later).unwrap(), hello(9));
        assert!(HelloPeek::read(&[1, 4, 0]).is_none(), "not a Hello");
        assert!(HelloPeek::read(&[]).is_none());
    }

    #[test]
    fn legacy_agents_understand_the_reject() {
        let bytes = encode(&LegacyReject::Reject { reason: "update by hand".into() });
        assert_eq!(decode::<FromHubV3>(&bytes).unwrap(), FromHubV3::Reject { reason: "update by hand".into() });
    }

    /// Pins the frozen layouts: a change here breaks agents in the field.
    #[test]
    fn frozen_layouts() {
        let reply = HelloReply::Accepted(Accepted {
            node_id: NodeId(3),
            name: "pc".into(),
            hub: BuildId::new("0.2.0", "b"),
            hub_protocol: 4,
            mode: SessionMode::UpdateOnly { reason: "r".into() },
        });
        assert_eq!(encode(&reply), [0, 3, 2, b'p', b'c', 5, b'0', b'.', b'2', b'.', b'0', 1, b'b', 4, 1, 1, b'r']);
        assert_eq!(encode(&HelloReply::Rejected { reason: "no".into() }), [1, 2, b'n', b'o']);
        assert_eq!(
            encode(&hello(4)),
            [
                0, 4, 5, b'0', b'.', b'2', b'.', b'0', 6, b'7', b'.', b'g', b'a', b'b', b'c', 12, b'l', b'i', b'n',
                b'u', b'x', b'-', b'x', b'8', b'6', b'_', b'6', b'4', 6, b'l', b'a', b'p', b't', b'o', b'p', 0, 3, 1,
                2, 3, 0
            ]
        );
    }
}
