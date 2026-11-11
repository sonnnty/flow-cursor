//! FlowCursor networking.
//!
//! * [`identity`]: persistent self-signed certificates (hub and agents).
//! * [`tls`]: certificate pinning verifiers. There is no CA: every peer is
//!   identified by the SHA-256 fingerprint of its certificate.
//! * [`endpoint`]: QUIC (quinn) client/server configs with mutual TLS.
//! * [`frame`]: length-prefixed postcard frames on QUIC streams.
//! * [`token`]: join tokens that carry the hub fingerprint.
//! * [`link`]: the per-peer input link with P2P ↔ relay failover.
//! * [`mdns`]: hub advertisement and discovery.

pub mod endpoint;
pub mod frame;
pub mod identity;
pub mod link;
pub mod mdns;
pub mod tls;
pub mod token;

pub use identity::Identity;
pub use quinn;
