//! Command line: multi-call dispatch (`rusty d`, `rusty ctl`, or argv[0] `rustyd` / `rustyctl`),
//! option parsing, signal handling and exit codes (0 clean, 1 runtime failure, 2 usage).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rusty_core::config::{LogFormat, Profile, StartupConfig};
use rusty_svc::init_logging;
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;

use crate::daemon::{start, with_bind, with_port_offset};

const DEFAULT_CONFIG: &str = "/etc/rusty/rusty.toml";
const DEFAULT_DEV_DIR: &str = "rusty-dev-data";

pub const USAGE: &str = "usage:\n  \
rusty d [--dev [--data-dir DIR] [--port-offset N] [--bind IP]] [--config FILE] [--log-format text|json]\n  \
rusty ctl <command> ...        (rustyctl)\n\
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
/// and `rustyctl --dev`.
pub(crate) fn dev_config(data_dir: Option<&Path>) -> Result<StartupConfig, String> {
    let dir = match data_dir {
        Some(d) => d.to_path_buf(),
        None => PathBuf::from(DEFAULT_DEV_DIR),
    };
    let dir = if dir.is_absolute() {
        dir
    } else {
        std::env::current_dir().map_err(|e| format!("cannot read the current directory: {e}"))?.join(dir)
    };
    Ok(StartupConfig::dev(&dir))
}

pub(crate) const DEFAULT_CONFIG_PATH: &str = DEFAULT_CONFIG;

fn load_config(args: &DaemonArgs) -> Result<(StartupConfig, Profile), String> {
    if args.dev {
        let mut cfg = with_port_offset(dev_config(args.data_dir.as_deref())?, args.port_offset).map_err(|e| e.to_string())?;
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

async fn run_daemon(prog: &str, args: DaemonArgs) -> u8 {
    // Signals are claimed before anything starts, so one that arrives early is not lost to the
    // default action (which would kill the process without closing the store).
    let (mut term, mut int, mut hup) = match (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    ) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        _ => {
            eprintln!("{prog}: cannot install signal handlers");
            return 1;
        }
    };
    let (mut cfg, profile) = match load_config(&args) {
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
    let stop = token.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = term.recv() => { tracing::info!("SIGTERM: shutting down"); stop.cancel(); return; }
                _ = int.recv() => { tracing::info!("SIGINT: shutting down"); stop.cancel(); return; }
                _ = hup.recv() => match tls.reload() {
                    Ok(()) => tracing::info!("SIGHUP: TLS certificate reloaded"),
                    Err(e) => tracing::error!(error = %e, "SIGHUP: TLS reload failed; keeping the previous certificate"),
                },
                _ = stop.cancelled() => return,
            }
        }
    });

    match running.wait().await {
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
