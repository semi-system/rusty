//! `Service` and the runner. A service is a long-running task built from a `Context`; the runner
//! starts them together, stops all of them when one fails or panics, and reports the first
//! failure. The all-in-one `rustyd` is this runner over every service; a multi-daemon deployment
//! is the same services under another composition root.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::{Error, Result};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::accept::Accept;
use crate::context::Context;
use crate::listener::{BoundListener, SessionHandler};

#[async_trait]
pub trait Service: Send {
    fn name(&self) -> &'static str;
    /// Run until `shutdown` is cancelled (then return `Ok`) or the service fails.
    async fn run(self: Box<Self>, ctx: Context, shutdown: CancellationToken) -> Result<()>;
}

/// Run every service to completion. Cancelling `shutdown` stops them all; a service that returns
/// an error or panics cancels `shutdown` itself so its siblings stop too. The first failure is
/// returned after every service has finished.
pub async fn run_services(
    services: Vec<Box<dyn Service>>,
    ctx: Context,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut set: JoinSet<(&'static str, Result<()>)> = JoinSet::new();
    for svc in services {
        let (ctx, token) = (ctx.clone(), shutdown.clone());
        let name = svc.name();
        set.spawn(async move { (name, svc.run(ctx, token).await) });
    }
    let mut first: Option<Error> = None;
    while let Some(done) = set.join_next().await {
        let failure = match done {
            Ok((_, Ok(()))) => None,
            Ok((name, Err(e))) => {
                tracing::error!(service = name, error = %e, "service failed");
                Some(e)
            }
            Err(e) if e.is_panic() => {
                tracing::error!("a service panicked");
                Some(Error::storage("a service panicked"))
            }
            Err(_) => None,
        };
        if let Some(e) = failure {
            shutdown.cancel();
            first.get_or_insert(e);
        }
    }
    first.map_or(Ok(()), Err)
}

/// A bound listener with the accepter and session handler it serves.
pub struct ListenerService {
    listener: BoundListener,
    accept: Arc<dyn Accept>,
    handler: Arc<dyn SessionHandler>,
}

impl ListenerService {
    pub fn new(listener: BoundListener, accept: Arc<dyn Accept>, handler: Arc<dyn SessionHandler>) -> ListenerService {
        ListenerService { listener, accept, handler }
    }
}

#[async_trait]
impl Service for ListenerService {
    fn name(&self) -> &'static str {
        self.listener.name()
    }

    async fn run(self: Box<Self>, ctx: Context, shutdown: CancellationToken) -> Result<()> {
        let grace = Duration::from_secs(ctx.cfg.limits.shutdown_grace_secs);
        let ListenerService { listener, accept, handler } = *self;
        listener.serve(accept, handler, ctx.events.clone(), shutdown, grace).await
    }
}
