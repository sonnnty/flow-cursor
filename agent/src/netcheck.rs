//! Where the agent learns its own interface addresses for the
//! allowed-networks check ([`flow_proto::net::on_allowed_network`]).
//!
//! The source is injectable so tests (and the simulated backend) can move
//! a machine between networks without touching the host's interfaces.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the agent re-reads its interface addresses. Reading them is a
/// single netlink / `GetAdaptersAddresses` call, cheap enough to poll.
pub const NET_CHECK_INTERVAL: Duration = Duration::from_secs(3);

/// Source of this machine's interface addresses.
pub trait AddrSource: Send + Sync + 'static {
    /// Current non-loopback addresses of interfaces that are not down.
    fn addrs(&self) -> Vec<IpAddr>;
}

/// The real interfaces of this machine.
#[derive(Debug, Default)]
pub struct SystemAddrs;

impl AddrSource for SystemAddrs {
    fn addrs(&self) -> Vec<IpAddr> {
        use if_addrs::IfOperStatus as S;
        match if_addrs::get_if_addrs() {
            Ok(list) => {
                let mut v: Vec<IpAddr> = list
                    .into_iter()
                    .filter(|i| !i.is_loopback())
                    // Disconnected adapters may still list a stale address
                    // (Windows); interfaces in an unknown state count.
                    .filter(|i| !matches!(i.oper_status, S::Down | S::NotPresent | S::LowerLayerDown))
                    .map(|i| i.ip())
                    .collect();
                v.sort();
                v.dedup();
                v
            }
            Err(e) => {
                tracing::warn!("reading interface addresses failed: {e}");
                Vec::new()
            }
        }
    }
}

/// Addresses set by hand (tests, `--backend sim`).
#[derive(Clone, Debug, Default)]
pub struct StaticAddrs(Arc<Mutex<Vec<IpAddr>>>);

impl StaticAddrs {
    pub fn new(addrs: impl IntoIterator<Item = IpAddr>) -> Self {
        let s = StaticAddrs::default();
        s.set(addrs);
        s
    }

    /// Simulates a network change.
    pub fn set(&self, addrs: impl IntoIterator<Item = IpAddr>) {
        *self.0.lock().expect("poisoned") = addrs.into_iter().collect();
    }
}

impl AddrSource for StaticAddrs {
    fn addrs(&self) -> Vec<IpAddr> {
        self.0.lock().expect("poisoned").clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_addrs_exclude_loopback() {
        assert!(SystemAddrs.addrs().iter().all(|a| !a.is_loopback()));
    }

    #[test]
    fn static_addrs_follow_set() {
        let s = StaticAddrs::new(["10.10.15.2".parse().unwrap()]);
        assert_eq!(s.addrs().len(), 1);
        s.set([]);
        assert!(s.addrs().is_empty());
    }
}
