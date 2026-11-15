//! The connector for the endpoint of a **tenant's** store (docs/design-s3-per-tenant.md, section 6): it resolves the name itself, refuses every address that is not a public one, and
//! connects to the addresses it just checked, so a name cannot answer "public" to the check and "inside" to the connection (no second lookup, no rebinding window). It runs for every
//! connection, not only when the store is entered.

use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::relay_policy::is_public_address;
use rusty_core::storage_policy::TENANT_S3_PORTS;
use tokio::net::TcpStream;

use crate::error::S3Fail;
use crate::http::Connector;

pub struct PolicyConnector {
    pub timeout: Duration,
}

/// Every resolved address must be public: one that is not makes the whole name refused (a name that mixes public and internal answers is not one to talk to).
pub(crate) fn check_addresses(addrs: &[SocketAddr]) -> Result<(), S3Fail> {
    if addrs.is_empty() || addrs.iter().any(|a| !is_public_address(a.ip())) {
        Err(S3Fail::Resolve)
    } else {
        Ok(())
    }
}

#[async_trait]
impl Connector for PolicyConnector {
    async fn connect(&self, host: &str, port: u16) -> Result<TcpStream, S3Fail> {
        if !TENANT_S3_PORTS.contains(&port) {
            return Err(S3Fail::Resolve);
        }
        let addrs: Vec<SocketAddr> = match tokio::time::timeout(self.timeout, tokio::net::lookup_host((host, port))).await {
            Err(_) => return Err(S3Fail::Timeout),
            Ok(Err(_)) => return Err(S3Fail::Resolve),
            Ok(Ok(a)) => a.collect(),
        };
        check_addresses(&addrs)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn one_internal_address_among_public_ones_refuses_the_name() {
        assert!(check_addresses(&[a("8.8.8.8:443"), a("[2606:4700:4700::1111]:443")]).is_ok());
        for bad in [
            vec![],
            vec![a("127.0.0.1:443")],
            vec![a("10.1.2.3:443")],
            vec![a("169.254.169.254:443")],
            vec![a("192.168.1.1:443")],
            vec![a("100.64.0.1:443")],
            vec![a("[::1]:443")],
            vec![a("[fd00::1]:443")],
            vec![a("[::ffff:127.0.0.1]:443")],
            vec![a("8.8.8.8:443"), a("10.0.0.1:443")],
        ] {
            assert_eq!(check_addresses(&bad), Err(S3Fail::Resolve), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn a_connection_to_an_inside_address_or_a_port_off_the_list_is_never_made() {
        // A server on loopback that would accept: the connector must not reach it.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let accepted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = accepted.clone();
        tokio::spawn(async move {
            if l.accept().await.is_ok() {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let c = PolicyConnector { timeout: Duration::from_secs(2) };
        // The port is not on the list.
        assert_eq!(c.connect("127.0.0.1", port).await.unwrap_err(), S3Fail::Resolve);
        // A listed port, but the address is loopback (an IP literal and a name that resolves to it).
        assert_eq!(c.connect("127.0.0.1", 443).await.unwrap_err(), S3Fail::Resolve);
        assert_eq!(c.connect("localhost", 9000).await.unwrap_err(), S3Fail::Resolve);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!accepted.load(std::sync::atomic::Ordering::SeqCst), "nothing connected to the loopback server");
    }
}
