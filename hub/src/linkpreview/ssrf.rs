//! SSRF protection for the link-preview fetcher.
//!
//! The hub runs on somebody's home PC, so a chat message must never make it talk to the host's
//! own machine or LAN. Two layers, both applied to every hop (the first URL and each redirect):
//!
//! 1. [`Policy::check_url`]: scheme, no credentials in the URL, a web port, a sane host name, and
//!    IP literals (including the odd decimal/hex/short forms, which the URL parser normalises) must
//!    be public.
//! 2. [`SafeResolver`]: the *only* DNS resolver the HTTP client has. Names that resolve to private
//!    addresses are dropped, and the client connects to the addresses that survived, so there is
//!    no gap between "checked" and "connected" for DNS rebinding to use.
//!
//! "Public" is not enough on a home network with native IPv6: the host and every LAN device have
//! global addresses. So the host's own interface addresses and their on-link networks (e.g. the
//! LAN's /64) are refused too ([`local_networks`], [`Policy::set_local_networks`]).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, RwLock};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::Url;

/// Why a URL was refused. Never shown to users (that would be an oracle for probing the LAN).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    Scheme,
    Credentials,
    /// Not a web port ([`WEB_PORTS`]).
    Port,
    Host,
    Address,
}

/// True for addresses that are reachable on the public internet. Everything else (loopback,
/// RFC 1918, link-local, CGNAT, multicast, documentation, reserved, transition mechanisms that
/// embed a private IPv4...) is refused.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || a == 0 // "this network"
        || (a == 100 && (64..=127).contains(&b)) // CGNAT 100.64/10 (also Tailscale)
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments 192.0.0/24
        || (a == 192 && b == 0 && c == 2) // TEST-NET-1
        || (a == 192 && b == 88 && c == 99) // 6to4 relay anycast 192.88.99/24 (deprecated)
        || (a == 198 && (b == 18 || b == 19)) // benchmarking 198.18/15
        || (a == 198 && b == 51 && c == 100) // TEST-NET-2
        || (a == 203 && b == 0 && c == 113) // TEST-NET-3
        || a >= 240) // reserved 240/4
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    // IPv4-mapped (::ffff:a.b.c.d) and the deprecated IPv4-compatible (::a.b.c.d) forms are
    // judged by the IPv4 address they carry.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let s = ip.segments();
    if s[..6] == [0, 0, 0, 0, 0, 0] {
        return false; // ::, ::1 and IPv4-compatible
    }
    // Only global unicast 2000::/3 is considered; that already excludes ::1, fc00::/7, fe80::/10,
    // fec0::/10, ff00::/8 and the NAT64 prefix 64:ff9b::/96.
    if s[0] & 0xe000 != 0x2000 {
        return false;
    }
    if s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8) {
        return false; // IETF protocol assignments 2001::/23 (Teredo, ORCHID, benchmarking...) and documentation 2001:db8::/32
    }
    if s[0] == 0x5f00 {
        return false; // SRv6 SIDs 5f00::/16
    }
    if s[0] == 0x3fff && s[1] < 0x1000 {
        return false; // documentation 3fff::/20
    }
    if s[0] == 0x2002 {
        // 6to4: the next 32 bits are an IPv4 address.
        let v4 = Ipv4Addr::new((s[1] >> 8) as u8, s[1] as u8, (s[2] >> 8) as u8, s[2] as u8);
        return is_public_v4(v4);
    }
    true
}

/// Ports a link may point at: the web's. (Anything else on a public address is more likely a
/// service than a page.)
pub const WEB_PORTS: [u16; 4] = [80, 443, 8080, 8443];

/// An address block: `addr` with its first `prefix` bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Net {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl Net {
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(n), IpAddr::V4(i)) => same_prefix(&n.octets(), &i.octets(), self.prefix),
            (IpAddr::V6(n), IpAddr::V6(i)) => same_prefix(&n.octets(), &i.octets(), self.prefix),
            _ => false,
        }
    }
}

fn same_prefix(a: &[u8], b: &[u8], bits: u8) -> bool {
    let bits = (bits as usize).min(a.len() * 8);
    let (bytes, rest) = (bits / 8, bits % 8);
    if a[..bytes] != b[..bytes] {
        return false;
    }
    rest == 0 || (a[bytes] ^ b[bytes]) >> (8 - rest) == 0
}

/// The networks a public interface address of this machine stands for: the address itself and
/// its on-link network, at least the /64 for IPv6 (what SLAAC hands every LAN device) but never
/// wider than a /48; at least the /24 for IPv4, never wider than a /16.
pub fn own_network(addr: IpAddr, on_link_prefix: u8) -> Net {
    let prefix = match addr {
        IpAddr::V6(_) => on_link_prefix.clamp(48, 64),
        IpAddr::V4(_) => on_link_prefix.clamp(16, 24),
    };
    Net { addr, prefix }
}

/// Every address of this machine's network interfaces, with its on-link prefix length.
#[cfg(windows)]
pub fn interface_addresses() -> Vec<(IpAddr, u8)> {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_FRIENDLY_NAME, GAA_FLAG_SKIP_MULTICAST,
        IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6};
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
        let r = unsafe { GetAdaptersAddresses(u32::from(AF_UNSPEC.0), flags, None, Some(first), &mut size) };
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
                let mut ua = (*adapter).FirstUnicastAddress;
                while !ua.is_null() {
                    let sa = (*ua).Address.lpSockaddr;
                    if !sa.is_null() {
                        let ip = match (*sa).sa_family {
                            f if f == AF_INET => {
                                let v4 = &*(sa as *const SOCKADDR_IN);
                                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.S_un.S_addr))))
                            }
                            f if f == AF_INET6 => {
                                let v6 = &*(sa as *const SOCKADDR_IN6);
                                Some(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.u.Byte)))
                            }
                            _ => None,
                        };
                        if let Some(ip) = ip {
                            out.push((ip, (*ua).OnLinkPrefixLength));
                        }
                    }
                    ua = (*ua).Next;
                }
                adapter = (*adapter).Next;
            }
        }
        return out;
    }
    out
}

/// Every address of this machine's network interfaces (not on Windows: none known).
#[cfg(not(windows))]
pub fn interface_addresses() -> Vec<(IpAddr, u8)> {
    Vec::new()
}

/// The networks to refuse because they are this machine or its LAN: each *public* interface
/// address with its on-link network ([`own_network`]). Private ones are refused anyway.
pub fn local_networks() -> Vec<Net> {
    let mut nets: Vec<Net> = interface_addresses().into_iter().filter(|(ip, _)| is_public_ip(*ip)).map(|(ip, p)| own_network(ip, p)).collect();
    nets.dedup();
    nets
}

/// What the fetcher may connect to.
#[derive(Debug, Default)]
pub struct Policy {
    /// Addresses allowed although they are not public. Empty in production; tests use it for
    /// their loopback test server.
    allow: Vec<IpAddr>,
    /// Addresses refused although they are public: the host's own (the DDNS name of this very
    /// server would otherwise loop back into it).
    deny: RwLock<Vec<IpAddr>>,
    /// Networks refused although they are public: this machine's interfaces and their on-link
    /// networks ([`local_networks`]).
    deny_nets: RwLock<Vec<Net>>,
}

impl Policy {
    pub fn new(allow: Vec<IpAddr>) -> Self {
        Policy { allow, deny: RwLock::default(), deny_nets: RwLock::default() }
    }

    pub fn set_own_addresses(&self, ips: Vec<IpAddr>) {
        *self.deny.write().unwrap_or_else(|e| e.into_inner()) = ips;
    }

    pub fn set_local_networks(&self, nets: Vec<Net>) {
        *self.deny_nets.write().unwrap_or_else(|e| e.into_inner()) = nets;
    }

    pub fn ip_ok(&self, ip: IpAddr) -> bool {
        // An IPv4-mapped IPv6 address is the same machine as its IPv4.
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        if self.deny.read().unwrap_or_else(|e| e.into_inner()).contains(&ip) {
            return false;
        }
        if self.deny_nets.read().unwrap_or_else(|e| e.into_inner()).iter().any(|n| n.contains(ip)) {
            return false;
        }
        self.allow.contains(&ip) || is_public_ip(ip)
    }

    /// The port is a web port, or the host is an explicitly allowed address (tests).
    fn port_ok(&self, url: &Url) -> bool {
        let Some(port) = url.port_or_known_default() else { return false };
        if WEB_PORTS.contains(&port) {
            return true;
        }
        match url.host() {
            Some(url::Host::Ipv4(ip)) => self.allow.contains(&IpAddr::V4(ip)),
            Some(url::Host::Ipv6(ip)) => self.allow.contains(&IpAddr::V6(ip)),
            _ => false,
        }
    }

    /// Cheap, synchronous checks on a URL before any request is made.
    pub fn check_url(&self, url: &Url) -> Result<(), Blocked> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Blocked::Scheme);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Blocked::Credentials);
        }
        match url.host() {
            None => return Err(Blocked::Host),
            Some(url::Host::Ipv4(ip)) => self.ip_ok(IpAddr::V4(ip)).then_some(()).ok_or(Blocked::Address)?,
            Some(url::Host::Ipv6(ip)) => self.ip_ok(IpAddr::V6(ip)).then_some(()).ok_or(Blocked::Address)?,
            Some(url::Host::Domain(d)) => {
                if !domain_ok(d) {
                    return Err(Blocked::Host);
                }
            }
        }
        if !self.port_ok(url) {
            return Err(Blocked::Port);
        }
        Ok(())
    }

    /// Keep the resolved addresses we may connect to.
    pub fn filter_resolved(&self, addrs: impl IntoIterator<Item = SocketAddr>) -> Vec<SocketAddr> {
        addrs.into_iter().filter(|a| self.ip_ok(a.ip())).take(8).collect()
    }
}

/// Names that can only mean "somewhere on my own network". (The resolver would catch most of them
/// through their addresses; this just saves the lookup and covers odd resolver setups.)
fn domain_ok(d: &str) -> bool {
    let d = d.trim_end_matches('.').to_ascii_lowercase();
    if d.is_empty() || !d.contains('.') {
        return false; // single-label names ("router", "localhost") are LAN / hosts-file names
    }
    const BAD_SUFFIXES: [&str; 8] =
        [".localhost", ".local", ".internal", ".lan", ".home", ".home.arpa", ".localdomain", ".intranet"];
    !BAD_SUFFIXES.iter().any(|s| d.ends_with(s))
}

/// The HTTP client's DNS resolver: resolves with the system resolver, then drops every address
/// the [`Policy`] refuses. A name with no allowed address fails to resolve at all.
pub struct SafeResolver(pub Arc<Policy>);

impl Resolve for SafeResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let policy = self.0.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let ok = policy.filter_resolved(found);
            if ok.is_empty() {
                return Err("address not allowed".into());
            }
            Ok(Box::new(ok.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn private_ranges_are_blocked() {
        for s in [
            "127.0.0.1", "127.255.255.254", "10.0.0.1", "10.255.255.255", "172.16.0.1", "172.31.255.255", "192.168.0.1",
            "192.168.1.50", "169.254.169.254", "169.254.0.1", "0.0.0.0", "0.1.2.3", "100.64.0.1", "100.127.255.255",
            "224.0.0.1", "239.255.255.250", "255.255.255.255", "240.0.0.1", "192.0.0.1", "192.0.2.1", "198.18.0.1",
            "198.51.100.1", "203.0.113.1", "::", "::1", "fc00::1", "fd12:3456::1", "fe80::1", "fec0::1", "ff02::1",
            "::ffff:127.0.0.1", "::ffff:10.1.2.3", "::ffff:192.168.1.1", "::127.0.0.1", "64:ff9b::7f00:1",
            "2002:7f00:1::1", "2002:c0a8:0101::1", "2001:db8::1", "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "192.88.99.1", "2001:2::1", "2001:10::1", "2001:20::1", "2001:1ff::1", "5f00::1",
        ] {
            assert!(!is_public_ip(ip(s)), "{s} must be blocked");
        }
    }

    #[test]
    fn public_addresses_pass() {
        for s in [
            "8.8.8.8", "1.1.1.1", "142.250.80.46", "172.15.0.1", "172.32.0.1", "100.63.255.255", "100.128.0.1",
            "223.255.255.255", "2606:4700:4700::1111", "2a00:1450:4001:81b::200e", "::ffff:8.8.8.8", "2002:0808:0808::1",
        ] {
            assert!(is_public_ip(ip(s)), "{s} must be allowed");
        }
    }

    #[test]
    fn urls_are_checked_before_any_request() {
        let p = Policy::new(vec![]);
        let check = |s: &str| p.check_url(&Url::parse(s).unwrap());
        assert_eq!(check("https://example.com/x"), Ok(()));
        assert_eq!(check("http://example.com:8080/"), Ok(()));
        assert_eq!(check("https://example.com:8443/"), Ok(()));
        assert_eq!(check("http://example.com:22/"), Err(Blocked::Port));
        assert_eq!(check("http://8.8.8.8:7890/api/health"), Err(Blocked::Port));
        assert_eq!(check("https://example.com:7880/"), Err(Blocked::Port));
        assert_eq!(check("ftp://example.com/"), Err(Blocked::Scheme));
        assert_eq!(check("file:///C:/Windows/win.ini"), Err(Blocked::Scheme));
        assert_eq!(check("http://user:pw@example.com/"), Err(Blocked::Credentials));
        assert_eq!(check("http://127.0.0.1:7890/api/health"), Err(Blocked::Address));
        assert_eq!(check("http://[::1]:7890/"), Err(Blocked::Address));
        assert_eq!(check("http://10.0.0.5/"), Err(Blocked::Address));
        assert_eq!(check("http://192.168.1.1/"), Err(Blocked::Address));
        assert_eq!(check("http://169.254.169.254/latest/meta-data/"), Err(Blocked::Address));
        assert_eq!(check("http://[fc00::1]/"), Err(Blocked::Address));
        assert_eq!(check("http://[::ffff:127.0.0.1]/"), Err(Blocked::Address));
        // The URL parser turns these tricks into plain IPv4.
        assert_eq!(check("http://2130706433/"), Err(Blocked::Address)); // 127.0.0.1 as one number
        assert_eq!(check("http://0x7f.1/"), Err(Blocked::Address));
        assert_eq!(check("http://0177.0.0.1/"), Err(Blocked::Address));
        assert_eq!(check("http://127.1/"), Err(Blocked::Address));
        assert_eq!(check("http://localhost:7890/"), Err(Blocked::Host));
        assert_eq!(check("http://router/"), Err(Blocked::Host));
        assert_eq!(check("http://printer.local/"), Err(Blocked::Host));
        assert_eq!(check("http://nas.lan/"), Err(Blocked::Host));
        assert_eq!(check("http://app.localhost/"), Err(Blocked::Host));
    }

    #[test]
    fn own_address_is_denied_and_test_exceptions_work() {
        let p = Policy::new(vec![ip("127.0.0.1")]);
        assert!(p.ip_ok(ip("127.0.0.1")));
        assert!(!p.ip_ok(ip("127.0.0.2")));
        assert!(p.ip_ok(ip("::ffff:127.0.0.1")));
        let p = Policy::new(vec![]);
        p.set_own_addresses(vec![ip("203.0.114.9")]);
        assert!(!p.ip_ok(ip("203.0.114.9")));
        assert!(!p.ip_ok(ip("::ffff:203.0.114.9")));
        assert!(p.ip_ok(ip("203.0.114.10")));
    }

    #[test]
    fn the_hosts_own_networks_are_denied() {
        let p = Policy::new(vec![]);
        // Today's gap: a global IPv6 address of this machine, or of a LAN device, looks public.
        assert!(p.ip_ok(ip("2a01:4f8:1:2::10")));
        // The host's interface 2a01:4f8:1:2::5 (on-link /64): itself and its LAN are refused;
        // another network is not.
        p.set_local_networks(vec![own_network(ip("2a01:4f8:1:2::5"), 64), own_network(ip("81.2.69.160"), 28)]);
        assert!(!p.ip_ok(ip("2a01:4f8:1:2::5")));
        assert!(!p.ip_ok(ip("2a01:4f8:1:2::10")), "a LAN device on the same /64");
        assert!(!p.ip_ok(ip("2a01:4f8:1:2:aaaa:bbbb:cccc:dddd")));
        assert!(p.ip_ok(ip("2a01:4f8:1:3::10")), "the next /64 is somebody else");
        assert!(!p.ip_ok(ip("81.2.69.161")), "public IPv4 on an interface: its /24");
        assert!(!p.ip_ok(ip("::ffff:81.2.69.170")));
        assert!(p.ip_ok(ip("81.2.70.1")));
        // On-link prefixes are clamped: a /128 still covers the /64, a /32 only the /48.
        assert_eq!(own_network(ip("2a01::1"), 128).prefix, 64);
        assert_eq!(own_network(ip("2a01::1"), 32).prefix, 48);
        assert_eq!(own_network(ip("81.2.69.160"), 8).prefix, 16);
        // The test exception never beats a denied network.
        let p = Policy::new(vec![ip("2a01:4f8:1:2::10")]);
        p.set_local_networks(vec![own_network(ip("2a01:4f8:1:2::5"), 64)]);
        assert!(!p.ip_ok(ip("2a01:4f8:1:2::10")));
    }

    /// The real interface list: it has this machine's loopback, and every public address it has
    /// (a global IPv6 address on a home network, say) ends up refused.
    #[cfg(windows)]
    #[test]
    fn this_machines_addresses_are_found_and_refused() {
        let all = interface_addresses();
        assert!(all.iter().any(|(a, _)| a.is_loopback()), "{all:?}");
        let p = Policy::new(vec![]);
        p.set_local_networks(local_networks());
        for (a, _) in all.iter().filter(|(a, _)| is_public_ip(*a)) {
            assert!(!p.ip_ok(*a), "{a} is this machine");
        }
    }

    #[test]
    fn resolver_keeps_only_public_answers() {
        let p = Policy::new(vec![]);
        let sa = |s: &str| SocketAddr::new(ip(s), 0);
        // A name that answers with both a private and a public address: only the public one stays.
        let kept = p.filter_resolved([sa("192.168.1.10"), sa("93.184.216.34"), sa("::1")]);
        assert_eq!(kept, vec![sa("93.184.216.34")]);
        // A name that only points inside (DNS rebinding to 127.0.0.1): nothing left.
        assert!(p.filter_resolved([sa("127.0.0.1")]).is_empty());
        assert!(p.filter_resolved([sa("fd00::5"), sa("10.0.0.1")]).is_empty());
    }

    /// The real resolver, against the system's own answer for "localhost" (127.0.0.1 and/or ::1):
    /// a name that resolves to a private address resolves to nothing.
    #[tokio::test]
    async fn resolver_refuses_names_that_point_inside() {
        let name = || "localhost".parse::<Name>().unwrap();
        let strict = SafeResolver(Arc::new(Policy::new(vec![])));
        assert!(strict.resolve(name()).await.is_err());
        // With the loopback exception the 127.0.0.1 answer stays and ::1 is dropped.
        let lax = SafeResolver(Arc::new(Policy::new(vec![ip("127.0.0.1")])));
        let got: Vec<SocketAddr> = lax.resolve(name()).await.unwrap().collect();
        assert!(!got.is_empty() && got.iter().all(|a| a.ip() == ip("127.0.0.1")), "{got:?}");
    }
}
