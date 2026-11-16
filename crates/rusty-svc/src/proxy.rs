//! The PROXY protocol, version 2, as the edge (HAProxy `send-proxy-v2`) speaks it to the services behind it, and the `Accept` that uses it.
//!
//! The edge terminates TLS and applies the connection limits; the service behind it sees the proxy's address, not the client's. PROXY v2 puts the
//! client's address in front of the stream. Trusting that header is a security decision: **a header from an address that is not a trusted proxy is
//! never read** (the connection is dropped), so a client cannot claim an address. Everything else about the parser is strict:
//!
//! * the 12-byte signature, version 2 and a known command are required; the header length is bounded ([`MAX_PAYLOAD`]) before it is read;
//! * `LOCAL` (the proxy's own health checks) and `PROXY` with an unspecified family keep the proxy's address as the peer;
//! * `PROXY` over TCP for IPv4 or IPv6 gives the source address; a UNIX family, a datagram transport or a truncated address block is refused;
//! * the TLV list is walked with every length checked against the payload; the SSL TLV (`PP2_TYPE_SSL`, client bit) tells that the client's connection used
//!   TLS at the edge.
//!
//! The parser is a pure function of bytes ([`parse`]) so that it can be fuzzed; nothing is buffered beyond the header, so the session starts exactly at the
//! first byte after it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::cidr::Cidr;
use rusty_core::{Error, Result};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use crate::accept::{Accept, BoxedStream, PeerInfo};

/// The 12 bytes that start every PROXY v2 header.
pub const SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";
/// The fixed part: signature, version and command, family and transport, payload length.
pub const PREFIX_LEN: usize = 16;
/// The most payload (addresses and TLVs) accepted. HAProxy sends a few dozen bytes; 2 KiB leaves room for its optional TLVs.
pub const MAX_PAYLOAD: usize = 2048;
/// How long the proxy may take to send the header.
pub const DEFAULT_HEADER_TIMEOUT: Duration = Duration::from_secs(5);

const PP2_TYPE_SSL: u8 = 0x20;
const PP2_CLIENT_SSL: u8 = 0x01;

/// Why a header was refused. The text is fixed; nothing of the input is echoed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyError {
    Signature,
    Version,
    Command,
    Transport,
    Family,
    TooLong,
    Truncated,
    Tlv,
}

/// What a header says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// The client address; `None` for `LOCAL` and for an unspecified family (keep the proxy's own address).
    pub source: Option<SocketAddr>,
    /// The client's connection used TLS at the edge (the SSL TLV with its client bit).
    pub ssl: bool,
}

/// The payload length announced by the 16-byte prefix, after checking the signature, the version and the command.
pub fn payload_len(prefix: &[u8; PREFIX_LEN]) -> std::result::Result<usize, ProxyError> {
    if prefix.get(..12) != Some(&SIGNATURE[..]) {
        return Err(ProxyError::Signature);
    }
    let ver_cmd = *prefix.get(12).ok_or(ProxyError::Truncated)?;
    if ver_cmd >> 4 != 0x2 {
        return Err(ProxyError::Version);
    }
    if !matches!(ver_cmd & 0x0f, 0x0 | 0x1) {
        return Err(ProxyError::Command);
    }
    let len = usize::from(u16::from_be_bytes([*prefix.get(14).ok_or(ProxyError::Truncated)?, *prefix.get(15).ok_or(ProxyError::Truncated)?]));
    if len > MAX_PAYLOAD {
        return Err(ProxyError::TooLong);
    }
    Ok(len)
}

/// Parse a whole header: the 16-byte prefix and its payload (exactly the length the prefix announced).
pub fn parse(prefix: &[u8; PREFIX_LEN], payload: &[u8]) -> std::result::Result<Header, ProxyError> {
    let len = payload_len(prefix)?;
    if payload.len() != len {
        return Err(ProxyError::Truncated);
    }
    let ver_cmd = *prefix.get(12).ok_or(ProxyError::Truncated)?;
    let fam_trans = *prefix.get(13).ok_or(ProxyError::Truncated)?;
    // LOCAL: the proxy itself (a health check); whatever follows the prefix is ignored.
    if ver_cmd & 0x0f == 0x0 {
        return Ok(Header { source: None, ssl: false });
    }
    let (family, transport) = (fam_trans >> 4, fam_trans & 0x0f);
    let addr_len = match family {
        // Unspecified: the proxy does not know the client (the spec says to use the connection's own addresses).
        0x0 => return Ok(Header { source: None, ssl: false }),
        0x1 => 12,
        0x2 => 36,
        _ => return Err(ProxyError::Family),
    };
    if transport != 0x1 {
        return Err(ProxyError::Transport);
    }
    if payload.len() < addr_len {
        return Err(ProxyError::Truncated);
    }
    let (block, tlvs) = payload.split_at(addr_len);
    let source = if family == 0x1 {
        let ip: [u8; 4] = block.get(0..4).and_then(|b| b.try_into().ok()).ok_or(ProxyError::Truncated)?;
        let port: [u8; 2] = block.get(8..10).and_then(|b| b.try_into().ok()).ok_or(ProxyError::Truncated)?;
        SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), u16::from_be_bytes(port))
    } else {
        let ip: [u8; 16] = block.get(0..16).and_then(|b| b.try_into().ok()).ok_or(ProxyError::Truncated)?;
        let port: [u8; 2] = block.get(32..34).and_then(|b| b.try_into().ok()).ok_or(ProxyError::Truncated)?;
        SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), u16::from_be_bytes(port))
    };
    Ok(Header { source: Some(source), ssl: ssl_of(tlvs)? })
}

/// Walk the TLVs (type, 16-bit length, value) checking every length; true when the SSL TLV says the client used TLS.
fn ssl_of(mut tlvs: &[u8]) -> std::result::Result<bool, ProxyError> {
    let mut ssl = false;
    while !tlvs.is_empty() {
        let head = tlvs.get(..3).ok_or(ProxyError::Tlv)?;
        let (kind, len) = (*head.first().ok_or(ProxyError::Tlv)?, usize::from(u16::from_be_bytes([*head.get(1).ok_or(ProxyError::Tlv)?, *head.get(2).ok_or(ProxyError::Tlv)?])));
        let value = tlvs.get(3..3 + len).ok_or(ProxyError::Tlv)?;
        if kind == PP2_TYPE_SSL {
            // client flags (1 byte) and verify (4 bytes), then sub-TLVs.
            let client = *value.first().ok_or(ProxyError::Tlv)?;
            if value.len() < 5 {
                return Err(ProxyError::Tlv);
            }
            ssl = client & PP2_CLIENT_SSL != 0;
        }
        tlvs = tlvs.get(3 + len..).ok_or(ProxyError::Tlv)?;
    }
    Ok(ssl)
}

/// Build a header (for tests and for tools that act as a proxy): `PROXY` over TCP with the given source and destination.
pub fn encode(source: SocketAddr, dest: SocketAddr, ssl: bool) -> Vec<u8> {
    let mut out = SIGNATURE.to_vec();
    out.push(0x21);
    let mut payload = Vec::new();
    match (source.ip(), dest.ip()) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            out.push(0x11);
            payload.extend_from_slice(&s.octets());
            payload.extend_from_slice(&d.octets());
        }
        (s, d) => {
            out.push(0x21);
            let to6 = |ip: IpAddr| match ip {
                IpAddr::V6(a) => a,
                IpAddr::V4(a) => a.to_ipv6_mapped(),
            };
            payload.extend_from_slice(&to6(s).octets());
            payload.extend_from_slice(&to6(d).octets());
        }
    }
    payload.extend_from_slice(&source.port().to_be_bytes());
    payload.extend_from_slice(&dest.port().to_be_bytes());
    if ssl {
        payload.extend_from_slice(&[PP2_TYPE_SSL, 0, 5, PP2_CLIENT_SSL, 0, 0, 0, 0]);
    }
    out.extend_from_slice(&u16::try_from(payload.len()).unwrap_or(u16::MAX).to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

/// A connection from the edge proxy: the PROXY v2 header names the client. `edge_terminates_tls` says that this listener's TLS (implicit TLS ports 465 and 993)
/// ended at the edge, so the stream is already protected.
#[derive(Debug, Clone)]
pub struct EdgeAccept {
    trusted: Vec<Cidr>,
    named: crate::NamedPeers,
    edge_terminates_tls: bool,
    timeout: Duration,
}

impl EdgeAccept {
    pub fn new(trusted: Vec<Cidr>, edge_terminates_tls: bool) -> EdgeAccept {
        EdgeAccept { trusted, named: crate::NamedPeers::none(), edge_terminates_tls, timeout: DEFAULT_HEADER_TIMEOUT }
    }

    /// Also trust the addresses of these names (a swarm has no fixed address: the edge is found by its service name).
    pub fn with_named(mut self, named: crate::NamedPeers) -> EdgeAccept {
        self.named = named;
        self
    }

    pub fn with_header_timeout(mut self, timeout: Duration) -> EdgeAccept {
        self.timeout = timeout;
        self
    }

    fn is_trusted(&self, ip: IpAddr) -> bool {
        self.trusted.iter().any(|c| c.contains(ip)) || self.named.contains(ip)
    }
}

#[async_trait]
impl Accept for EdgeAccept {
    /// The client's address is known only after the header, so the per-address cap is applied by the listener afterwards, on the real client.
    fn per_ip_after_accept(&self) -> bool {
        true
    }

    async fn accept(&self, mut tcp: TcpStream, addr: SocketAddr) -> Result<(BoxedStream, PeerInfo)> {
        // Not a trusted proxy: nothing is read, whatever it sends (a client cannot claim an address).
        if !self.is_trusted(addr.ip()) {
            return Err(Error::Invalid("not a trusted proxy".into()));
        }
        let read = async {
            let mut prefix = [0u8; PREFIX_LEN];
            tcp.read_exact(&mut prefix).await.map_err(|_| ProxyError::Truncated)?;
            let len = payload_len(&prefix)?;
            let mut payload = vec![0u8; len];
            tcp.read_exact(&mut payload).await.map_err(|_| ProxyError::Truncated)?;
            parse(&prefix, &payload)
        };
        let header = match tokio::time::timeout(self.timeout, read).await {
            Ok(Ok(h)) => h,
            Ok(Err(_)) => return Err(Error::Invalid("invalid PROXY header".into())),
            Err(_) => return Err(Error::Invalid("PROXY header timed out".into())),
        };
        let peer = PeerInfo { addr: header.source.unwrap_or(addr), tls: self.edge_terminates_tls || header.ssl };
        Ok((Box::new(tcp), peer))
    }
}

/// The invariants of the parser for arbitrary input (the fuzz target and a property test call this): it never panics, a header it accepts has a payload of
/// exactly the announced length within the cap, and an accepted client address encodes and parses back to itself.
pub fn check_invariants(data: &[u8]) {
    let mut prefix = [0u8; PREFIX_LEN];
    let take = data.len().min(PREFIX_LEN);
    if let (Some(dst), Some(src)) = (prefix.get_mut(..take), data.get(..take)) {
        dst.copy_from_slice(src);
    }
    let rest = data.get(take..).unwrap_or_default();
    // Arbitrary bytes, and the same bytes behind a valid signature (so that the deeper checks are reached).
    for with_signature in [false, true] {
        let mut p = prefix;
        if with_signature {
            if let Some(sig) = p.get_mut(..12) {
                sig.copy_from_slice(&SIGNATURE);
            }
        }
        let announced = payload_len(&p);
        if let Ok(len) = announced {
            assert!(len <= MAX_PAYLOAD);
        }
        let payload = rest.get(..announced.unwrap_or(0).min(rest.len())).unwrap_or_default();
        if let Ok(h) = parse(&p, payload) {
            assert_eq!(announced.ok(), Some(payload.len()));
            if let Some(src) = h.source {
                let again = parse_all(&encode(src, src, h.ssl));
                assert_eq!(again.map(|x| (x.source, x.ssl)), Ok((Some(src), h.ssl)));
            }
        }
    }
}

fn parse_all(h: &[u8]) -> std::result::Result<Header, ProxyError> {
    let mut p = [0u8; PREFIX_LEN];
    p.copy_from_slice(h.get(..PREFIX_LEN).ok_or(ProxyError::Truncated)?);
    parse(&p, h.get(PREFIX_LEN..).ok_or(ProxyError::Truncated)?)
}
