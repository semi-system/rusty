//! `rusty-admin healthcheck`: is the service answering? It asks its own listener for the session state (`GET /api/v1/session`); `401` (nobody is signed
//! in) and `200` both mean the service is up and its store answers. No credential, no cookie, nothing is written. The image has no shell and no curl, so
//! the container's HEALTHCHECK runs this command.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A wildcard listen address is probed on loopback.
fn target(listen: SocketAddr) -> SocketAddr {
    if listen.ip().is_unspecified() {
        let ip = if listen.is_ipv4() { std::net::IpAddr::from([127, 0, 0, 1]) } else { std::net::IpAddr::from(std::net::Ipv6Addr::LOCALHOST) };
        SocketAddr::new(ip, listen.port())
    } else {
        listen
    }
}

/// The PROXY protocol v2 header of a `LOCAL` command (a proxy's own health check): the signature, version 2 and `LOCAL`, no address family, no payload.
const PROXY_V2_LOCAL: [u8; 16] = [0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, 0x20, 0x00, 0x00, 0x00];

/// The status code of the answer to `GET /api/v1/session` at `listen`, if there was one within `limit`. `proxied`: the service reads the PROXY header (`[admin] proxy_protocol`), so the probe
/// starts with the `LOCAL` one, as the edge's own health checks do.
pub async fn status(listen: SocketAddr, limit: Duration, proxied: bool) -> Option<u16> {
    tokio::time::timeout(limit, async {
        let mut s = TcpStream::connect(target(listen)).await.ok()?;
        if proxied {
            s.write_all(&PROXY_V2_LOCAL).await.ok()?;
        }
        s.write_all(b"GET /api/v1/session HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nAccept: application/json\r\n\r\n").await.ok()?;
        let mut head = Vec::with_capacity(64);
        let mut buf = [0u8; 64];
        while head.len() < 32 {
            let n = s.read(&mut buf).await.ok()?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(&buf[..n]);
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

/// Healthy: the service answered `200` or `401`.
pub async fn healthy(listen: SocketAddr, limit: Duration, proxied: bool) -> bool {
    matches!(status(listen, limit, proxied).await, Some(200 | 401))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn serve_once(reply: &'static [u8]) -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut b = [0u8; 512];
                let _ = s.read(&mut b).await;
                let _ = s.write_all(reply).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn only_a_200_or_401_from_the_service_is_healthy() {
        let t = Duration::from_millis(500);
        assert!(healthy(serve_once(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n").await, t, false).await);
        assert!(healthy(serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await, t, false).await);
        assert!(!healthy(serve_once(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n").await, t, false).await);
        assert!(!healthy(serve_once(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await, t, false).await);
        assert!(!healthy(serve_once(b"SSH-2.0-OpenSSH\r\n").await, t, false).await, "not HTTP");
        assert!(!healthy(serve_once(b"").await, t, false).await, "closed without an answer");
    }

    #[tokio::test]
    async fn nothing_listening_and_a_server_that_never_answers_are_unhealthy() {
        let free = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = free.local_addr().unwrap();
        drop(free);
        assert!(!healthy(addr, Duration::from_millis(500), false).await);
        let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = silent.local_addr().unwrap();
        tokio::spawn(async move {
            let _held = silent.accept().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        assert!(!healthy(addr, Duration::from_millis(300), false).await);
    }

    #[test]
    fn a_wildcard_listen_address_is_probed_on_loopback() {
        assert_eq!(target("0.0.0.0:8080".parse().unwrap()).to_string(), "127.0.0.1:8080");
        assert_eq!(target("[::]:8080".parse().unwrap()).to_string(), "[::1]:8080");
        assert_eq!(target("10.1.2.3:8080".parse().unwrap()).to_string(), "10.1.2.3:8080");
    }
}
