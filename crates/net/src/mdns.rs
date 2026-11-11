//! mDNS advertisement of the hub (`_flowcursor._udp.local.`). The TXT
//! record carries the hub fingerprint as a hint only; agents always verify
//! the certificate against their pinned fingerprint.

use anyhow::Result;
use flow_proto::{Fingerprint, MDNS_SERVICE};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

pub struct Advertisement {
    daemon: ServiceDaemon,
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

/// Starts advertising the hub on all interfaces.
pub fn advertise(instance: &str, port: u16, fp: &Fingerprint) -> Result<Advertisement> {
    let daemon = ServiceDaemon::new()?;
    let host = format!("{}.local.", instance.replace(['.', ' '], "-"));
    let props = [("fp", fp.to_hex()), ("v", flow_proto::PROTOCOL_VERSION.to_string())];
    let info = ServiceInfo::new(MDNS_SERVICE, instance, &host, "", port, &props[..])?.enable_addr_auto();
    daemon.register(info)?;
    Ok(Advertisement { daemon })
}

#[derive(Clone, Debug)]
pub struct HubAdvert {
    pub addrs: Vec<SocketAddr>,
    pub fingerprint: Option<Fingerprint>,
}

/// Browses for hubs (blocking; run in `spawn_blocking`).
pub fn discover(timeout: Duration) -> Result<Vec<HubAdvert>> {
    let daemon = ServiceDaemon::new()?;
    let rx = daemon.browse(MDNS_SERVICE)?;
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let port = info.get_port();
                let mut addrs: Vec<SocketAddr> =
                    info.get_addresses_v4().into_iter().map(|ip| SocketAddr::new(ip.into(), port)).collect();
                addrs.sort();
                let fingerprint = info.get_property_val_str("fp").and_then(Fingerprint::from_hex);
                out.push(HubAdvert { addrs, fingerprint });
                // One hub is the normal case in a home network.
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    Ok(out)
}
