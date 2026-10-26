//! This PC's global IPv6 address (v1.0.2), for invites made on this server and for the startup
//! diagnostics.
//!
//! A PC usually has two global IPv6 addresses per network: a stable ("public") one and a
//! temporary one that Windows replaces every day or so (privacy extensions). An invite can live
//! for a year, so it should carry the stable one: [`stable_ipv6`] prefers addresses that are not
//! deprecated and not temporary.

use std::net::Ipv6Addr;

use vox_proto::InviteHints;

/// One IPv6 address of an interface that is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V6Addr {
    pub ip: Ipv6Addr,
    /// A temporary (privacy) address: random suffix, replaced every day or so.
    pub temporary: bool,
    /// Deprecated: still answers, but is on its way out.
    pub deprecated: bool,
}

/// Global unicast (2000::/3) minus documentation (2001:db8::/32), Teredo (2001::/32) and 6to4
/// (2002::/16): an address friends on the internet can reach (if no firewall is in the way).
pub fn is_global_v6(v6: Ipv6Addr) -> bool {
    let s = v6.segments();
    (s[0] & 0xe000) == 0x2000 && !(s[0] == 0x2001 && (s[1] == 0x0db8 || s[1] == 0)) && s[0] != 0x2002
}

/// The best global address for friends to use: current before deprecated, stable before
/// temporary, then the system's order.
pub fn pick(addrs: &[V6Addr]) -> Option<Ipv6Addr> {
    addrs.iter().filter(|a| is_global_v6(a.ip)).min_by_key(|a| (a.deprecated, a.temporary)).map(|a| a.ip)
}

/// This PC's stable global IPv6, if it has one.
pub fn stable_ipv6() -> Option<Ipv6Addr> {
    pick(&addresses())
}

/// The invite hints of a hub that listens on IPv6: when it knows no public address of its own
/// (`public_host` unset or loopback), its global IPv6. Old apps (v1.0.1) put `public_host` in the
/// `vox1-` code as one more address to try, so the code carries the IPv6 with no app update.
/// A `public_host` the owner set (a DDNS name, the public IPv4) is kept: it is the address they
/// chose.
pub fn with_ipv6(mut hints: InviteHints, listens_on_ipv6: bool, v6: Option<Ipv6Addr>) -> InviteHints {
    if listens_on_ipv6 && hints.public_host.is_empty() {
        if let Some(ip) = v6.filter(|ip| is_global_v6(*ip)) {
            hints.public_host = ip.to_string();
        }
    }
    hints
}

/// The IPv6 addresses of the interfaces that are up.
#[cfg(windows)]
pub fn addresses() -> Vec<V6Addr> {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_FRIENDLY_NAME, GAA_FLAG_SKIP_MULTICAST,
        IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{IpDadStateDeprecated, IpSuffixOriginRandom, AF_INET6, SOCKADDR_IN6};
    const ERROR_BUFFER_OVERFLOW: u32 = 111;
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER | GAA_FLAG_SKIP_FRIENDLY_NAME;
    let mut out = Vec::new();
    let mut size: u32 = 32 * 1024;
    for _ in 0..4 {
        // u64 storage: the structures need 8-byte alignment.
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        let first = buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: `buf` is writable for `size` bytes and suitably aligned; the API fills it with
        // a linked list of adapters that only points inside it.
        let r = unsafe { GetAdaptersAddresses(u32::from(AF_INET6.0), flags, None, Some(first), &mut size) };
        if r == ERROR_BUFFER_OVERFLOW {
            continue;
        }
        if r != 0 {
            return out;
        }
        let mut adapter = first as *const IP_ADAPTER_ADDRESSES_LH;
        // SAFETY: walking the lists the API just wrote into `buf` (alive for the whole loop).
        unsafe {
            while !adapter.is_null() {
                if (*adapter).OperStatus == IfOperStatusUp {
                    let mut ua = (*adapter).FirstUnicastAddress;
                    while !ua.is_null() {
                        let sa = (*ua).Address.lpSockaddr;
                        if !sa.is_null() && (*sa).sa_family == AF_INET6 {
                            let v6 = &*(sa as *const SOCKADDR_IN6);
                            out.push(V6Addr {
                                ip: Ipv6Addr::from(v6.sin6_addr.u.Byte),
                                temporary: (*ua).SuffixOrigin == IpSuffixOriginRandom,
                                deprecated: (*ua).DadState == IpDadStateDeprecated,
                            });
                        }
                        ua = (*ua).Next;
                    }
                }
                adapter = (*adapter).Next;
            }
        }
        return out;
    }
    out
}

/// The IPv6 addresses of the interfaces (elsewhere: what the interface list knows, all taken as
/// stable).
#[cfg(not(windows))]
pub fn addresses() -> Vec<V6Addr> {
    crate::linkpreview::ssrf::interface_addresses()
        .into_iter()
        .filter_map(|(ip, _)| match ip {
            std::net::IpAddr::V6(ip) => Some(V6Addr { ip, temporary: false, deprecated: false }),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(ip: &str, temporary: bool, deprecated: bool) -> V6Addr {
        V6Addr { ip: ip.parse().unwrap(), temporary, deprecated }
    }

    #[test]
    fn the_stable_global_address_wins() {
        // 2001:db8::/32 is documentation (not global): 3fff::/20 is the newer documentation range
        // and still counts as global unicast here, so the tests use it for "a real address".
        let list = [
            a("fe80::1", false, false),
            a("fd00::5", false, false),
            a("3fff:0:0:1::aaaa", true, true),
            a("3fff:0:0:1::bbbb", true, false),
            a("3fff:0:0:1::cccc", false, false),
        ];
        assert_eq!(pick(&list), Some("3fff:0:0:1::cccc".parse().unwrap()));
        // Only temporary ones: the current one.
        assert_eq!(pick(&list[..4]), Some("3fff:0:0:1::bbbb".parse().unwrap()));
        assert_eq!(pick(&list[..3]), Some("3fff:0:0:1::aaaa".parse().unwrap()));
        assert_eq!(pick(&list[..2]), None, "link-local and ULA are not reachable from the internet");
        assert_eq!(pick(&[a("2001:db8::1", false, false), a("2002::1", false, false), a("2001::1", false, false)]), None);
        for (ip, global) in [("2001:4860::1", true), ("2001:db8::1", false), ("fe80::1", false), ("fd00::1", false), ("::1", false)] {
            assert_eq!(is_global_v6(ip.parse().unwrap()), global, "{ip}");
        }
    }

    #[test]
    fn invites_get_the_ipv6_only_when_nothing_else_is_known() {
        let v6: Option<Ipv6Addr> = Some("3fff::10".parse().unwrap());
        let empty = InviteHints { hub_port: 7890, ..Default::default() };
        assert_eq!(with_ipv6(empty.clone(), true, v6).public_host, "3fff::10");
        // The owner's own public_host stays.
        let set = InviteHints { public_host: "meuservidor.example.org".into(), ..empty.clone() };
        assert_eq!(with_ipv6(set.clone(), true, v6), set);
        // A hub that does not listen on IPv6 (ipv6 = false, or bound to one IPv4 address).
        assert_eq!(with_ipv6(empty.clone(), false, v6), empty);
        assert_eq!(with_ipv6(empty.clone(), true, None), empty);
        assert_eq!(with_ipv6(empty.clone(), true, Some("fd00::1".parse().unwrap())), empty);
    }

    /// Whatever this machine has, the pick is a global address or nothing.
    #[test]
    fn this_machine_gives_a_global_address_or_none() {
        if let Some(ip) = stable_ipv6() {
            assert!(is_global_v6(ip), "{ip}");
        }
    }
}
