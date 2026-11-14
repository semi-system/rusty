//! The process lifecycle every Rusty daemon shares, taken out of the all-in-one `rusty` binary so that a
//! service that is its own binary behaves identically:
//!
//! * [`Signals::claim`] is called **before anything starts**: a SIGTERM that arrives early is then not lost
//!   to the default action (which would kill the process without closing the store or the listeners).
//! * [`Signals::spawn`] turns SIGTERM and SIGINT into a cancelled token (every service drains and stops),
//!   and SIGHUP into a reload (the TLS certificate); a reload that fails keeps the previous certificate and
//!   the process running. Without a reload function SIGHUP is claimed and ignored, never fatal.
//! * [`exit_code`] turns the end of the services into the exit status (0 clean, 1 a runtime failure) and
//!   logs it.

use rusty_core::Result;
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// What SIGHUP does: reload the TLS certificate and key. An error leaves the previous ones in force.
pub type Reload = Box<dyn Fn() -> Result<()> + Send + Sync>;

/// The three signals of a daemon, claimed.
pub struct Signals {
    term: Signal,
    int: Signal,
    hup: Signal,
}

impl Signals {
    /// Claim SIGTERM, SIGINT and SIGHUP. Do this first in `main`, before configuration and start-up.
    pub fn claim() -> std::io::Result<Signals> {
        Ok(Signals { term: signal(SignalKind::terminate())?, int: signal(SignalKind::interrupt())?, hup: signal(SignalKind::hangup())? })
    }

    /// Serve the signals in a task until `stop` is cancelled (by a signal or by anyone else).
    pub fn spawn(self, stop: CancellationToken, reload: Option<Reload>) -> JoinHandle<()> {
        let Signals { mut term, mut int, mut hup } = self;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = term.recv() => { tracing::info!("SIGTERM: shutting down"); stop.cancel(); return; }
                    _ = int.recv() => { tracing::info!("SIGINT: shutting down"); stop.cancel(); return; }
                    _ = hup.recv() => match &reload {
                        Some(r) => match r() {
                            Ok(()) => tracing::info!("SIGHUP: TLS certificate reloaded"),
                            Err(e) => tracing::error!(error = %e, "SIGHUP: TLS reload failed; keeping the previous certificate"),
                        },
                        None => tracing::info!("SIGHUP: nothing to reload"),
                    },
                    _ = stop.cancelled() => return,
                }
            }
        })
    }
}

/// The exit status for how the services ended: 0 for a clean stop, 1 for an error (logged).
pub fn exit_code(ended: Result<()>) -> u8 {
    match ended {
        Ok(()) => {
            tracing::info!("stopped");
            0
        }
        Err(e) => {
            tracing::error!(error = %e, "stopped with an error");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_core::Error;

    #[test]
    fn a_clean_end_is_zero_and_an_error_is_one() {
        assert_eq!(exit_code(Ok(())), 0);
        assert_eq!(exit_code(Err(Error::storage("x"))), 1);
    }
}
