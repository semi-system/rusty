//! The real `rusty-admin` binary: the setup token is the only line on standard output, it is not in the logs, the service answers on its port, SIGTERM
//! stops it cleanly, and a bad command line or configuration exits 2 without echoing what was typed.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rusty_store::fixtures::pg_db as pg;

const BIN: &str = env!("CARGO_BIN_EXE_rusty-admin");

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn write_config(dir: &Path, db: &pg::PgTestDb, port: u16) -> std::path::PathBuf {
    let (url, password) = db.split_url();
    let pw = dir.join("pg.pw");
    std::fs::write(&pw, password).unwrap();
    std::fs::set_permissions(&pw, std::fs::Permissions::from_mode(0o600)).unwrap();
    let conf = dir.join("admin.toml");
    std::fs::write(
        &conf,
        format!(
            "hostname = \"admin.example.org\"\n[admin]\nlisten = \"127.0.0.1:{port}\"\npublic_origin = \"https://admin.example.org\"\n\
             [limits]\nshutdown_grace_secs = 2\n\
             [store]\nurl = \"{url}\"\npassword_file = \"{pw}\"\nsslmode = \"disable\"\ninsecure_allow_unverified_tls = true\n",
            pw = pw.display()
        ),
    )
    .unwrap();
    conf
}

fn wait_for_port(port: u16, child: &mut Child) {
    let until = Instant::now() + Duration::from_secs(30);
    while Instant::now() < until {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        if let Ok(Some(st)) = child.try_wait() {
            panic!("the binary exited early: {st}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the binary did not listen");
}

fn http_get(port: u16, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, out.split_once("\r\n\r\n").map(|(_, b)| b.to_owned()).unwrap_or_default())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_binary_prints_only_the_token_serves_and_stops_cleanly_on_sigterm() {
    let url = pg::pg_admin_url().expect("RUSTY_TEST_PG_URL");
    let db = pg::PgTestDb::create_with_roles(&url).await;
    let tmp = tempfile::tempdir().unwrap();
    let port = free_port();
    let conf = write_config(tmp.path(), &db, port);
    let mut child = Command::new(BIN).args(["--config", conf.to_str().unwrap()]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut token = String::new();
    stdout.read_line(&mut token).unwrap();
    let token = token.trim().to_owned();
    if token.is_empty() {
        let mut e = String::new();
        let _ = child.stderr.take().unwrap().read_to_string(&mut e);
        panic!("no token; the binary said: {e}");
    }
    assert_eq!(token.len(), 64, "the first line of standard output is the token");
    wait_for_port(port, &mut child);
    assert_eq!(http_get(port, "/api/v1/setup"), (200, "{\"required\":true}".into()));
    // The container health check: the same configuration, the running service answers (exit 0); once it is stopped it does not (exit 1).
    let health = Command::new(BIN).args(["healthcheck", "--config", conf.to_str().unwrap()]).output().unwrap();
    assert_eq!(health.status.code(), Some(0), "{}", String::from_utf8_lossy(&health.stderr));
    assert!(health.stdout.is_empty(), "the health check prints nothing");
    let st = Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap();
    assert!(st.success());
    let status = child.wait().unwrap();
    let gone = Command::new(BIN).args(["healthcheck", "--config", conf.to_str().unwrap()]).output().unwrap();
    assert_eq!(gone.status.code(), Some(1), "a stopped service is not healthy");
    assert_eq!(status.code(), Some(0), "a clean stop");
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "", "nothing but the token was written to standard output");
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert!(!err.contains(&token), "the token is never logged");
    assert!(err.contains("listening"), "{err}");
}

#[test]
fn a_bad_command_line_or_configuration_exits_2_and_echoes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(BIN).arg("--bogus-secret-argument").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("bogus-secret"), "the argument is not echoed");
    let out = Command::new(BIN).args(["--config", tmp.path().join("missing.toml").to_str().unwrap()]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    // A section of another service is refused, and its value is not echoed.
    let conf = tmp.path().join("bad.toml");
    std::fs::write(&conf, "hostname = \"admin.example.org\"\n[smtp]\nlisten_25 = \"TOPSECRET\"\n").unwrap();
    std::fs::set_permissions(&conf, std::fs::Permissions::from_mode(0o600)).unwrap();
    let out = Command::new(BIN).args(["--config", conf.to_str().unwrap()]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("[smtp] is not accepted") && !err.contains("TOPSECRET"), "{err}");
    assert!(out.stdout.is_empty());
}
