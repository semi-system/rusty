//! `rusty-dns`: the authoritative name server for the hosted domains as its own binary (docs/design-authoritative-dns.md).
//!
//! ```text
//! rusty-dns [--config FILE]               (default /etc/rusty/dns.toml)
//! rusty-dns healthcheck [--config FILE]   exit 0 when the running server answers a question, 1 when not (the container HEALTHCHECK)
//! ```
//!
//! Exit status: 0 after a clean stop, 1 for a runtime failure, 2 for a bad command line or configuration. Logs go to standard error.
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use rusty_core::config::{Role, StartupConfig};
use rusty_runtime::process::{exit_code, Signals};
use rusty_runtime::store::store_config;
use rusty_store::Store;
use tokio_util::sync::CancellationToken;

const DEFAULT_CONFIG: &str = "/etc/rusty/dns.toml";

fn usage() -> ExitCode {
    eprintln!("usage: rusty-dns [--config FILE]");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1).peekable();
    let health = args.peek().is_some_and(|a| a == "healthcheck");
    if health {
        args.next();
    }
    let mut config: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        match a.to_str() {
            Some("--config") => match args.next() {
                Some(v) => config = Some(PathBuf::from(v)),
                None => return usage(),
            },
            Some("--help" | "-h") => {
                eprintln!("usage: rusty-dns [--config FILE]");
                return ExitCode::SUCCESS;
            }
            _ => return usage(),
        }
    }
    let path = config.unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG));
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("rusty-dns: cannot start the runtime: {e}");
            return ExitCode::from(1);
        }
    };
    if health {
        return runtime.block_on(async move { ExitCode::from(healthcheck(path).await) });
    }
    runtime.block_on(async move { ExitCode::from(run(path).await) })
}

/// `healthcheck`: read the same configuration, ask the first listener. 0 healthy, 1 not, 2 a configuration that does not load.
async fn healthcheck(path: PathBuf) -> u8 {
    let cfg: StartupConfig = match StartupConfig::load_for(Role::Dns, &path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rusty-dns: {e}");
            return 2;
        }
    };
    let Some(addr) = cfg.dns.as_ref().and_then(|d| d.listen.first().copied()) else { return 2 };
    if rusty_dns::run::healthy(addr, Duration::from_secs(3)).await {
        0
    } else {
        1
    }
}

async fn run(path: PathBuf) -> u8 {
    // Signals first: a SIGTERM that arrives early must not kill the process before the store is closed.
    let signals = match Signals::claim() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rusty-dns: cannot claim the termination signals: {e}");
            return 1;
        }
    };
    let cfg: StartupConfig = match StartupConfig::load_for(Role::Dns, &path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rusty-dns: {e}");
            return 2;
        }
    };
    if let Err(e) = rusty_svc::init_logging(cfg.log_level, cfg.log_format) {
        eprintln!("rusty-dns: {e}");
        return 1;
    }
    let store_cfg = match store_config(&cfg, rusty_core::config::Profile::Production) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rusty-dns: {e}");
            return 2;
        }
    };
    let store = match Store::open(store_cfg).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot open the database");
            return 1;
        }
    };
    let stop = CancellationToken::new();
    let running = match rusty_dns::run::start(&cfg, &store, stop.clone()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "cannot start the name server");
            let _ = store.close().await;
            return 1;
        }
    };
    let signal_task = signals.spawn(stop.clone(), None);
    tracing::info!(udp = ?running.udp, tcp = ?running.tcp, "name server listening");
    let ended = running.task.await.map_err(|_| rusty_core::Error::storage("the name server panicked"));
    stop.cancel();
    let _ = signal_task.await;
    let closed = store.close().await;
    exit_code(ended.and(closed))
}
