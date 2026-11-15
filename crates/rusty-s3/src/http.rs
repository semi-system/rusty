//! A small HTTP/1.1 client, enough for S3 and no more. One connection per request (`Connection: close`), TCP or TLS, and hard limits everywhere:
//!
//! * no redirect is followed (a 3xx is a protocol error: it would carry the signature to another host), no proxy, no cookies, no compression;
//! * the status line and headers are at most [`MAX_HEAD`] bytes, the body at most the limit the caller names;
//! * the whole request, connection included, has a deadline.
//!
//! The way a TCP connection is made is a seam ([`Connector`]): the address policy for tenant-chosen endpoints (public addresses only, checked at every connection) is applied there.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::error::S3Fail;

/// The longest status line plus headers accepted.
pub const MAX_HEAD: usize = 32 * 1024;

/// Makes the TCP connection to `host:port`. `Resolve` is the failure to use for a name that does not resolve or an address that the policy refuses.
#[async_trait]
pub trait Connector: Send + Sync {
    async fn connect(&self, host: &str, port: u16) -> Result<TcpStream, S3Fail>;
}

/// Resolves with the system resolver and connects, with a timeout. No address policy: for the platform's own store, whose endpoint the operator wrote (it may be an internal address).
pub struct DirectConnector {
    pub timeout: Duration,
}

#[async_trait]
impl Connector for DirectConnector {
    async fn connect(&self, host: &str, port: u16) -> Result<TcpStream, S3Fail> {
        let addrs: Vec<SocketAddr> = match tokio::time::timeout(self.timeout, tokio::net::lookup_host((host, port))).await {
            Err(_) => return Err(S3Fail::Timeout),
            Ok(Err(_)) => return Err(S3Fail::Resolve),
            Ok(Ok(a)) => a.collect(),
        };
        if addrs.is_empty() {
            return Err(S3Fail::Resolve);
        }
        let mut last = S3Fail::Connect;
        for a in addrs {
            match tokio::time::timeout(self.timeout, TcpStream::connect(a)).await {
                Ok(Ok(s)) => return Ok(s),
                Ok(Err(_)) => last = S3Fail::Connect,
                Err(_) => last = S3Fail::Timeout,
            }
        }
        Err(last)
    }
}

/// Where requests go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub tls: bool,
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    /// `https://host[:port]` or `http://host[:port]` and nothing else (no path, no credentials, no query).
    pub fn parse(s: &str) -> Option<Endpoint> {
        let (tls, rest) = match s.strip_prefix("https://") {
            Some(r) => (true, r),
            None => (false, s.strip_prefix("http://")?),
        };
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.is_empty() || rest.contains(['/', '?', '#', '@', ' ']) || rest.starts_with('[') {
            return None;
        }
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().ok().filter(|p| *p != 0)?),
            None => (rest, if tls { 443 } else { 80 }),
        };
        let ok_host = !host.is_empty() && host.len() <= 253 && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
        ok_host.then(|| Endpoint { tls, host: host.to_ascii_lowercase(), port })
    }

    /// The `Host` header value: the port only when it is not the scheme's own.
    pub fn host_header(&self, host: &str) -> String {
        let default = if self.tls { 443 } else { 80 };
        if self.port == default {
            host.to_owned()
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

/// A reply.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    /// Lower-case names.
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub struct Http {
    pub endpoint: Endpoint,
    connector: Arc<dyn Connector>,
    tls: Option<TlsConnector>,
    /// The whole request, from the first connection attempt to the last byte.
    pub deadline: Duration,
}

impl Http {
    pub fn new(endpoint: Endpoint, connector: Arc<dyn Connector>, tls: Option<Arc<rustls::ClientConfig>>, deadline: Duration) -> Http {
        Http { endpoint, connector, tls: tls.map(TlsConnector::from), deadline }
    }

    /// One request. `target` is the path and query as sent; `host` the name that goes in the `Host` header and the TLS server name (it may be `bucket.host` for a virtual-hosted
    /// request); `headers` are sent as they are (the caller signed them). `max_body` bounds the reply body.
    pub async fn send(&self, method: &str, target: &str, host: &str, headers: &[(String, String)], body: &[u8], max_body: usize) -> Result<Reply, S3Fail> {
        match tokio::time::timeout(self.deadline, self.send_inner(method, target, host, headers, body, max_body)).await {
            Ok(r) => r,
            Err(_) => Err(S3Fail::Timeout),
        }
    }

    async fn send_inner(&self, method: &str, target: &str, host: &str, headers: &[(String, String)], body: &[u8], max_body: usize) -> Result<Reply, S3Fail> {
        let tcp = self.connector.connect(host, self.endpoint.port).await?;
        let mut io: Box<dyn Io> = if self.endpoint.tls {
            let connector = self.tls.as_ref().ok_or(S3Fail::Tls)?;
            let name = rustls_pki_types::ServerName::try_from(host.to_owned()).map_err(|_| S3Fail::Tls)?;
            Box::new(connector.connect(name, tcp).await.map_err(|_| S3Fail::Tls)?)
        } else {
            Box::new(tcp)
        };
        let mut head = format!("{method} {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: rusty\r\nContent-Length: {}\r\n", self.endpoint.host_header(host), body.len());
        for (n, v) in headers {
            // Header names and values come from this crate, never from a tenant; this is the second line against a smuggled line.
            if n.contains(['\r', '\n', ':']) || v.contains(['\r', '\n']) {
                return Err(S3Fail::Protocol);
            }
            head.push_str(&format!("{n}: {v}\r\n"));
        }
        head.push_str("\r\n");
        io.write_all(head.as_bytes()).await.map_err(|_| S3Fail::Connect)?;
        io.write_all(body).await.map_err(|_| S3Fail::Connect)?;
        io.flush().await.map_err(|_| S3Fail::Connect)?;
        read_reply(BufReader::new(io), method == "HEAD", max_body).await
    }
}

async fn read_line<R: AsyncBufReadExt + Unpin>(r: &mut R, budget: &mut usize) -> Result<String, S3Fail> {
    let mut line = Vec::new();
    loop {
        let mut b = [0u8; 1];
        let n = r.read(&mut b).await.map_err(|_| S3Fail::Connect)?;
        if n == 0 {
            return Err(S3Fail::Protocol);
        }
        if *budget == 0 {
            return Err(S3Fail::Protocol);
        }
        *budget -= 1;
        if b[0] == b'\n' {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return String::from_utf8(line).map_err(|_| S3Fail::Protocol);
        }
        line.push(b[0]);
    }
}

/// Parse a reply from `r`. Exposed to the tests of this module and the crate.
pub(crate) async fn read_reply<R: AsyncBufReadExt + Unpin>(mut r: R, head_only: bool, max_body: usize) -> Result<Reply, S3Fail> {
    let mut budget = MAX_HEAD;
    let status_line = read_line(&mut r, &mut budget).await?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(S3Fail::Protocol);
    }
    let status: u16 = parts.next().and_then(|s| s.parse().ok()).filter(|s| (100..600).contains(s)).ok_or(S3Fail::Protocol)?;
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let line = read_line(&mut r, &mut budget).await?;
        if line.is_empty() {
            break;
        }
        let (n, v) = line.split_once(':').ok_or(S3Fail::Protocol)?;
        headers.push((n.trim().to_ascii_lowercase(), v.trim().to_owned()));
    }
    // An interim reply (100 Continue) is skipped: the real one follows.
    if status == 100 {
        return Box::pin(read_reply(r, head_only, max_body)).await;
    }
    if (300..400).contains(&status) {
        return Err(S3Fail::Protocol);
    }
    let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
    let chunked = header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
    let length: Option<usize> = match header("content-length") {
        Some(v) => Some(v.parse().map_err(|_| S3Fail::Protocol)?),
        None => None,
    };
    let body = if head_only || status == 204 || status == 304 {
        Bytes::new()
    } else if chunked {
        read_chunked(&mut r, max_body).await?
    } else if let Some(n) = length {
        if n > max_body {
            return Err(S3Fail::TooLarge);
        }
        let mut buf = vec![0u8; n];
        r.read_exact(&mut buf).await.map_err(|_| S3Fail::Connect)?;
        Bytes::from(buf)
    } else {
        // No length and not chunked: the body runs to the end of the connection.
        let mut buf = BytesMut::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = r.read(&mut chunk).await.map_err(|_| S3Fail::Connect)?;
            if n == 0 {
                break;
            }
            if buf.len() + n > max_body {
                return Err(S3Fail::TooLarge);
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        buf.freeze()
    };
    Ok(Reply { status, headers, body })
}

async fn read_chunked<R: AsyncBufReadExt + Unpin>(r: &mut R, max_body: usize) -> Result<Bytes, S3Fail> {
    let mut out = BytesMut::new();
    let mut budget = MAX_HEAD;
    loop {
        let line = read_line(r, &mut budget).await?;
        let size_text = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| S3Fail::Protocol)?;
        if size == 0 {
            // Trailers, up to the blank line.
            loop {
                if read_line(r, &mut budget).await?.is_empty() {
                    return Ok(out.freeze());
                }
            }
        }
        if out.len().saturating_add(size) > max_body {
            return Err(S3Fail::TooLarge);
        }
        let start = out.len();
        out.resize(start + size, 0);
        r.read_exact(&mut out[start..]).await.map_err(|_| S3Fail::Connect)?;
        let mut crlf = [0u8; 2];
        r.read_exact(&mut crlf).await.map_err(|_| S3Fail::Connect)?;
        if &crlf != b"\r\n" {
            return Err(S3Fail::Protocol);
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(raw: &[u8], head_only: bool, max: usize) -> Result<Reply, S3Fail> {
        read_reply(BufReader::new(raw), head_only, max).await
    }

    #[tokio::test]
    async fn replies_with_a_length_chunks_or_a_close_are_read() {
        let r = parse(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nETag: \"x\"\r\n\r\nhello", false, 100).await.unwrap();
        assert_eq!((r.status, &r.body[..], r.header("etag")), (200, &b"hello"[..], Some("\"x\"")));
        let r = parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;ext=1\r\nde\r\n0\r\nTrailer: x\r\n\r\n", false, 100).await.unwrap();
        assert_eq!(&r.body[..], b"abcde");
        let r = parse(b"HTTP/1.0 200 OK\r\n\r\nuntil the end", false, 100).await.unwrap();
        assert_eq!(&r.body[..], b"until the end");
        let r = parse(b"HTTP/1.1 200 OK\r\nContent-Length: 42\r\nLast-Modified: x\r\n\r\n", true, 100).await.unwrap();
        assert!(r.body.is_empty(), "a HEAD reply has no body whatever its length says");
        let r = parse(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n", false, 100).await.unwrap();
        assert_eq!(r.status, 204);
    }

    #[tokio::test]
    async fn anything_malformed_oversized_or_redirecting_is_refused() {
        assert_eq!(parse(b"HTTP/1.1 200 OK\r\nContent-Length: 101\r\n\r\n", false, 100).await.unwrap_err(), S3Fail::TooLarge);
        assert_eq!(parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n65\r\n", false, 100).await.unwrap_err(), S3Fail::TooLarge, "0x65 = 101");
        assert_eq!(parse(b"HTTP/1.1 200 OK\r\nContent-Length: abc\r\n\r\n", false, 100).await.unwrap_err(), S3Fail::Protocol);
        assert_eq!(parse(b"SSH-2.0-OpenSSH\r\n", false, 100).await.unwrap_err(), S3Fail::Protocol);
        assert_eq!(parse(b"HTTP/1.1 999 X\r\n\r\n", false, 100).await.unwrap_err(), S3Fail::Protocol);
        assert_eq!(parse(b"HTTP/1.1 200 OK\r\nno colon here\r\n\r\n", false, 100).await.unwrap_err(), S3Fail::Protocol);
        for redirect in [301, 302, 307, 308] {
            let raw = format!("HTTP/1.1 {redirect} Moved\r\nLocation: http://elsewhere/\r\n\r\n");
            assert_eq!(parse(raw.as_bytes(), false, 100).await.unwrap_err(), S3Fail::Protocol, "{redirect}");
        }
        // Headers that never end are cut at the limit.
        let mut huge = b"HTTP/1.1 200 OK\r\n".to_vec();
        huge.extend(b"X-A: b\r\n".repeat(MAX_HEAD / 8 + 10));
        assert_eq!(parse(&huge, false, 100).await.unwrap_err(), S3Fail::Protocol);
        // A body cut short is a connection fault, not a short object.
        assert_eq!(parse(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc", false, 100).await.unwrap_err(), S3Fail::Connect);
    }

    #[test]
    fn an_endpoint_is_a_scheme_a_host_and_maybe_a_port_and_nothing_else() {
        let e = Endpoint::parse("https://S3.Example.com").unwrap();
        assert_eq!((e.tls, e.host.as_str(), e.port), (true, "s3.example.com", 443));
        assert_eq!(Endpoint::parse("http://127.0.0.1:9000/").unwrap().port, 9000);
        assert_eq!(e.host_header("b.s3.example.com"), "b.s3.example.com");
        assert_eq!(Endpoint::parse("http://h:9000").unwrap().host_header("h"), "h:9000");
        for bad in ["", "ftp://h", "https://", "https://h/path", "https://u:p@h", "https://h?x=1", "https://h:0", "https://h:99999", "https://[::1]:9000", "https://h h", "h"] {
            assert!(Endpoint::parse(bad).is_none(), "{bad}");
        }
    }
}
