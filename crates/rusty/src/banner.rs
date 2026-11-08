//! The skeleton session of U1: until the SMTP and IMAP crates land, every service port greets
//! with a protocol-shaped banner, answers the first line with a "not available yet" status and
//! closes. It exists so the listener, TLS, limits and shutdown paths are real and testable now;
//! U3/U4/U6 replace it with the protocol sessions behind the same `SessionHandler` seam.

use std::time::Duration;

use async_trait::async_trait;
use rusty_svc::{BoxedStream, ListenerKind, PeerInfo, SessionHandler};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const IDLE: Duration = Duration::from_secs(10);

pub struct BannerSession {
    kind: ListenerKind,
    hostname: String,
}

impl BannerSession {
    pub fn new(kind: ListenerKind, hostname: &str) -> BannerSession {
        BannerSession { kind, hostname: hostname.to_owned() }
    }

    fn greeting(&self) -> String {
        if self.kind.is_imap() {
            "* OK [CAPABILITY IMAP4rev1] rusty (skeleton: no mailbox access yet)\r\n".to_owned()
        } else {
            format!("220 {} ESMTP rusty (skeleton: no mail accepted yet)\r\n", self.hostname)
        }
    }

    fn unavailable(&self) -> String {
        if self.kind.is_imap() {
            "* BYE service not available yet\r\n".to_owned()
        } else {
            format!("421 4.3.0 {} service not available yet\r\n", self.hostname)
        }
    }

    fn going_away(&self) -> String {
        if self.kind.is_imap() {
            "* BYE server shutting down\r\n".to_owned()
        } else {
            format!("421 4.3.2 {} shutting down\r\n", self.hostname)
        }
    }
}

#[async_trait]
impl SessionHandler for BannerSession {
    async fn handle(&self, mut stream: BoxedStream, _peer: PeerInfo, shutdown: CancellationToken) {
        if stream.write_all(self.greeting().as_bytes()).await.is_err() {
            return;
        }
        let mut buf = [0u8; 1024];
        let reply = tokio::select! {
            _ = shutdown.cancelled() => self.going_away(),
            r = tokio::time::timeout(IDLE, stream.read(&mut buf)) => match r {
                Ok(Ok(0)) | Ok(Err(_)) => return, // the client left
                _ => self.unavailable(),
            },
        };
        let _ = stream.write_all(reply.as_bytes()).await;
        let _ = stream.shutdown().await;
    }
}
