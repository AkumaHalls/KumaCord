//! Opening the ports on the router by itself (UPnP IGD), so friends on the internet reach the
//! hosted server without anybody touching the router.
//!
//! The protocol work is `igd-next` (pure Rust, sync, a small HTTP/1 client without TLS: about the
//! smallest way to speak SSDP + SOAP to a router). It runs on the blocking pool. Everything here
//! goes through [`PortMapper`], so the logic is tested against a fake router and the tests never
//! touch a real one ([`super::NetMode`]).
//!
//! Mappings are leased for [`LEASE_SECS`] and renewed every [`RENEW_EVERY`] while the server runs,
//! and removed when it stops. Routers that only accept permanent mappings get those (removed on
//! stop too; after a crash they stay until the router forgets them or they are reused next time).

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use super::ports::PortBlock;

/// How long a mapping lives without renewal.
pub const LEASE_SECS: u32 = 3600;
/// Mappings are renewed this often (well inside the lease).
pub const RENEW_EVERY: Duration = Duration::from_secs(20 * 60);
/// What routers show next to the mapping.
const DESCRIPTION: &str = "concord";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    pub fn name(self) -> &'static str {
        match self {
            Proto::Tcp => "TCP",
            Proto::Udp => "UDP",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapError {
    /// The router only takes permanent mappings (lease 0).
    OnlyPermanent,
    /// Another device already has this port.
    InUse,
    Other(String),
}

/// A router that maps ports (the real one is [`Igd`]; tests use a fake).
pub trait PortMapper: Send + Sync {
    /// The router's WAN address.
    fn external_ip(&self) -> Result<IpAddr, String>;
    /// This PC's address as the router sees it (where mapped traffic goes).
    fn local_ip(&self) -> Option<Ipv4Addr>;
    fn add(&self, proto: Proto, port: u16, to: SocketAddrV4, lease_secs: u32) -> Result<(), MapError>;
    fn remove(&self, proto: Proto, port: u16) -> Result<(), String>;
}

/// The ports of a block, as mapped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mapped {
    pub ports: Vec<(Proto, u16)>,
    /// The router only took permanent mappings.
    pub permanent: bool,
    /// Ports the router refused, with why.
    pub failed: Vec<(Proto, u16, String)>,
}

impl Mapped {
    pub fn complete(&self) -> bool {
        self.failed.is_empty() && !self.ports.is_empty()
    }
}

fn wanted(block: &PortBlock) -> [(Proto, u16); 4] {
    [(Proto::Tcp, block.hub), (Proto::Tcp, block.signal), (Proto::Tcp, block.rtc_tcp), (Proto::Udp, block.rtc_udp)]
}

/// Map every port of `block` to `local` (same port numbers outside and inside). Also renews:
/// mapping a port again just extends its lease.
pub fn map_block(m: &dyn PortMapper, block: &PortBlock, local: Ipv4Addr, permanent_only: bool) -> Mapped {
    let mut out = Mapped { permanent: permanent_only, ..Default::default() };
    for (proto, port) in wanted(block) {
        let to = SocketAddrV4::new(local, port);
        let mut r = m.add(proto, port, to, if out.permanent { 0 } else { LEASE_SECS });
        if r == Err(MapError::OnlyPermanent) && !out.permanent {
            out.permanent = true;
            r = m.add(proto, port, to, 0);
        }
        match r {
            Ok(()) => out.ports.push((proto, port)),
            Err(e) => {
                let why = match e {
                    MapError::InUse => "a porta já está aberta para outro aparelho".to_string(),
                    MapError::OnlyPermanent => "o roteador recusou".to_string(),
                    MapError::Other(e) => e,
                };
                tracing::warn!(proto = proto.name(), port, why, "upnp: mapping refused");
                out.failed.push((proto, port, why));
            }
        }
    }
    tracing::info!(mapped = ?out.ports, permanent = out.permanent, failed = out.failed.len(), %local, "upnp: ports mapped");
    out
}

/// Remove what [`map_block`] made (errors are only logged: the lease ends anyway).
pub fn unmap(m: &dyn PortMapper, mapped: &Mapped) {
    for (proto, port) in &mapped.ports {
        if let Err(e) = m.remove(*proto, *port) {
            tracing::warn!(proto = proto.name(), port, error = %e, "upnp: could not remove a mapping");
        }
    }
    if !mapped.ports.is_empty() {
        tracing::info!(count = mapped.ports.len(), "upnp: mappings removed");
    }
}

// ---------------------------------------------------------------- the real router

/// A router found on the network.
pub struct Igd {
    gw: igd_next::Gateway,
    local: Option<Ipv4Addr>,
}

/// Look for the router (SSDP), for at most `timeout`. Read-only: nothing is mapped yet.
pub fn discover(timeout: Duration) -> Result<Igd, String> {
    let mut opts = igd_next::SearchOptions::default();
    opts.timeout = Some(timeout);
    opts.single_search_timeout = Some(timeout);
    let gw = igd_next::search_gateway(opts).map_err(|e| e.to_string())?;
    let local = local_ip_towards(gw.addr);
    tracing::info!(gateway = %gw.addr, ?local, "upnp: router found");
    Ok(Igd { gw, local })
}

/// The address of this PC on the route to `peer` (nothing is sent).
fn local_ip_towards(peer: SocketAddr) -> Option<Ipv4Addr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect(peer).ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

fn igd_proto(p: Proto) -> igd_next::PortMappingProtocol {
    match p {
        Proto::Tcp => igd_next::PortMappingProtocol::TCP,
        Proto::Udp => igd_next::PortMappingProtocol::UDP,
    }
}

impl PortMapper for Igd {
    fn external_ip(&self) -> Result<IpAddr, String> {
        self.gw.get_external_ip().map_err(|e| e.to_string())
    }

    fn local_ip(&self) -> Option<Ipv4Addr> {
        self.local
    }

    fn add(&self, proto: Proto, port: u16, to: SocketAddrV4, lease_secs: u32) -> Result<(), MapError> {
        use igd_next::AddPortError as E;
        self.gw.add_port(igd_proto(proto), port, SocketAddr::V4(to), lease_secs, DESCRIPTION).map_err(|e| match e {
            E::OnlyPermanentLeasesSupported => MapError::OnlyPermanent,
            E::PortInUse => MapError::InUse,
            other => MapError::Other(other.to_string()),
        })
    }

    fn remove(&self, proto: Proto, port: u16) -> Result<(), String> {
        self.gw.remove_port(igd_proto(proto), port).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
pub mod fake {
    //! A router in memory (tests).
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeIgd {
        pub wan: Option<IpAddr>,
        pub permanent_only: bool,
        /// Ports some other device already has.
        pub taken: Vec<(Proto, u16)>,
        /// (proto, port) -> (target, lease)
        pub table: Mutex<HashMap<(Proto, u16), (SocketAddrV4, u32)>>,
        pub adds: Mutex<u32>,
    }

    impl PortMapper for FakeIgd {
        fn external_ip(&self) -> Result<IpAddr, String> {
            self.wan.ok_or_else(|| "no WAN".to_string())
        }
        fn local_ip(&self) -> Option<Ipv4Addr> {
            Some(Ipv4Addr::new(192, 168, 0, 20))
        }
        fn add(&self, proto: Proto, port: u16, to: SocketAddrV4, lease_secs: u32) -> Result<(), MapError> {
            *self.adds.lock().unwrap() += 1;
            if self.taken.contains(&(proto, port)) {
                return Err(MapError::InUse);
            }
            if self.permanent_only && lease_secs != 0 {
                return Err(MapError::OnlyPermanent);
            }
            self.table.lock().unwrap().insert((proto, port), (to, lease_secs));
            Ok(())
        }
        fn remove(&self, proto: Proto, port: u16) -> Result<(), String> {
            self.table.lock().unwrap().remove(&(proto, port)).map(|_| ()).ok_or_else(|| "not mapped".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeIgd;
    use super::*;

    const LOCAL: Ipv4Addr = Ipv4Addr::new(192, 168, 0, 20);

    #[test]
    fn every_port_is_mapped_leased_and_removed() {
        let r = FakeIgd { wan: Some("203.0.113.7".parse().unwrap()), ..Default::default() };
        let b = PortBlock::nth(1);
        let m = map_block(&r, &b, LOCAL, false);
        assert!(m.complete() && !m.permanent);
        assert_eq!(m.ports, [(Proto::Tcp, 7990), (Proto::Tcp, 7980), (Proto::Tcp, 7981), (Proto::Udp, 7982)]);
        {
            let t = r.table.lock().unwrap();
            assert_eq!(t.len(), 4);
            assert_eq!(t[&(Proto::Udp, 7982)], (SocketAddrV4::new(LOCAL, 7982), LEASE_SECS));
        }
        // Renewing maps the same ports again.
        let again = map_block(&r, &b, LOCAL, m.permanent);
        assert_eq!(again, m);
        assert_eq!(r.table.lock().unwrap().len(), 4);
        unmap(&r, &m);
        assert!(r.table.lock().unwrap().is_empty());
    }

    #[test]
    fn permanent_only_routers_get_permanent_mappings() {
        let r = FakeIgd { permanent_only: true, ..Default::default() };
        let m = map_block(&r, &PortBlock::DEFAULT, LOCAL, false);
        assert!(m.complete() && m.permanent);
        assert!(r.table.lock().unwrap().values().all(|(_, lease)| *lease == 0));
        // Renewals go straight to permanent (no refused try first).
        let before = *r.adds.lock().unwrap();
        map_block(&r, &PortBlock::DEFAULT, LOCAL, m.permanent);
        assert_eq!(*r.adds.lock().unwrap() - before, 4);
        unmap(&r, &m);
        assert!(r.table.lock().unwrap().is_empty());
    }

    /// Read-only look at the real router (run by hand: `cargo test -p voxhub-server
    /// real_router_read_only -- --ignored --nocapture`): search and ask for the WAN address,
    /// never map anything.
    #[test]
    #[ignore]
    fn real_router_read_only() {
        match discover(Duration::from_secs(4)) {
            Ok(igd) => eprintln!("router found; this PC {:?}; WAN address {:?}", igd.local_ip(), igd.external_ip()),
            Err(e) => eprintln!("no UPnP router: {e}"),
        }
    }

    #[test]
    fn a_port_taken_by_another_device_is_reported() {
        let r = FakeIgd { taken: vec![(Proto::Udp, 7882)], ..Default::default() };
        let m = map_block(&r, &PortBlock::DEFAULT, LOCAL, false);
        assert!(!m.complete());
        assert_eq!(m.ports.len(), 3);
        assert_eq!(m.failed.len(), 1);
        assert_eq!((m.failed[0].0, m.failed[0].1), (Proto::Udp, 7882));
        unmap(&r, &m);
        assert!(r.table.lock().unwrap().is_empty());
    }
}
