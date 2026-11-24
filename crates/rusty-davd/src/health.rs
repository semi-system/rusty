//! `rusty-dav healthcheck`: is the service answering? `OPTIONS /dav/` needs no credential and writes nothing; `200` means the service is up. The image has no shell and no curl, so the container's
//! HEALTHCHECK runs this command.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn target(listen: SocketAddr) -> SocketAddr {
    if listen.ip().is_unspecified() {
        let ip = if listen.is_ipv4() { std::net::IpAddr::from([127, 0, 0, 1]) } else { std::net::IpAddr::from(std::net::Ipv6Addr::LOCALHOST) };
        SocketAddr::new(ip, listen.port())
    } else {
        listen
    }
}

/// The PROXY protocol v2 header of a `LOCAL` command (a proxy's own health check).
const PROXY_V2_LOCAL: [u8; 16] = [0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, 0x20, 0x00, 0x00, 0x00];

/// The status code of the answer to `OPTIONS /dav/` at `listen`, if there was one within `limit`.
pub async fn status(listen: SocketAddr, limit: Duration, proxied: bool) -> Option<u16> {
    tokio::time::timeout(limit, async {
        let mut s = TcpStream::connect(target(listen)).await.ok()?;
        if proxied {
            s.write_all(&PROXY_V2_LOCAL).await.ok()?;
        }
        s.write_all(b"OPTIONS /dav/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await.ok()?;
        let mut head = Vec::with_capacity(64);
        let mut buf = [0u8; 64];
        while head.len() < 32 {
            let n = s.read(&mut buf).await.ok()?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(buf.get(..n)?);
        }
        let line = std::str::from_utf8(&head).ok()?.lines().next()?;
        let mut parts = line.split_whitespace();
        (parts.next()?.starts_with("HTTP/1.")).then_some(())?;
        parts.next()?.parse().ok()
    })
    .await
    .ok()
    .flatten()
}

pub async fn healthy(listen: SocketAddr, limit: Duration, proxied: bool) -> bool {
    status(listen, limit, proxied).await == Some(200)
}
