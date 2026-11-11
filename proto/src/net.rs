//! Network allowlist: CIDR blocks and the "is this machine on an allowed
//! network" decision, shared by the hub (validation) and the agents.
//!
//! Matching is purely by address: a machine is on an allowed network when
//! any of its own non-loopback interface addresses lies inside one of the
//! allowed blocks. A VPN tunnel interface therefore never counts just for
//! existing — its address (e.g. 10.8.0.5) is not inside the home block
//! (e.g. 10.10.15.0/24) — and the address the hub sees (the VPN's) plays no
//! role at all.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

/// An IPv4 or IPv6 network block (`10.10.15.0/24`, `fd00::/8`). A bare
/// address is a single host (`/32` or `/128`). Host bits may be set
/// (`10.10.15.7/24` means `10.10.15.0/24`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cidr {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Result<Cidr, String> {
        let s = s.trim();
        let (ip, prefix) = match s.split_once('/') {
            Some((ip, p)) => (ip.trim(), Some(p.trim())),
            None => (s, None),
        };
        let addr: IpAddr =
            ip.parse().map_err(|_| format!("{s:?} is not an IP network (expected e.g. 10.10.15.0/24)"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => max,
            Some(p) => match p.parse::<u8>() {
                Ok(n) if n <= max => n,
                _ => return Err(format!("{s:?}: prefix length must be 0–{max}")),
            },
        };
        Ok(Cidr { addr, prefix })
    }

    /// Whether `ip` lies inside this block. IPv4-mapped IPv6 addresses
    /// (`::ffff:a.b.c.d`) are compared as IPv4.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr.to_canonical(), ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                prefix_eq(&u32::from(net).to_be_bytes(), &u32::from(ip).to_be_bytes(), self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => prefix_eq(&net.octets(), &ip.octets(), self.prefix),
            _ => false,
        }
    }
}

fn prefix_eq(a: &[u8], b: &[u8], prefix: u8) -> bool {
    let full = (prefix / 8) as usize;
    let rest = prefix % 8;
    if a[..full] != b[..full] {
        return false;
    }
    if rest == 0 {
        return true;
    }
    let mask = 0xFFu8 << (8 - rest);
    a[full] & mask == b[full] & mask
}

impl FromStr for Cidr {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Cidr::parse(s)
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// Parses a list of CIDR strings (blank entries are ignored).
pub fn parse_networks(list: &[String]) -> Result<Vec<Cidr>, String> {
    list.iter().filter(|s| !s.trim().is_empty()).map(|s| Cidr::parse(s)).collect()
}

/// True when the allowlist is empty (no restriction) or any of `addrs`
/// (the machine's own interface addresses) lies inside an allowed block.
/// Loopback and unspecified addresses never count.
pub fn on_allowed_network(allowed: &[Cidr], addrs: &[IpAddr]) -> bool {
    allowed.is_empty()
        || addrs
            .iter()
            .filter(|a| !a.is_loopback() && !a.is_unspecified())
            .any(|a| allowed.iter().any(|c| c.contains(*a)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn parse_and_contains_v4() {
        let c = Cidr::parse(" 10.10.15.0/24 ").unwrap();
        assert_eq!(c.to_string(), "10.10.15.0/24");
        assert!(c.contains(ip("10.10.15.0")));
        assert!(c.contains(ip("10.10.15.255")));
        assert!(!c.contains(ip("10.10.16.1")));
        assert!(!c.contains(ip("10.10.5.24")));
        // Host bits set are fine.
        assert!(Cidr::parse("10.10.15.7/24").unwrap().contains(ip("10.10.15.200")));
        // Odd prefixes.
        let c = Cidr::parse("192.168.0.0/23").unwrap();
        assert!(c.contains(ip("192.168.1.9")));
        assert!(!c.contains(ip("192.168.2.1")));
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains(ip("8.8.8.8")));
        // Bare address = one host.
        let h = Cidr::parse("10.0.0.1").unwrap();
        assert_eq!(h.prefix, 32);
        assert!(h.contains(ip("10.0.0.1")) && !h.contains(ip("10.0.0.2")));
    }

    #[test]
    fn parse_and_contains_v6() {
        let c = Cidr::parse("fd00:1234::/32").unwrap();
        assert!(c.contains(ip("fd00:1234::1")));
        assert!(!c.contains(ip("fd00:1235::1")));
        assert!(!c.contains(ip("10.10.15.1")), "families never mix");
        assert!(!Cidr::parse("10.0.0.0/8").unwrap().contains(ip("fd00::1")));
        // IPv4-mapped IPv6 is compared as IPv4.
        assert!(Cidr::parse("10.10.15.0/24").unwrap().contains(ip("::ffff:10.10.15.9")));
        assert_eq!(Cidr::parse("::1").unwrap().prefix, 128);
    }

    #[test]
    fn invalid_cidrs() {
        for bad in ["", "10.10.15.0/33", "fd00::/129", "10.10.15/24", "hello", "10.0.0.0/-1", "10.0.0.0/x", "/24"] {
            assert!(Cidr::parse(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(parse_networks(&["10.0.0.0/8".into(), " ".into(), "nope".into()]).is_err());
        assert_eq!(parse_networks(&["10.0.0.0/8".into(), " ".into()]).unwrap().len(), 1);
    }

    #[test]
    fn allowed_network_decision() {
        let home = parse_networks(&["10.10.15.0/24".into()]).unwrap();
        // Empty allowlist: no restriction.
        assert!(on_allowed_network(&[], &[]));
        assert!(on_allowed_network(&[], &[ip("1.2.3.4")]));
        // At home.
        assert!(on_allowed_network(&home, &[ip("10.10.15.42"), ip("fe80::1")]));
        // Away with a VPN: the tunnel address is not in the home block.
        assert!(!on_allowed_network(&home, &[ip("192.168.1.50"), ip("10.8.0.5")]));
        // Loopback never counts, even when it would match.
        let lo = parse_networks(&["127.0.0.0/8".into()]).unwrap();
        assert!(!on_allowed_network(&lo, &[ip("127.0.0.1")]));
        assert!(!on_allowed_network(&home, &[]));
    }
}
