//! This PC's own addresses: the LAN IPv4 friends at home reach, and a public IPv6 if it has one.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, UdpSocket};

use crate::linkpreview::ssrf::interface_addresses;

/// The IPv4 this PC would use to reach the internet. Only a routing decision: `connect` on a UDP
/// socket sends nothing (192.0.2.1 is a documentation address nobody answers on anyway).
pub fn primary_ipv4() -> Option<Ipv4Addr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() && !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

/// A home network address (RFC 1918), not a link-local or CGNAT one.
pub fn is_lan_v4(ip: Ipv4Addr) -> bool {
    ip.is_private()
}

/// The server PC's LAN IPv4 and its prefix length: the one the default route uses when it is a
/// LAN address, else the first LAN address of an interface. The prefix comes from the interface
/// (24 when unknown).
pub fn lan_ipv4() -> Option<(Ipv4Addr, u8)> {
    pick_lan(primary_ipv4(), &interface_addresses())
}

fn pick_lan(primary: Option<Ipv4Addr>, ifs: &[(IpAddr, u8)]) -> Option<(Ipv4Addr, u8)> {
    let lan: Vec<(Ipv4Addr, u8)> = ifs
        .iter()
        .filter_map(|(ip, p)| match ip {
            IpAddr::V4(v4) if is_lan_v4(*v4) => Some((*v4, (*p).clamp(8, 30))),
            _ => None,
        })
        .collect();
    if let Some(p) = primary.filter(|p| is_lan_v4(*p)) {
        return Some(lan.iter().copied().find(|(ip, _)| *ip == p).unwrap_or((p, 24)));
    }
    lan.first().copied()
}

/// A global IPv6 address of this PC, if it has one: friends with IPv6 can reach it without any
/// port forwarding (a router's IPv6 firewall may still block it).
pub fn public_ipv6() -> Option<Ipv6Addr> {
    interface_addresses().into_iter().find_map(|(ip, _)| match ip {
        IpAddr::V6(v6) if is_global_v6(v6) => Some(v6),
        _ => None,
    })
}

/// Global unicast (2000::/3) minus documentation (2001:db8::/32), Teredo (2001::/32) and 6to4
/// (2002::/16).
fn is_global_v6(v6: Ipv6Addr) -> bool {
    let s = v6.segments();
    (s[0] & 0xe000) == 0x2000 && !(s[0] == 0x2001 && (s[1] == 0x0db8 || s[1] == 0)) && s[0] != 0x2002
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lan_address_follows_the_default_route() {
        let ifs: Vec<(IpAddr, u8)> = vec![
            ("127.0.0.1".parse().unwrap(), 8),
            ("172.20.5.2".parse().unwrap(), 20),
            ("192.168.0.20".parse().unwrap(), 24),
            ("fe80::1".parse().unwrap(), 64),
        ];
        assert_eq!(pick_lan(Some(Ipv4Addr::new(192, 168, 0, 20)), &ifs), Some((Ipv4Addr::new(192, 168, 0, 20), 24)));
        assert_eq!(pick_lan(None, &ifs), Some((Ipv4Addr::new(172, 20, 5, 2), 20)));
        // A PC with a public address on its interface: the default route is not a LAN one.
        assert_eq!(pick_lan(Some(Ipv4Addr::new(198, 51, 100, 3)), &ifs), Some((Ipv4Addr::new(172, 20, 5, 2), 20)));
        assert_eq!(pick_lan(Some(Ipv4Addr::new(10, 0, 0, 9)), &[]), Some((Ipv4Addr::new(10, 0, 0, 9), 24)));
        assert_eq!(pick_lan(None, &[("100.64.1.1".parse().unwrap(), 10)]), None);
        for (ip, global) in [("2001:4860::1", true), ("2001:db8:14c::5", false), ("2001:db8::1", false), ("2001::1", false), ("2002::1", false), ("fe80::1", false), ("fd00::1", false)] {
            assert_eq!(is_global_v6(ip.parse().unwrap()), global, "{ip}");
        }
    }
}
