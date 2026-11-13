//! Connecting to a remote server, behind a trait so the worker can be tested against loopback test
//! doubles and so a deployment can bind a dedicated source address. Production connects to the address
//! the resolver returned on the given port (25 for MX delivery); `bind` is the optional local address
//! (`[outbound] bind_address`).

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use rusty_svc::BoxedStream;
use tokio::net::{TcpSocket, TcpStream};

#[async_trait]
pub trait Dialer: Send + Sync {
    async fn dial(&self, addr: IpAddr, port: u16, bind: Option<IpAddr>, limit: Duration) -> io::Result<BoxedStream>;
}

pub struct TcpDialer;

#[async_trait]
impl Dialer for TcpDialer {
    async fn dial(&self, addr: IpAddr, port: u16, bind: Option<IpAddr>, limit: Duration) -> io::Result<BoxedStream> {
        let remote = SocketAddr::new(addr, port);
        let socket = if remote.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
        if let Some(local) = bind {
            // A bind address of the wrong family is a configuration slip: say so, do not silently skip it.
            socket.bind(SocketAddr::new(local, 0))?;
        }
        let stream: TcpStream = tokio::time::timeout(limit, socket.connect(remote))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timed out"))??;
        let _ = stream.set_nodelay(true);
        Ok(Box::new(stream))
    }
}

/// Addresses mail must never be delivered to: not a real mail exchanger. Private ranges are allowed
/// (a mail system on a private network is legitimate); what is refused is what can only be this host,
/// its link, or a broadcast/multicast address.
pub fn forbidden_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => a.is_loopback() || a.is_unspecified() || a.is_multicast() || a.is_broadcast() || a.is_link_local(),
        IpAddr::V6(a) => {
            a.is_loopback()
                || a.is_unspecified()
                || a.is_multicast()
                || (a.segments().first().is_some_and(|s| s & 0xffc0 == 0xfe80))
                || a.to_ipv4_mapped().is_some_and(|v| forbidden_address(IpAddr::V4(v)))
        }
    }
}
