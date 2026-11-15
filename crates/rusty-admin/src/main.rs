//! `rusty-admin`: the admin web service as its own binary (admin web design, section 11).
//!
//! ```text
//! rusty-admin [--config FILE]        (default /etc/rusty/admin.toml)
//! rusty-admin healthcheck [--config FILE]   exit 0 when the running service answers, 1 when not (the container HEALTHCHECK)
//! ```
//!
//! Exit status: 0 after a clean stop, 1 for a runtime failure, 2 for a bad command line or configuration. The setup token, when one is made, is the only
//! thing written to standard output (one line); logs go to standard error.
#![forbid(unsafe_code)]

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use rusty_core::config::{Role, StartupConfig};
use rusty_runtime::process::{exit_code, Signals};
use rusty_runtime::store::store_config;
use rusty_store::Store;
use tokio_util::sync::CancellationToken;

const DEFAULT_CONFIG: &str = "/etc/rusty/admin.toml";

fn usage() -> ExitCode {
    eprintln!("usage: rusty-admin [--config FILE]");
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
                eprintln!("usage: rusty-admin [--config FILE]");
                return ExitCode::SUCCESS;
            }
            _ => return usage(),
        }
    }
    let path = config.unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG));
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("rusty-admin: cannot start the runtime: {e}");
            return ExitCode::from(1);
        }
    };
    if health {
        return runtime.block_on(async move { ExitCode::from(healthcheck(path).await) });
    }
    runtime.block_on(async move { ExitCode::from(run(path).await) })
}

/// `healthcheck`: read the same configuration, ask the listener. 0 healthy, 1 not, 2 a configuration that does not load.
async fn healthcheck(path: PathBuf) -> u8 {
    let cfg: StartupConfig = match StartupConfig::load_for(Role::Admin, &path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rusty-admin: {e}");
            return 2;
        }
    };
    let Some(admin) = cfg.admin.as_ref() else { return 2 };
    if rusty_admin::health::healthy(admin.listen, std::time::Duration::from_secs(5)).await {
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
            eprintln!("rusty-admin: cannot claim the termination signals: {e}");
            return 1;
        }
    };
    let cfg: StartupConfig = match StartupConfig::load_for(Role::Admin, &path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rusty-admin: {e}");
            return 2;
        }
    };
    if let Err(e) = rusty_svc::init_logging(cfg.log_level, cfg.log_format) {
        eprintln!("rusty-admin: {e}");
        return 1;
    }
    let store_cfg = match store_config(&cfg, rusty_core::config::Profile::Production) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rusty-admin: {e}");
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
    let running = match rusty_admin::run::start(&cfg, &store, stop.clone()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "cannot start the admin service");
            let _ = store.close().await;
            return 1;
        }
    };
    let reload = running.tls.clone().map(|t| -> rusty_runtime::process::Reload { Box::new(move || t.reload()) });
    let signal_task = signals.spawn(stop.clone(), reload);
    tracing::info!(addr = %running.addr, "admin service listening");
    if let Some(token) = &running.setup_token {
        // The one line on standard output: the token and nothing else. It is not logged.
        tracing::warn!("no administrator exists: the setup token is printed on standard output; open the admin address and enter it");
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{token}");
        let _ = out.flush();
    }
    let ended = match running.task.await {
        Ok(r) => r,
        Err(_) => Err(rusty_core::Error::storage("the admin listener panicked")),
    };
    stop.cancel();
    let _ = signal_task.await;
    let closed = store.close().await;
    exit_code(ended.and(closed))
}
