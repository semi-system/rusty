//! `rustyctl` through the real binary: exit codes, passwords only from stdin, JSON output, and
//! administration while `rusty d` is running on the same database.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::{pg_dev, private_tmp, PgDev};
use rusty_directory::{Authenticator, Directory, LocalAuthenticator};
use rusty_store::Store;

const BIN: &str = env!("CARGO_BIN_EXE_rusty");

struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn finish(child: std::process::Child, stdin: &str) -> Out {
    let mut child = child;
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let o = child.wait_with_output().unwrap();
    Out {
        code: o.status.code(),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

/// `rusty ctl --dev --data-dir <data> <args>` against the database of `pg` (named in the environment, the way
/// a developer does it), with `stdin` as its standard input.
fn ctl(pg: &PgDev, data: &Path, args: &[&str], stdin: &str) -> Out {
    let child = Command::new(BIN)
        .args(["ctl", "--dev", "--data-dir", data.to_str().unwrap()])
        .args(args)
        .env("RUSTY_DEV_PG_URL", &pg.url)
        .env("RUSTY_DEV_PG_PASSWORD", &pg.password)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    finish(child, stdin)
}

async fn authenticator(pg: &PgDev) -> (Store, LocalAuthenticator) {
    let store = pg.open_store().await;
    let a = LocalAuthenticator::new(Directory::with_repo(store.repos().directory.clone())).await.unwrap();
    (store, a)
}

// backend-neutral: process-level test of the binary over one Postgres database
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_from_the_shell_creates_a_domain_and_two_accounts_that_can_log_in() {
    let tmp = private_tmp();
    let data = tmp.path().join("data");
    let pg = pg_dev().await;
    let o = ctl(&pg, &data, &["domain", "add", "example.com"], "");
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    assert!(o.stdout.contains("domain example.com created"), "{}", o.stdout);
    let o = ctl(&pg, &data, &["account", "add", "postmaster@example.com", "--password-stdin"], "first-secret-pw\n");
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    let o = ctl(&pg, &data, &["account", "add", "bob@example.com", "--password-stdin"], "second-secret-pw\r\n");
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    for out in [&o.stdout, &o.stderr] {
        assert!(!out.contains("second-secret-pw"));
    }

    let o = ctl(&pg, &data, &["--json", "account", "list"], "");
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    let v: serde_json::Value = serde_json::from_str(o.stdout.trim()).unwrap();
    assert_eq!(v["accounts"].as_array().unwrap().len(), 2);
    assert_eq!(v["accounts"][0]["address"], "postmaster@example.com");

    let (store, auth) = authenticator(&pg).await;
    assert!(auth.verify("postmaster@example.com", "first-secret-pw").await.unwrap().is_some());
    assert!(auth.verify("bob@example.com", "second-secret-pw").await.unwrap().is_some());
    assert!(auth.verify("bob@example.com", "first-secret-pw").await.unwrap().is_none());
    store.close().await.unwrap();
}

// backend-neutral: process-level test of the binary, no database involved
#[test]
fn a_password_in_argv_is_a_usage_error_and_is_not_echoed() {
    let tmp = private_tmp();
    let data = tmp.path().join("data");
    let child = Command::new(BIN)
        .args(["ctl", "--dev", "--data-dir", data.to_str().unwrap()])
        .args(["account", "add", "a@example.com", "--password", "hunter2-secret"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let o = finish(child, "");
    assert_eq!(o.code, Some(2));
    assert!(!o.stderr.contains("hunter2-secret") && !o.stdout.contains("hunter2-secret"));
    // Usage errors touch nothing: the data directory was not even created.
    assert!(!data.exists());
}

// backend-neutral: process-level test of the binary over one Postgres database
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failures_exit_one_and_leave_nothing_behind() {
    let tmp = private_tmp();
    let data = tmp.path().join("data");
    let pg = pg_dev().await;
    assert_eq!(ctl(&pg, &data, &["domain", "add", "example.com"], "").code, Some(0));
    // duplicate domain, missing domain, empty password, short password
    let o = ctl(&pg, &data, &["domain", "add", "example.com"], "");
    assert_eq!((o.code, o.stderr.contains("already exists")), (Some(1), true), "{}", o.stderr);
    let o = ctl(&pg, &data, &["account", "add", "a@nowhere.test", "--password-stdin"], "long-enough-pw\n");
    assert_eq!((o.code, o.stderr.contains("domain: not found")), (Some(1), true), "{}", o.stderr);
    let o = ctl(&pg, &data, &["account", "add", "a@example.com", "--password-stdin"], "");
    assert_eq!((o.code, o.stderr.contains("password: empty")), (Some(1), true), "{}", o.stderr);
    let o = ctl(&pg, &data, &["account", "add", "a@example.com", "--password-stdin"], "short\n");
    assert_eq!((o.code, o.stderr.contains("too short")), (Some(1), true), "{}", o.stderr);
    let o = ctl(&pg, &data, &["--json", "account", "list"], "");
    let v: serde_json::Value = serde_json::from_str(o.stdout.trim()).unwrap();
    assert!(v["accounts"].as_array().unwrap().is_empty());
    // a bad configuration file is a runtime failure (1), not a usage error (2)
    let c = Command::new(BIN).args(["ctl", "--config", "/nonexistent/rusty.toml", "domain", "list"]).output().unwrap();
    assert_eq!(c.status.code(), Some(1));
    // an overlong password line is refused, not truncated
    let long = format!("{}\n", "x".repeat(5000));
    let o = ctl(&pg, &data, &["account", "add", "a@example.com", "--password-stdin"], &long);
    assert_eq!((o.code, o.stderr.contains("password: too long")), (Some(1), true), "{}", o.stderr);
}

// backend-neutral: process-level test of the binary over one Postgres database
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctl_and_a_running_daemon_write_the_same_database_concurrently() {
    let tmp = private_tmp();
    let data = tmp.path().join("data");
    let pg = pg_dev().await;
    let off = 1_000 + (std::process::id() % 15_000) as u16;
    let mut daemon = Command::new(BIN)
        .args(["d", "--dev", "--data-dir", data.to_str().unwrap(), "--port-offset", &off.to_string()])
        .env("RUSTY_DEV_PG_URL", &pg.url)
        .env("RUSTY_DEV_PG_PASSWORD", &pg.password)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let t0 = Instant::now();
    while TcpStream::connect(("127.0.0.1", 2525 + off)).is_err() {
        assert!(t0.elapsed() < Duration::from_secs(30), "the daemon never came up");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(ctl(&pg, &data, &["domain", "add", "live.test"], "").code, Some(0));
    // four ctl processes write at once, while the daemon holds the same database open
    let (url, password) = (pg.url.clone(), pg.password.clone());
    let threads: Vec<_> = (0..4)
        .map(|t| {
            let data: PathBuf = data.clone();
            let (url, password) = (url.clone(), password.clone());
            std::thread::spawn(move || {
                for i in 0..5 {
                    let addr = format!("u{t}x{i}@live.test");
                    let child = Command::new(BIN)
                        .args(["ctl", "--dev", "--data-dir", data.to_str().unwrap()])
                        .args(["account", "add", &addr, "--password-stdin"])
                        .env("RUSTY_DEV_PG_URL", &url)
                        .env("RUSTY_DEV_PG_PASSWORD", &password)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .unwrap();
                    let o = finish(child, "some-long-password\n");
                    assert_eq!(o.code, Some(0), "{addr}: {}", o.stderr);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let o = ctl(&pg, &data, &["--json", "account", "list", "live.test"], "");
    let v: serde_json::Value = serde_json::from_str(o.stdout.trim()).unwrap();
    assert_eq!(v["accounts"].as_array().unwrap().len(), 20);
    // the daemon is still healthy and stops cleanly
    assert!(daemon.try_wait().unwrap().is_none());
    let st = Command::new("kill").args(["-TERM", &daemon.id().to_string()]).status().unwrap();
    assert!(st.success());
    let status = daemon.wait().unwrap();
    assert_eq!(status.code(), Some(0));
    let mut err = String::new();
    daemon.stderr.take().unwrap().read_to_string(&mut err).unwrap();
}

/// A production-style configuration file (only port 25 enabled, so no TLS material is needed) whose `[store]`
/// section is `store`.
fn prod_config(dir: &Path, store: &str) -> PathBuf {
    let cfg = dir.join("rusty.toml");
    std::fs::write(
        &cfg,
        format!(
            "blob_dir = \"{}\"\nhostname = \"mx.example.test\"\n[smtp]\nlisten_587 = \"\"\nlisten_465 = \"\"\n[imap]\nlisten_143 = \"\"\nlisten_993 = \"\"\n[store]\n{store}",
            dir.join("data/blobs").display()
        ),
    )
    .unwrap();
    cfg
}

fn ctl_cfg(cfg: &Path, args: &[&str]) -> Out {
    let o = Command::new(BIN).args(["ctl", "--config", cfg.to_str().unwrap()]).args(args).stdin(Stdio::null()).output().unwrap();
    Out { code: o.status.code(), stdout: String::from_utf8_lossy(&o.stdout).into_owned(), stderr: String::from_utf8_lossy(&o.stderr).into_owned() }
}

// backend-neutral: process-level test of the binary; every case is refused before a connection is made
#[test]
fn production_mode_runs_the_daemons_startup_checks_and_fails_closed() {
    let tmp = private_tmp();
    // a configuration the daemon would refuse (no hostname) is refused here too
    let bad = tmp.path().join("bad.toml");
    std::fs::write(
        &bad,
        format!("blob_dir = \"{}\"\n[store]\nurl = \"postgres://rusty_app@db.internal/rusty\"\npassword_file = \"/run/secrets/pg\"\n", tmp.path().join("blobs").display()),
    )
    .unwrap();
    let o = ctl_cfg(&bad, &["domain", "list"]);
    assert_eq!(o.code, Some(1), "{}", o.stderr);
    assert!(o.stderr.contains("hostname"), "{}", o.stderr);
    // no [store] section at all: Postgres is the only backend, so there is nothing to default to
    let none = tmp.path().join("none.toml");
    std::fs::write(&none, format!("blob_dir = \"{}\"\nhostname = \"mx.example.test\"\n", tmp.path().join("blobs").display())).unwrap();
    let o = ctl_cfg(&none, &["domain", "list"]);
    assert_eq!(o.code, Some(1), "{}", o.stderr);
    // a database file path is not a setting any more: the key is refused, not ignored
    let old = tmp.path().join("old.toml");
    std::fs::write(&old, format!("database_path = \"/var/lib/rusty/meta.sqlite\"\nblob_dir = \"{}\"\n", tmp.path().join("blobs").display())).unwrap();
    assert_eq!(ctl_cfg(&old, &["domain", "list"]).code, Some(1));
    // production never talks to a database without a password file or without verified TLS
    let nopw = prod_config(tmp.path(), "url = \"postgres://rusty_app@db.internal/rusty\"\n");
    let o = ctl_cfg(&nopw, &["domain", "list"]);
    assert_eq!(o.code, Some(1), "{}", o.stderr);
    assert!(o.stderr.contains("password_file"), "{}", o.stderr);
    let plain = prod_config(tmp.path(), "url = \"postgres://rusty_app@db.internal/rusty\"\npassword_file = \"/run/secrets/pg\"\nsslmode = \"disable\"\n");
    let o = ctl_cfg(&plain, &["domain", "list"]);
    assert_eq!(o.code, Some(1), "{}", o.stderr);
    assert!(o.stderr.contains("sslmode"), "{}", o.stderr);
}

// backend-neutral: process-level test of the binary; the dev profile needs a named Postgres server
#[test]
fn the_dev_profile_without_a_database_says_what_to_set() {
    let tmp = private_tmp();
    let data = tmp.path().join("data");
    let child = Command::new(BIN)
        .args(["ctl", "--dev", "--data-dir", data.to_str().unwrap(), "domain", "list"])
        .env_remove("RUSTY_DEV_PG_URL")
        .env_remove("RUSTY_DEV_PG_PASSWORD")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let o = finish(child, "");
    assert_eq!(o.code, Some(1), "{}", o.stderr);
    assert!(o.stderr.contains("RUSTY_DEV_PG_URL"), "{}", o.stderr);
    // a password in the URL is refused and never echoed
    let child = Command::new(BIN)
        .args(["ctl", "--dev", "--data-dir", data.to_str().unwrap(), "domain", "list"])
        .env("RUSTY_DEV_PG_URL", "postgres://rusty:hunter2-secret@127.0.0.1:5432/rusty")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let o = finish(child, "");
    assert_eq!(o.code, Some(1), "{}", o.stderr);
    assert!(!o.stderr.contains("hunter2-secret") && !o.stdout.contains("hunter2-secret"));
}
