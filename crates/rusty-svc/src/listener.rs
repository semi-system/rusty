//! The listener loop: accept, enforce the connection limits, run one task per session, isolate
//! panics, and on shutdown stop accepting, let sessions drain until a deadline, then cut the rest.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::{Error, Result};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::accept::{Accept, BoxedStream, PeerInfo};
use crate::events::{RejectReason, ServiceEvent, ServiceEvents};

/// One session. The handler must end promptly once `shutdown` is cancelled (it may say goodbye
/// first); a session still running at the drain deadline is aborted.
#[async_trait]
pub trait SessionHandler: Send + Sync {
    async fn handle(&self, stream: BoxedStream, peer: PeerInfo, shutdown: CancellationToken);
}

#[derive(Debug, Clone)]
pub struct ListenerSpec {
    pub name: &'static str,
    pub addr: SocketAddr,
    pub max_connections: u32,
    pub max_per_ip: u32,
}

/// A listener that has bound its socket (so the caller can learn the address before serving).
pub struct BoundListener {
    spec: ListenerSpec,
    listener: TcpListener,
}

type IpCounts = Arc<Mutex<HashMap<IpAddr, u32>>>;

struct IpGuard {
    map: IpCounts,
    ip: IpAddr,
}

impl IpGuard {
    /// Count one more connection from `ip`, or `None` when it already has `max`.
    fn take(map: &IpCounts, ip: IpAddr, max: u32) -> Option<IpGuard> {
        let mut m = map.lock().unwrap_or_else(|e| e.into_inner());
        let n = m.get(&ip).copied().unwrap_or(0);
        if n >= max {
            return None;
        }
        m.insert(ip, n + 1);
        Some(IpGuard { map: map.clone(), ip })
    }
}

impl Drop for IpGuard {
    fn drop(&mut self) {
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = m.get_mut(&self.ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
    }
}

impl BoundListener {
    pub async fn bind(spec: ListenerSpec) -> Result<BoundListener> {
        let listener = TcpListener::bind(spec.addr)
            .await
            .map_err(|e| Error::storage_with("cannot bind listener", e))?;
        Ok(BoundListener { spec, listener })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.listener.local_addr().unwrap_or(self.spec.addr)
    }

    pub fn name(&self) -> &'static str {
        self.spec.name
    }

    /// Serve until `shutdown` is cancelled, then drain for at most `grace`.
    pub async fn serve(
        self,
        accept: Arc<dyn Accept>,
        handler: Arc<dyn SessionHandler>,
        events: Arc<dyn ServiceEvents>,
        shutdown: CancellationToken,
        grace: Duration,
    ) -> Result<()> {
        let BoundListener { spec, listener } = self;
        let name = spec.name;
        let total = Arc::new(Semaphore::new(spec.max_connections as usize));
        let per_ip: IpCounts = Arc::default();
        let mut tasks: JoinSet<()> = JoinSet::new();

        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                Some(done) = tasks.join_next(), if !tasks.is_empty() => reap(done, name, &*events),
                res = listener.accept() => {
                    let (tcp, addr) = match res {
                        Ok(x) => x,
                        Err(e) => {
                            tracing::warn!(service = name, error = %e, "accept failed");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                    };
                    let Ok(permit) = total.clone().try_acquire_owned() else {
                        events.emit(ServiceEvent::Rejected { service: name, peer: addr, reason: RejectReason::TotalLimit });
                        continue;
                    };
                    let Some(guard) = IpGuard::take(&per_ip, addr.ip(), spec.max_per_ip) else {
                        events.emit(ServiceEvent::Rejected { service: name, peer: addr, reason: RejectReason::PerIpLimit });
                        continue;
                    };
                    let (accept, handler, events, token) =
                        (accept.clone(), handler.clone(), events.clone(), shutdown.clone());
                    tasks.spawn(async move {
                        let _permit = permit;
                        let _guard = guard;
                        match accept.accept(tcp, addr).await {
                            Ok((stream, peer)) => {
                                events.emit(ServiceEvent::Accepted { service: name, peer: peer.addr });
                                handler.handle(stream, peer, token).await;
                                events.emit(ServiceEvent::Closed { service: name, peer: peer.addr });
                            }
                            Err(_) => events.emit(ServiceEvent::Rejected {
                                service: name,
                                peer: addr,
                                reason: RejectReason::HandshakeFailed,
                            }),
                        }
                    });
                }
            }
        }

        // Stop accepting first: new connections are refused from here on.
        drop(listener);
        let deadline = tokio::time::sleep(grace);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                done = tasks.join_next() => match done {
                    Some(d) => reap(d, name, &*events),
                    None => break,
                },
                _ = &mut deadline => {
                    tracing::warn!(service = name, open = tasks.len(), "drain deadline reached, cutting sessions");
                    tasks.abort_all();
                    while let Some(d) = tasks.join_next().await {
                        reap(d, name, &*events);
                    }
                    break;
                }
            }
        }
        Ok(())
    }
}

fn reap(done: std::result::Result<(), tokio::task::JoinError>, name: &'static str, events: &dyn ServiceEvents) {
    if let Err(e) = done {
        if e.is_panic() {
            events.emit(ServiceEvent::SessionPanicked { service: name });
        }
    }
}
