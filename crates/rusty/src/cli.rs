//! Command line: multi-call dispatch (`rusty d`, `rusty ctl`, or argv[0] `rustyd` / `rustyctl`),
//! option parsing, signal handling and exit codes (0 clean, 1 runtime failure, 2 usage).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rusty_core::config::{LogFormat, Profile, SslMode, StartupConfig};
use rusty_svc::init_logging;
use rusty_runtime::process::{exit_code, Signals};
use tokio_util::sync::CancellationToken;

use crate::daemon::{start, with_bind, with_port_offset};
use rusty_runtime::store::DEV_PG_PASSWORD_ENV;

/// The Postgres server of the dev profile (`postgres://user@host:port/database`, no password).
pub(crate) const DEV_PG_URL_ENV: &str = "RUSTY_DEV_PG_URL";
/// Optional: `disable`, `require`, `verify-ca` or `verify-full` (default: `disable` on loopback, `verify-full` elsewhere).
pub(crate) const DEV_PG_SSLMODE_ENV: &str = "RUSTY_DEV_PG_SSLMODE";
/// Optional: the CA bundle (PEM, absolute path) that signs the database server's certificate.
pub(crate) const DEV_PG_CA_ENV: &str = "RUSTY_DEV_PG_CA_BUNDLE";

const DEFAULT_CONFIG: &str = "/etc/rusty/rusty.toml";
const DEFAULT_DEV_DIR: &str = "rusty-dev-data";

pub const USAGE: &str = "usage:\n  \
rusty d [--dev [--data-dir DIR] [--port-offset N] [--bind IP]] [--config FILE] [--log-format text|json]\n  \
rusty ctl <command> ...        (rustyctl)\n  \
rusty healthcheck [options of `rusty d`]   probe the listeners (exit 0 healthy, 1 not)\n  \
rusty --version\n\
\n\
  rusty d --dev      development profile: data under ./rusty-dev-data, high ports on 127.0.0.1\n\
                     (2525, 5587, 4465, 1143, 9993), self-signed certificate. Never for the internet.\n\
  rusty d --config   production: read FILE (default /etc/rusty/rusty.toml)\n\
The binary is multi-call: invoked as `rustyd` or `rustyctl` it behaves as `rusty d` / `rusty ctl`.";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DaemonArgs {
    pub dev: bool,
    pub config: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub port_offset: u16,
    pub bind: Option<std::net::IpAddr>,
    pub log_format: Option<LogFormat>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    Daemon(DaemonArgs),
    Ctl(Vec<OsString>),
    /// `rusty healthcheck`: probe the listeners a daemon started with the same options would serve.
    Healthcheck(DaemonArgs),
    Version,
    Help,
}

fn parse_daemon(args: &[OsString]) -> Result<Invocation, String> {
    let mut a = DaemonArgs::default();
    let mut it = args.iter();
    let value = |it: &mut std::slice::Iter<'_, OsString>, opt: &str| -> Result<OsString, String> {
        it.next().cloned().ok_or_else(|| format!("{opt} needs a value"))
    };
    while let Some(arg) = it.next() {
        match arg.to_str() {
            Some("--dev") => a.dev = true,
            Some("--config") => a.config = Some(PathBuf::from(value(&mut it, "--config")?)),
            Some("--data-dir") => a.data_dir = Some(PathBuf::from(value(&mut it, "--data-dir")?)),
            Some("--port-offset") => {
                let v = value(&mut it, "--port-offset")?;
                a.port_offset = v
                    .to_str()
                    .and_then(|s| s.parse::<u16>().ok())
                    .ok_or_else(|| "--port-offset must be a number 0..=65535".to_owned())?;
            }
            Some("--bind") => {
                a.bind = Some(
                    value(&mut it, "--bind")?
                        .to_str()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| "--bind must be an IP address".to_owned())?,
                );
            }
            Some("--log-format") => {
                a.log_format = match value(&mut it, "--log-format")?.to_str() {
                    Some("text") => Some(LogFormat::Text),
                    Some("json") => Some(LogFormat::Json),
                    _ => return Err("--log-format must be text or json".into()),
                }
            }
            Some("--help") | Some("-h") => return Ok(Invocation::Help),
            _ => return Err("unknown option".into()),
        }
    }
    if a.dev && a.config.is_some() {
        return Err("--dev and --config exclude each other".into());
    }
    if !a.dev && (a.data_dir.is_some() || a.port_offset != 0 || a.bind.is_some()) {
        return Err("--data-dir, --port-offset and --bind need --dev".into());
    }
    Ok(Invocation::Daemon(a))
}

/// Decide what the process was asked to do from its full argument vector (argv[0] included).
pub fn parse_args(argv: &[OsString]) -> Result<Invocation, String> {
    let name = argv
        .first()
        .and_then(|a| Path::new(a).file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("rusty")
        .to_owned();
    let rest = argv.get(1..).unwrap_or_default();
    match name.as_str() {
        "rustyd" => parse_daemon(rest),
        "rustyctl" => Ok(Invocation::Ctl(rest.to_vec())),
        _ => match rest.first().and_then(|s| s.to_str()) {
            Some("d") => parse_daemon(rest.get(1..).unwrap_or_default()),
            Some("ctl") => Ok(Invocation::Ctl(rest.get(1..).unwrap_or_default().to_vec())),
            Some("healthcheck") => match parse_daemon(rest.get(1..).unwrap_or_default())? {
                Invocation::Daemon(a) => Ok(Invocation::Healthcheck(a)),
                other => Ok(other),
            },
            Some("--version") | Some("-V") | Some("version") => Ok(Invocation::Version),
            Some("--help") | Some("-h") | Some("help") => Ok(Invocation::Help),
            Some(_) => Err("unknown command".into()),
            None => Err("a command is required".into()),
        },
    }
}

/// The process entry point: returns the exit code.
pub fn main(argv: Vec<OsString>) -> ExitCode {
    let prog = argv
        .first()
        .and_then(|a| Path::new(a).file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("rusty")
        .to_owned();
    match parse_args(&argv) {
        Err(msg) => {
            eprintln!("{prog}: {msg}\n{USAGE}");
            ExitCode::from(2)
        }
        Ok(Invocation::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Invocation::Version) => {
            println!("rusty {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Invocation::Healthcheck(args)) => match load_config(&args, false).and_then(|(cfg, _)| crate::health::check(&cfg)) {
            Ok(n) => {
                println!("healthy: {n} listeners answered");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{prog}: unhealthy: {e}");
                ExitCode::from(1)
            }
        },
        Ok(Invocation::Ctl(args)) => crate::ctl::run(args),
        Ok(Invocation::Daemon(args)) => {
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("{prog}: cannot start the runtime: {e}");
                    return ExitCode::from(1);
                }
            };
            ExitCode::from(rt.block_on(run_daemon(&prog, args)))
        }
    }
}

/// The dev profile configuration for `data_dir` (default `./rusty-dev-data`), shared by `rusty d --dev`
/// and `rustyctl --dev`. The metadata database is the Postgres server named by the environment.
pub(crate) fn dev_config(data_dir: Option<&Path>, with_store: bool) -> Result<StartupConfig, String> {
    let dir = match data_dir {
        Some(d) => d.to_path_buf(),
        None => PathBuf::from(DEFAULT_DEV_DIR),
    };
    let dir = if dir.is_absolute() {
        dir
    } else {
        std::env::current_dir().map_err(|e| format!("cannot read the current directory: {e}"))?.join(dir)
    };
    let cfg = StartupConfig::dev(&dir);
    if !with_store {
        // `rusty healthcheck` only probes the listeners: it needs no database.
        return Ok(cfg);
    }
    // The dev profile keeps its metadata in Postgres: the developer names the server in RUSTY_DEV_PG_URL
    // (postgres://user@host:port/database, no password) and may give the password in RUSTY_DEV_PG_PASSWORD. The
    // schema is built once with `rustyctl --dev db migrate --without-roles`. Production reads its whole
    // configuration from --config.
    let sslmode = match std::env::var(DEV_PG_SSLMODE_ENV).ok().as_deref() {
        None | Some("") => None,
        Some("disable") => Some(SslMode::Disable),
        Some("require") => Some(SslMode::Require),
        Some("verify-ca") => Some(SslMode::VerifyCa),
        Some("verify-full") => Some(SslMode::VerifyFull),
        Some(_) => return Err(format!("{DEV_PG_SSLMODE_ENV} must be disable, require, verify-ca or verify-full")),
    };
    let ca_bundle = std::env::var_os(DEV_PG_CA_ENV).filter(|v| !v.is_empty()).map(PathBuf::from);
    match std::env::var(DEV_PG_URL_ENV) {
        Ok(url) if !url.is_empty() => cfg.with_dev_postgres(&url, sslmode, ca_bundle).map_err(|e| format!("{DEV_PG_URL_ENV}: {e}")),
        Ok(_) | Err(std::env::VarError::NotPresent) => Err(format!(
            "the dev profile needs a Postgres server: set {DEV_PG_URL_ENV} (postgres://user@host:5432/database, no password) and optionally {DEV_PG_PASSWORD_ENV}, then run `rustyctl --dev db migrate --without-roles` once"
        )),
        Err(_) => Err(format!("{DEV_PG_URL_ENV} is not valid UTF-8")),
    }
}

pub(crate) const DEFAULT_CONFIG_PATH: &str = DEFAULT_CONFIG;

fn load_config(args: &DaemonArgs, with_store: bool) -> Result<(StartupConfig, Profile), String> {
    if args.dev {
        let mut cfg = with_port_offset(dev_config(args.data_dir.as_deref(), with_store)?, args.port_offset).map_err(|e| e.to_string())?;
        if let Some(ip) = args.bind {
            cfg = with_bind(cfg, ip);
        }
        Ok((cfg, Profile::Dev))
    } else {
        let path = args.config.clone().unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG));
        let cfg = StartupConfig::load(&path).map_err(|e| format!("configuration: {e}"))?;
        Ok((cfg, Profile::Production))
    }
}

/// How often the certificate files are looked at. The edge renews certificates and writes them for this service (see docs/acme.md): a change is picked up
/// by the next handshake without a signal or a restart.
const CERT_POLL: std::time::Duration = std::time::Duration::from_secs(30);
/// The wait after a change is seen, so a writer can finish both files before they are read.
const CERT_SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

fn stamp(paths: &[std::path::PathBuf]) -> Vec<Option<(std::time::SystemTime, u64)>> {
    paths.iter().map(|p| std::fs::metadata(p).ok().and_then(|m| m.modified().ok().map(|t| (t, m.len())))).collect()
}

/// Reloads the TLS provider when a certificate or key file changes (modification time or size). A failed reload (half-written or invalid pair) keeps the
/// certificate in use and is tried again at the next change.
fn watch_cert_files(paths: Vec<std::path::PathBuf>, tls: rusty_svc::TlsProvider, stop: CancellationToken, poll: std::time::Duration, settle: std::time::Duration) {
    tokio::spawn(async move {
        let mut seen = stamp(&paths);
        loop {
            tokio::select! { _ = stop.cancelled() => return, _ = tokio::time::sleep(poll) => {} }
            let now = stamp(&paths);
            if now == seen {
                continue;
            }
            tokio::time::sleep(settle).await;
            let now = stamp(&paths);
            match tls.reload() {
                Ok(()) => {
                    tracing::info!("certificate files changed: reloaded");
                    seen = now;
                }
                Err(e) => tracing::warn!(error = %e, "certificate files changed but did not load; keeping the current certificate"),
            }
        }
    });
}

async fn run_daemon(
prog: &str, args: DaemonArgs) -> u8 {
    // Signals are claimed before anything starts, so one that arrives early is not lost to the
    // default action (which would kill the process without closing the store).
    let Ok(signals) = Signals::claim() else {
        eprintln!("{prog}: cannot install signal handlers");
        return 1;
    };
    let (mut cfg, profile) = match load_config(&args, true) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{prog}: {e}");
            return 1;
        }
    };
    if let Some(f) = args.log_format {
        cfg.log_format = f;
    }
    // A second initialisation (tests in one process) is not an error worth stopping for.
    let _ = init_logging(cfg.log_level, cfg.log_format);

    let watched: Vec<std::path::PathBuf> = [cfg.tls.cert_file.clone(), cfg.tls.key_file.clone()].into_iter().flatten().collect();
    let token = CancellationToken::new();
    let running = match start(cfg, profile, token.clone()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "start-up failed");
            eprintln!("{prog}: start-up failed: {e}");
            return 1;
        }
    };

    let tls = running.tls().clone();
    if !watched.is_empty() {
        watch_cert_files(watched, running.tls().clone(), token.clone(), CERT_POLL, CERT_SETTLE);
    }
    signals.spawn(token.clone(), Some(Box::new(move || tls.reload())));

    exit_code(running.wait().await)
}

#[cfg(test)]
mod cert_watch_tests {
    use super::*;
    use rusty_core::config::TlsConfig;
    use rusty_svc::{FileSecretSource, TlsProvider};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::time::Duration;

    fn write_pair(dir: &std::path::Path, name: &str) {
        let ck = rcgen::generate_simple_self_signed(vec![name.to_owned()]).unwrap();
        let (c, k) = (dir.join("cert.pem"), dir.join("key.pem"));
        std::fs::write(&c, ck.cert.pem()).unwrap();
        std::fs::write(&k, ck.signing_key.serialize_pem()).unwrap();
        std::fs::set_permissions(&c, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&k, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    async fn leaf_changes(p: &TlsProvider, from: &[u8]) -> bool {
        for _ in 0..100 {
            if p.leaf_der().as_deref() != Some(from) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_certificate_the_edge_renews_is_served_without_a_restart_and_a_broken_write_keeps_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        write_pair(dir.path(), "mail.example.test");
        let cfg = TlsConfig { cert_file: Some(dir.path().join("cert.pem")), key_file: Some(dir.path().join("key.pem")), dev_self_signed: false };
        let tls = TlsProvider::from_config(&cfg, "mail.example.test", Arc::new(FileSecretSource::new())).unwrap();
        let first = tls.leaf_der().unwrap();
        let stop = CancellationToken::new();
        watch_cert_files(vec![dir.path().join("cert.pem"), dir.path().join("key.pem")], tls.clone(), stop.clone(), Duration::from_millis(50), Duration::from_millis(20));

        // A half-written pair (new certificate, old key) does not load: the certificate in use stays.
        let old_key = std::fs::read(dir.path().join("key.pem")).unwrap();
        write_pair(dir.path(), "mail.example.test");
        std::fs::write(dir.path().join("key.pem"), &old_key).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(tls.leaf_der().unwrap(), first, "a mismatched pair must not replace the certificate");

        // The pair completes: the next handshakes use the new certificate.
        write_pair(dir.path(), "mail.example.test");
        assert!(leaf_changes(&tls, &first).await, "the renewed certificate was not picked up");
        stop.cancel();
    }
}
