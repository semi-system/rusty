//! The `Accept` seam: how an accepted TCP connection becomes a session stream plus the facts the
//! session layer may know about the peer. TLS is an I/O-driver concern: sessions see only
//! [`PeerInfo::tls`]. An edge accepter (PROXY protocol, HAProxy TLS termination) is another
//! implementation that fills the same [`PeerInfo`].

use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::{Error, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

use crate::tls::TlsProvider;

/// Any byte stream a session can run over (plain TCP, TLS, a test pipe).
pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

pub type BoxedStream = Box<dyn AsyncStream>;

/// What a session may know about its peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerInfo {
    /// The peer's address (for an edge accepter: the original client, not the proxy).
    pub addr: SocketAddr,
    /// The connection is already protected by TLS (implicit-TLS port or edge termination).
    pub tls: bool,
}

#[async_trait]
pub trait Accept: Send + Sync {
    /// Turn an accepted socket into a stream. Runs inside the session task, so a slow peer
    /// delays only itself. An error drops the connection.
    async fn accept(&self, tcp: TcpStream, addr: SocketAddr) -> Result<(BoxedStream, PeerInfo)>;
}

/// Plain TCP; STARTTLS (if the protocol has it) is the session's business.
#[derive(Debug, Default, Clone, Copy)]
pub struct DirectTcp;

#[async_trait]
impl Accept for DirectTcp {
    async fn accept(&self, tcp: TcpStream, addr: SocketAddr) -> Result<(BoxedStream, PeerInfo)> {
        Ok((Box::new(tcp), PeerInfo { addr, tls: false }))
    }
}

/// How long an implicit-TLS client may take to finish the handshake.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Implicit TLS (ports 465 and 993): the handshake completes before the session starts.
#[derive(Clone)]
pub struct DirectTls {
    provider: TlsProvider,
    timeout: Duration,
}

impl DirectTls {
    pub fn new(provider: TlsProvider) -> DirectTls {
        DirectTls { provider, timeout: DEFAULT_HANDSHAKE_TIMEOUT }
    }

    pub fn with_handshake_timeout(mut self, timeout: Duration) -> DirectTls {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl Accept for DirectTls {
    async fn accept(&self, tcp: TcpStream, addr: SocketAddr) -> Result<(BoxedStream, PeerInfo)> {
        let hs = self.provider.acceptor().accept(tcp);
        match tokio::time::timeout(self.timeout, hs).await {
            Ok(Ok(stream)) => Ok((Box::new(stream), PeerInfo { addr, tls: true })),
            Ok(Err(_)) => Err(Error::Invalid("TLS handshake failed".into())),
            Err(_) => Err(Error::Invalid("TLS handshake timed out".into())),
        }
    }
}
