//! Can friends on the internet reach this connection? Decided from the router's WAN address (UPnP)
//! and the address a STUN server sees.

use std::net::IpAddr;

/// What the host's connection allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// This PC has the public address itself (no router NAT in the way).
    Direct,
    /// The router has the public address and opened the ports (or can): friends get in.
    Open,
    /// The provider's CGNAT, or a second router in front: friends cannot reach this connection
    /// directly, whatever is done on this PC or router.
    Cgnat(CgnatSign),
    /// No UPnP: it works only if the ports are forwarded by hand on the router.
    Unknown,
    /// Nothing answered from the internet (offline, or UDP blocked).
    Offline,
}

/// What gave CGNAT away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgnatSign {
    /// The router's WAN address is in 100.64.0.0/10 (the range providers use for CGNAT).
    SharedRange,
    /// The router's WAN address is a private one: another router (the provider's modem) is in front.
    PrivateWan,
    /// The router's WAN address is not the one the internet sees.
    Mismatch,
}

/// 100.64.0.0/10.
pub fn is_shared_range(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(v4) if v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1]))
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback() || v4.is_unspecified(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80 || v6.is_loopback(),
    }
}

/// `router_wan`: the router's external address (UPnP), `seen`: what STUN saw, `seen_is_mine`:
/// the seen address is on one of this PC's own interfaces.
pub fn classify(router_wan: Option<IpAddr>, seen: Option<IpAddr>, seen_is_mine: bool) -> Reach {
    if let Some(wan) = router_wan {
        if is_shared_range(wan) {
            return Reach::Cgnat(CgnatSign::SharedRange);
        }
        if is_private(wan) {
            return Reach::Cgnat(CgnatSign::PrivateWan);
        }
        if seen.is_some_and(|s| s != wan) {
            return Reach::Cgnat(CgnatSign::Mismatch);
        }
        return Reach::Open;
    }
    match seen {
        Some(s) if seen_is_mine && !is_private(s) => Reach::Direct,
        Some(s) if is_shared_range(s) => Reach::Cgnat(CgnatSign::SharedRange),
        Some(_) => Reach::Unknown,
        None => Reach::Offline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn cgnat_is_recognised() {
        // A normal home router: WAN = what the internet sees.
        assert_eq!(classify(ip("203.0.113.7"), ip("203.0.113.7"), false), Reach::Open);
        assert_eq!(classify(ip("203.0.113.7"), None, false), Reach::Open, "STUN blocked, router is enough");
        // Provider CGNAT: the router got an address from 100.64/10.
        assert_eq!(classify(ip("100.71.3.9"), ip("203.0.113.7"), false), Reach::Cgnat(CgnatSign::SharedRange));
        assert_eq!(classify(ip("100.127.255.1"), None, false), Reach::Cgnat(CgnatSign::SharedRange));
        assert_eq!(classify(ip("100.128.0.1"), ip("100.128.0.1"), false), Reach::Open, "100.128/9 is not shared");
        // The provider's modem in front of the router.
        assert_eq!(classify(ip("192.168.1.2"), ip("203.0.113.7"), false), Reach::Cgnat(CgnatSign::PrivateWan));
        assert_eq!(classify(ip("10.0.0.2"), None, false), Reach::Cgnat(CgnatSign::PrivateWan));
        // A public-looking WAN that is still not what the internet sees.
        assert_eq!(classify(ip("198.51.100.4"), ip("203.0.113.7"), false), Reach::Cgnat(CgnatSign::Mismatch));
        // No UPnP.
        assert_eq!(classify(None, ip("203.0.113.7"), false), Reach::Unknown);
        assert_eq!(classify(None, ip("203.0.113.7"), true), Reach::Direct);
        assert_eq!(classify(None, ip("100.64.0.9"), false), Reach::Cgnat(CgnatSign::SharedRange));
        assert_eq!(classify(None, None, false), Reach::Offline);
    }
}
