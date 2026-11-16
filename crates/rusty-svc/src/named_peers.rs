//! Peers named by DNS: the addresses a set of host names resolve to right now (Docker's DNS: a service name, `tasks.<service>` in a swarm), resolved again every few seconds. A swarm has no fixed
//! addresses, so a service that trusts its edge, or lets the admin web read its metrics, names them instead of an address block.
//!
//! Safe by default: until a name resolves nothing is trusted; a name whose lookup fails keeps what it gave last time (a hiccup must not cut a service off from its edge), and one that answers
//! replaces it. The set is shared and cheap to read on every accepted connection.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// How often the names are resolved again.
pub const REFRESH: Duration = Duration::from_secs(5);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

type IpSet = HashSet<IpAddr>;

/// The addresses of a few names, kept up to date in the background. Clones share the set.
#[derive(Debug, Clone, Default)]
pub struct NamedPeers {
    ips: Arc<RwLock<IpSet>>,
}

impl NamedPeers {
    /// No names: nobody is named.
    pub fn none() -> NamedPeers {
        NamedPeers::default()
    }

    /// Resolve `names` once (so the first connection already finds them), then again every `refresh` until `shutdown`. No names: nothing is started.
    pub async fn start(names: Vec<String>, refresh: Duration, shutdown: CancellationToken) -> NamedPeers {
        let peers = NamedPeers::none();
        if names.is_empty() {
            return peers;
        }
        let mut last = vec![IpSet::new(); names.len()];
        resolve_all(&names, &mut last, &peers.ips).await;
        let ips = peers.ips.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(refresh) => resolve_all(&names, &mut last, &ips).await,
                }
            }
        });
        peers
    }

    /// A set with these addresses and no background task (for tests and for callers that resolve themselves).
    pub fn fixed(ips: impl IntoIterator<Item = IpAddr>) -> NamedPeers {
        let p = NamedPeers::none();
        *p.ips.write().unwrap_or_else(std::sync::PoisonError::into_inner) = ips.into_iter().collect();
        p
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.ips.read().unwrap_or_else(std::sync::PoisonError::into_inner).contains(&ip)
    }

    pub fn is_empty(&self) -> bool {
        self.ips.read().unwrap_or_else(std::sync::PoisonError::into_inner).is_empty()
    }
}

async fn resolve_all(names: &[String], last: &mut [IpSet], into: &RwLock<IpSet>) {
    for (n, slot) in names.iter().zip(last.iter_mut()) {
        if let Ok(Ok(it)) = tokio::time::timeout(LOOKUP_TIMEOUT, tokio::net::lookup_host((n.as_str(), 0))).await {
            *slot = it.map(|a| a.ip()).collect();
        }
    }
    *into.write().unwrap_or_else(std::sync::PoisonError::into_inner) = last.iter().flatten().copied().collect();
}
