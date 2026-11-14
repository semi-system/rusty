//! `rusty ctl` / `rustyctl`: the composition root of the administration commands. It parses the
//! command line (`rusty-ctl`), reads the configuration, opens the store, builds the `Directory`
//! facade and the password reader (standard input or the terminal), runs the command and prints
//! the result. All command logic lives in `rusty-ctl`; this file only wires.
//!
//! Exit codes: 0 success, 1 the command failed, 2 usage error.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use rusty_core::config::{Profile, StartupConfig};
use rusty_ctl::{
    execute, parse_args, render, CtlEnv, CtlError, PasswordReader, PasswordSource, Secret, Source, USAGE,
};
use rusty_directory::Directory;
use rusty_store::Store;

use crate::cli::{dev_config, DEFAULT_CONFIG_PATH};
use crate::daemon::prepare_dev_dirs;

/// Longest password line read from standard input or the terminal.
const MAX_PASSWORD_LINE: u64 = 4096;

/// Passwords from standard input (one line) or the controlling terminal (twice, no echo).
struct TerminalPasswords;

fn trim_line(mut line: Vec<u8>) -> Result<Secret, CtlError> {
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    let s = String::from_utf8(line).map_err(|_| CtlError::runtime("password: not valid UTF-8"))?;
    if s.is_empty() {
        return Err(CtlError::runtime("password: empty"));
    }
    Ok(Secret::new(s))
}

/// One line of at most `MAX_PASSWORD_LINE` bytes; a longer one is refused (never truncated).
fn read_line_bounded(r: &mut impl BufRead) -> Result<Vec<u8>, CtlError> {
    let mut buf = Vec::new();
    r.take(MAX_PASSWORD_LINE + 1)
        .read_until(b'\n', &mut buf)
        .map_err(|_| CtlError::runtime("password: cannot read it"))?;
    if buf.len() as u64 > MAX_PASSWORD_LINE {
        return Err(CtlError::runtime("password: too long"));
    }
    Ok(buf)
}

fn prompt_tty() -> Result<Secret, CtlError> {
    let no_tty = || CtlError::runtime("no terminal available; use --password-stdin");
    let mut tty = OpenOptions::new().read(true).write(true).open("/dev/tty").map_err(|_| no_tty())?;
    let saved = rustix::termios::tcgetattr(&tty).map_err(|_| no_tty())?;
    let mut quiet = saved.clone();
    quiet.local_modes.remove(rustix::termios::LocalModes::ECHO);
    rustix::termios::tcsetattr(&tty, rustix::termios::OptionalActions::Flush, &quiet).map_err(|_| no_tty())?;
    // Echo comes back on every path out of here.
    let result = (|| {
        let mut ask = |text: &str| -> Result<Vec<u8>, CtlError> {
            let _ = write!(tty, "{text}");
            let _ = tty.flush();
            let mut reader = std::io::BufReader::new(&tty);
            let line = read_line_bounded(&mut reader);
            let _ = writeln!(&tty);
            line
        };
        let first = ask("Password: ")?;
        let second = ask("Repeat password: ")?;
        if first != second {
            return Err(CtlError::runtime("password: the two entries differ"));
        }
        trim_line(first)
    })();
    let _ = rustix::termios::tcsetattr(&tty, rustix::termios::OptionalActions::Flush, &saved);
    result
}

impl PasswordReader for TerminalPasswords {
    fn read(&mut self, source: PasswordSource) -> Result<Secret, CtlError> {
        match source {
            PasswordSource::Stdin => {
                let stdin = std::io::stdin();
                let line = read_line_bounded(&mut stdin.lock())?;
                trim_line(line)
            }
            PasswordSource::Prompt => prompt_tty(),
            PasswordSource::None => Err(CtlError::usage("no password source")),
        }
    }
}

fn config_for(source: &Source) -> Result<(StartupConfig, bool, String), String> {
    match source {
        Source::Dev { data_dir } => {
            let cfg = dev_config(data_dir.as_deref(), true)?;
            Ok((cfg, true, "development profile".to_owned()))
        }
        Source::Config(path) => {
            let path = path.clone().unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
            let cfg = StartupConfig::load(&path).map_err(|e| format!("configuration: {e}"))?;
            // The same start-up rules as the daemon (hostname, TLS for authenticated listeners, ...).
            cfg.check_startup(Profile::Production).map_err(|e| format!("configuration: {e}"))?;
            Ok((cfg, false, "read".to_owned()))
        }
    }
}

async fn open_directory(cfg: &StartupConfig, dev: bool) -> Result<(Store, Directory), String> {
    if dev {
        prepare_dev_dirs(cfg).await.map_err(|e| e.to_string())?;
    }
    let profile = if dev { Profile::Dev } else { Profile::Production };
    let sc = rusty_runtime::store::store_config(cfg, profile).map_err(|e| e.to_string())?;
    let store = Store::open(sc).await.map_err(|e| format!("cannot open the database: {e}"))?;
    let dir = Directory::with_repo(store.repos().directory.clone());
    Ok((store, dir))
}

/// Entry point of `rusty ctl ...` (the arguments after `ctl`).
pub fn run(args: Vec<OsString>) -> ExitCode {
    let prog = "rustyctl";
    let Some(args) = args.into_iter().map(|a| a.into_string().ok()).collect::<Option<Vec<String>>>() else {
        eprintln!("{prog}: arguments must be valid UTF-8\n{USAGE}");
        return ExitCode::from(2);
    };
    let parsed = match parse_args(&args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{prog}: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if parsed.command == rusty_ctl::Command::Help {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{prog}: cannot start the runtime: {e}");
            return ExitCode::from(1);
        }
    };
    ExitCode::from(rt.block_on(async move {
        let (cfg, dev, note) = match config_for(&parsed.source) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("{prog}: {e}");
                return 1;
            }
        };
        if matches!(parsed.command, rusty_ctl::Command::DbMigrate { .. } | rusty_ctl::Command::DbStatus) {
            return run_db(prog, &cfg, &parsed).await;
        }
        let (store, directory) = match open_directory(&cfg, dev).await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("{prog}: {e}");
                return 1;
            }
        };
        let mut passwords = TerminalPasswords;
        let mut env = CtlEnv {
            directory: &directory,
            passwords: &mut passwords,
            hostname: cfg.hostname.as_deref(),
            config_note: &note,
            dkim_dir: cfg.outbound.dkim_dir.as_deref(),
        };
        let code = match execute(&parsed.command, &mut env).await {
            Ok(outcome) => {
                let text = render(&outcome, parsed.json);
                if !text.is_empty() {
                    println!("{text}");
                }
                0
            }
            Err(e) => {
                if parsed.json {
                    eprintln!("{}", serde_error(&e));
                } else {
                    eprintln!("{prog}: {e}");
                }
                e.exit
            }
        };
        // A failure while closing the pool is reported but does not hide the result.
        if let Err(e) = store.close().await {
            eprintln!("{prog}: closing the database failed: {e}");
            return if code == 0 { 1 } else { code };
        }
        code
    }))
}

/// `db migrate` and `db status`: the migrator role against Postgres directly; never the daemon's role,
/// never through a pooler.
async fn run_db(prog: &str, cfg: &StartupConfig, parsed: &rusty_ctl::Parsed) -> u8 {
    use rusty_store::{migrate_postgres, schema_status, SchemaState};
    let say = |text: String, json: serde_json::Value| {
        println!("{}", if parsed.json { json.to_string() } else { text });
    };
    let (url, security) = match rusty_runtime::store::migrator_target(cfg) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{prog}: {e}");
            return 1;
        }
    };
    let name = |s: &SchemaState| format!("{s:?}").to_lowercase();
    match &parsed.command {
        rusty_ctl::Command::DbStatus => match schema_status(&url, &security).await {
            Ok(s) => {
                say(
                    format!("schema: {} (applied {}, expected {}, pending {:?})", name(&s.state), s.applied.len(), s.expected.len(), s.pending()),
                    serde_json::json!({ "state": name(&s.state), "applied": s.applied, "expected": s.expected, "pending": s.pending() }),
                );
                if s.state == SchemaState::Current { 0 } else { 1 }
            }
            Err(e) => {
                eprintln!("{prog}: {e}");
                1
            }
        },
        rusty_ctl::Command::DbMigrate { require_roles } => match migrate_postgres(&url, &security, *require_roles).await {
            Ok(r) => {
                let applied_now: Vec<i64> = r.after.applied.iter().copied().filter(|v| !r.before.applied.contains(v)).collect();
                say(
                    format!("schema: {} (newly applied {:?}); privilege matrix {}", name(&r.after.state), applied_now, if r.grants_applied { "applied" } else { "NOT applied (roles missing)" }),
                    serde_json::json!({ "state": name(&r.after.state), "applied_now": applied_now, "grants_applied": r.grants_applied }),
                );
                0
            }
            Err(e) => {
                eprintln!("{prog}: {e}");
                1
            }
        },
        _ => 2,
    }
}

/// A failure in `--json` mode, so a script reads one format on both streams.
fn serde_error(e: &CtlError) -> String {
    serde_json::json!({ "error": e.to_string() }).to_string()
}
