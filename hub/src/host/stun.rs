//! A minimal STUN Binding request (RFC 5389): which public address does the internet see for this
//! PC? Compared with the router's own WAN address it tells whether the provider puts the
//! connection behind CGNAT (see `nat.rs`). One UDP packet each way, no dependency.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

/// Public STUN servers, tried in order.
pub const SERVERS: [&str; 2] = ["stun.l.google.com:19302", "stun.cloudflare.com:3478"];

const MAGIC: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// A Binding request: the 20-byte header and no attributes.
pub fn request(tid: &[u8; 12]) -> [u8; 20] {
    let mut m = [0u8; 20];
    m[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // length 0
    m[4..8].copy_from_slice(&MAGIC.to_be_bytes());
    m[8..20].copy_from_slice(tid);
    m
}

/// The address in a Binding success answer to the request `tid`: XOR-MAPPED-ADDRESS, else
/// MAPPED-ADDRESS (very old servers). `None` for anything else.
pub fn parse_response(buf: &[u8], tid: &[u8; 12]) -> Option<SocketAddr> {
    if buf.len() < 20 || u16::from_be_bytes([buf[0], buf[1]]) != BINDING_SUCCESS {
        return None;
    }
    let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    if u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) != MAGIC || &buf[8..20] != tid || buf.len() < 20 + len {
        return None;
    }
    let mut at = 20;
    let end = 20 + len;
    let mut mapped = None;
    while at + 4 <= end {
        let kind = u16::from_be_bytes([buf[at], buf[at + 1]]);
        let alen = u16::from_be_bytes([buf[at + 2], buf[at + 3]]) as usize;
        let value = buf.get(at + 4..at + 4 + alen).filter(|_| at + 4 + alen <= end)?;
        match kind {
            ATTR_XOR_MAPPED_ADDRESS => {
                if let Some(a) = address(value, Some(tid)) {
                    return Some(a);
                }
            }
            ATTR_MAPPED_ADDRESS => mapped = mapped.or_else(|| address(value, None)),
            _ => {}
        }
        at += 4 + alen.div_ceil(4) * 4;
    }
    mapped
}

/// `value` of a (XOR-)MAPPED-ADDRESS: reserved byte, family, port, address. With `xor_tid`, the
/// port and address are XORed with the magic cookie (and, for IPv6, the transaction id).
fn address(value: &[u8], xor_tid: Option<&[u8; 12]>) -> Option<SocketAddr> {
    if value.len() < 8 {
        return None;
    }
    let cookie = MAGIC.to_be_bytes();
    let mut port = u16::from_be_bytes([value[2], value[3]]);
    if xor_tid.is_some() {
        port ^= (MAGIC >> 16) as u16;
    }
    let ip = match value[1] {
        0x01 => {
            let mut o = [value[4], value[5], value[6], value[7]];
            if xor_tid.is_some() {
                for (b, k) in o.iter_mut().zip(cookie) {
                    *b ^= k;
                }
            }
            IpAddr::V4(Ipv4Addr::from(o))
        }
        0x02 if value.len() >= 20 => {
            let mut o = [0u8; 16];
            o.copy_from_slice(&value[4..20]);
            if let Some(tid) = xor_tid {
                let key: Vec<u8> = cookie.iter().chain(tid.iter()).copied().collect();
                for (b, k) in o.iter_mut().zip(key) {
                    *b ^= k;
                }
            }
            IpAddr::V6(Ipv6Addr::from(o))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// This PC's public IPv4 address as a STUN server sees it (`None`: no answer from any server
/// within `timeout` each, e.g. offline or UDP blocked).
pub async fn public_ipv4(timeout: Duration) -> Option<Ipv4Addr> {
    for server in SERVERS {
        match ask(server, timeout).await {
            Some(SocketAddr::V4(a)) => return Some(*a.ip()),
            other => tracing::debug!(server, ?other, "stun: no IPv4 answer"),
        }
    }
    None
}

async fn ask(server: &str, timeout: Duration) -> Option<SocketAddr> {
    let addr = tokio::time::timeout(timeout, tokio::net::lookup_host(server)).await.ok()?.ok()?.find(|a| a.is_ipv4())?;
    let sock = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.ok()?;
    let tid: [u8; 12] = crate::util::random_bytes();
    let mut buf = [0u8; 512];
    // Two tries: UDP may drop the first packet.
    for _ in 0..2 {
        sock.send_to(&request(&tid), addr).await.ok()?;
        let until = tokio::time::Instant::now() + timeout / 2;
        while let Ok(Ok((n, from))) = tokio::time::timeout_at(until, sock.recv_from(&mut buf)).await {
            if from == addr {
                if let Some(a) = parse_response(&buf[..n], &tid) {
                    return Some(a);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const TID: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

    fn answer(attrs: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (kind, v) in attrs {
            body.extend_from_slice(&kind.to_be_bytes());
            body.extend_from_slice(&(v.len() as u16).to_be_bytes());
            body.extend_from_slice(v);
            while body.len() % 4 != 0 {
                body.push(0);
            }
        }
        let mut m = Vec::new();
        m.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        m.extend_from_slice(&(body.len() as u16).to_be_bytes());
        m.extend_from_slice(&MAGIC.to_be_bytes());
        m.extend_from_slice(&TID);
        m.extend_from_slice(&body);
        m
    }

    fn xor_v4(ip: [u8; 4], port: u16) -> Vec<u8> {
        let c = MAGIC.to_be_bytes();
        let p = (port ^ (MAGIC >> 16) as u16).to_be_bytes();
        vec![0, 1, p[0], p[1], ip[0] ^ c[0], ip[1] ^ c[1], ip[2] ^ c[2], ip[3] ^ c[3]]
    }

    #[test]
    fn the_request_is_a_binding_request() {
        let r = request(&TID);
        assert_eq!(&r[..4], &[0, 1, 0, 0]);
        assert_eq!(&r[4..8], &[0x21, 0x12, 0xA4, 0x42]);
        assert_eq!(&r[8..], &TID);
    }

    #[test]
    fn xor_mapped_addresses_are_read() {
        let a = answer(&[(0x8022, b"test software".to_vec()), (ATTR_XOR_MAPPED_ADDRESS, xor_v4([203, 0, 113, 9], 54321))]);
        assert_eq!(parse_response(&a, &TID), Some("203.0.113.9:54321".parse().unwrap()));
        // IPv6: XOR with the cookie and the transaction id.
        let ip: Ipv6Addr = "2001:db8::42".parse().unwrap();
        let key: Vec<u8> = MAGIC.to_be_bytes().iter().chain(TID.iter()).copied().collect();
        let p = (7000u16 ^ (MAGIC >> 16) as u16).to_be_bytes();
        let mut v = vec![0, 2, p[0], p[1]];
        v.extend(ip.octets().iter().zip(&key).map(|(b, k)| b ^ k));
        assert_eq!(parse_response(&answer(&[(ATTR_XOR_MAPPED_ADDRESS, v)]), &TID), Some(SocketAddr::new(ip.into(), 7000)));
    }

    #[test]
    fn plain_mapped_address_is_the_fallback() {
        let plain = vec![0, 1, 0x1f, 0x90, 198, 51, 100, 7];
        let a = answer(&[(ATTR_MAPPED_ADDRESS, plain.clone())]);
        assert_eq!(parse_response(&a, &TID), Some("198.51.100.7:8080".parse().unwrap()));
        // XOR-MAPPED wins when both are there.
        let a = answer(&[(ATTR_MAPPED_ADDRESS, plain), (ATTR_XOR_MAPPED_ADDRESS, xor_v4([203, 0, 113, 1], 1))]);
        assert_eq!(parse_response(&a, &TID), Some("203.0.113.1:1".parse().unwrap()));
    }

    #[test]
    fn foreign_or_broken_answers_are_ignored() {
        let good = answer(&[(ATTR_XOR_MAPPED_ADDRESS, xor_v4([203, 0, 113, 9], 1))]);
        assert_eq!(parse_response(&good, &[0; 12]), None, "another transaction");
        let mut err = good.clone();
        err[0..2].copy_from_slice(&0x0111u16.to_be_bytes());
        assert_eq!(parse_response(&err, &TID), None, "an error response");
        assert_eq!(parse_response(&good[..good.len() - 2], &TID), None, "cut short");
        let mut bad_len = answer(&[(ATTR_XOR_MAPPED_ADDRESS, xor_v4([203, 0, 113, 9], 1))]);
        bad_len[22..24].copy_from_slice(&200u16.to_be_bytes());
        assert_eq!(parse_response(&bad_len, &TID), None, "attribute longer than the message");
        assert_eq!(parse_response(&[0u8; 10], &TID), None);
        assert_eq!(parse_response(&answer(&[]), &TID), None, "no address");
    }
}
