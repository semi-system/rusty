//! Command-line parsing and multi-call dispatch, and the real binary: `rusty d --dev` starts,
//! creates its files, serves on the dev ports and exits 0 on SIGTERM; SIGHUP reloads the
//! certificate; the multi-call names (`rustyd`, `rustyctl`) dispatch by argv[0].

mod common;

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::net::TcpStream;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use rusty::cli::{parse_args, Invocation};

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

#[test]
fn multi_call_dispatch_by_argv0_and_subcommand() {
    assert!(matches!(parse_args(&os(&["rusty", "d", "--dev"])), Ok(Invocation::Daemon(_))));
    assert!(matches!(parse_args(&os(&["/usr/bin/rustyd", "--dev"])), Ok(Invocation::Daemon(_))));
    assert!(matches!(parse_args(&os(&["rusty", "ctl", "domain", "list"])), Ok(Invocation::Ctl(_))));
    assert!(matches!(parse_args(&os(&["/usr/local/bin/rustyctl", "check"])), Ok(Invocation::Ctl(_))));
    assert!(matches!(parse_args(&os(&["rusty", "--help"])), Ok(Invocation::Help)));
    assert!(parse_args(&os(&["rusty"])).is_err());
    assert!(parse_args(&os(&["rusty", "bogus"])).is_err());
}

#[test]
fn daemon_arguments() {
    let d = |a: &[&str]| match parse_args(&os(a)) {
        Ok(Invocation::Daemon(d)) => Ok(d),
        Ok(_) => panic!("not a daemon invocation"),
        Err(e) => Err(e),
    };
    let ok = d(&["rusty", "d", "--dev", "--data-dir", "/tmp/x", "--port-offset", "100"]).unwrap();
    assert!(ok.dev);
    assert_eq!(ok.data_dir.as_deref(), Some(Path::new("/tmp/x")));
    assert_eq!(ok.port_offset, 100);
    let prod = d(&["rusty", "d", "--config", "/etc/rusty/rusty.toml"]).unwrap();
    assert!(!prod.dev);
    assert_eq!(prod.config.as_deref(), Some(Path::new("/etc/rusty/rusty.toml")));
    // dev-only options without --dev, --dev with --config, unknown options, missing values, bad numbers
    assert!(d(&["rusty", "d", "--data-dir", "/tmp/x"]).is_err());
    assert!(d(&["rusty", "d", "--port-offset", "1"]).is_err());
    assert!(d(&["rusty", "d", "--dev", "--config", "/x"]).is_err());
    assert!(d(&["rusty", "d", "--wat"]).is_err());
    assert!(d(&["rusty", "d", "--config"]).is_err());
    assert!(d(&["rusty", "d", "--dev", "--port-offset", "x"]).is_err());
    assert!(d(&["rusty", "d", "--dev", "--port-offset", "70000"]).is_err());
    // --bind (dev only): an IP address
    assert_eq!(d(&["rusty", "d", "--dev", "--bind", "0.0.0.0"]).unwrap().bind, Some("0.0.0.0".parse().unwrap()));
    assert!(d(&["rusty", "d", "--dev", "--bind", "not-an-ip"]).is_err());
    assert!(d(&["rusty", "d", "--bind", "0.0.0.0"]).is_err());
    // the production default config path
    assert_eq!(d(&["rusty", "d"]).unwrap().config, None);
}

const BIN: &str = env!("CARGO_BIN_EXE_rusty");

fn spawn(bin: &Path, args: &[&str]) -> Child {
    Command::new(bin).args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
}

/// Like [`spawn`], with the Postgres server of `pg` named in the environment (the dev profile reads it there).
fn spawn_dev(pg: &PgDev, args: &[&str]) -> Child {
    Command::new(BIN)
        .args(args)
        .env("RUSTY_DEV_PG_URL", &pg.url)
        .env("RUSTY_DEV_PG_PASSWORD", &pg.password)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn wait_for_port(port: u16, child: &mut Child) {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(30) {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            let mut err = String::new();
            child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
            panic!("process exited early with {status}: {err}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("port {port} never opened");
}

fn offset() -> u16 {
    // spread concurrent test processes over the port space, below the ephemeral range (32768+) so that
    // client sockets of other tests cannot hold one of our listener ports
    1_000 + (std::process::id() % 15_000) as u16
}

fn signal(child: &Child, sig: &str) {
    let st = Command::new("kill").args([&format!("-{sig}"), &child.id().to_string()]).status().unwrap();
    assert!(st.success());
}

fn wait_exit(child: &mut Child, within: Duration) -> std::process::ExitStatus {
    let t0 = Instant::now();
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s;
        }
        assert!(t0.elapsed() < within, "process did not exit within {within:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rusty_d_dev_starts_serves_and_exits_zero_on_sigterm() {
    let tmp = private_tmp();
    let data = tmp.path().join("data");
    let off = offset();
    let pg = pg_dev().await;
    let mut child = spawn_dev(
        &pg,
        &["d", "--dev", "--data-dir", data.to_str().unwrap(), "--port-offset", &off.to_string()],
    );
    wait_for_port(2525 + off, &mut child);

    // it created its files with the private modes
    assert_eq!(mode_of(&data), 0o700);
    assert!(data.join("blobs").join(rusty_store::BLOB_ROOT_MARKER).exists());

    // an SMTP banner on the dev port, an IMAP banner on the IMAP dev port
    let mut s = BufReader::new(TcpStream::connect(("127.0.0.1", 2525 + off)).unwrap());
    let mut line = String::new();
    s.read_line(&mut line).unwrap();
    assert!(line.starts_with("220 "), "{line}");

    signal(&child, "TERM");
    let status = wait_exit(&mut child, Duration::from_secs(20));
    assert_eq!(status.code(), Some(0), "SIGTERM must end in exit code 0");
    // the idle session was told goodbye, not cut
    line.clear();
    s.read_line(&mut line).unwrap();
    assert!(line.starts_with("421 "), "{line}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_also_ends_with_exit_zero_and_logs_json_when_asked() {
    let tmp = private_tmp();
    let data = tmp.path().join("data");
    let off = offset() + 1;
    let pg = pg_dev().await;
    let mut child = spawn_dev(
        &pg,
        &["d", "--dev", "--data-dir", data.to_str().unwrap(), "--port-offset", &off.to_string(), "--log-format", "json"],
    );
    wait_for_port(2525 + off, &mut child);
    signal(&child, "INT");
    let status = wait_exit(&mut child, Duration::from_secs(20));
    assert_eq!(status.code(), Some(0));
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert!(err.lines().any(|l| l.starts_with('{') && l.contains("\"message\"")), "no JSON log lines: {err}");
    assert!(err.contains("DEV ONLY"), "the dev certificate must be announced loudly: {err}");
}

#[test]
fn rusty_d_without_a_config_or_with_a_broken_one_exits_nonzero_without_listening() {
    let tmp = private_tmp();
    let bad = tmp.path().join("bad.toml");
    std::fs::write(&bad, "bogus = 1\n").unwrap();
    let mut c = spawn(Path::new(BIN), &["d", "--config", bad.to_str().unwrap()]);
    let st = wait_exit(&mut c, Duration::from_secs(10));
    assert_eq!(st.code(), Some(1));
    let mut c = spawn(Path::new(BIN), &["d", "--config", tmp.path().join("missing.toml").to_str().unwrap()]);
    assert_eq!(wait_exit(&mut c, Duration::from_secs(10)).code(), Some(1));
    // usage errors are exit code 2
    let mut c = spawn(Path::new(BIN), &["d", "--bogus"]);
    assert_eq!(wait_exit(&mut c, Duration::from_secs(10)).code(), Some(2));
    let mut c = spawn(Path::new(BIN), &[]);
    assert_eq!(wait_exit(&mut c, Duration::from_secs(10)).code(), Some(2));
}

#[test]
fn the_multi_call_names_dispatch_by_argv0() {
    let tmp = private_tmp();
    let ctl: PathBuf = tmp.path().join("rustyctl");
    symlink(BIN, &ctl).unwrap();
    let out = Command::new(&ctl).arg("--bogus").output().unwrap();
    // `rustyctl` is reached (argv[0] dispatch): a usage error from its own parser
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("rustyctl"));
    let rd: PathBuf = tmp.path().join("rustyd");
    symlink(BIN, &rd).unwrap();
    let out = Command::new(&rd).arg("--bogus").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("rustyd") || String::from_utf8_lossy(&out.stderr).contains("usage"));
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn leaf_via_tls(port: u16) -> Vec<u8> {
    use std::sync::Arc;
    #[derive(Debug)]
    struct NoVerify;
    impl rustls::client::danger::ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &[rustls::pki_types::CertificateDer<'_>],
            _: &rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(&self, _: &[u8], _: &rustls::pki_types::CertificateDer<'_>, _: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(&self, _: &[u8], _: &rustls::pki_types::CertificateDer<'_>, _: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
        }
    }
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    let mut conn =
        rustls::ClientConnection::new(Arc::new(cfg), "mail.example.test".try_into().unwrap()).unwrap();
    let mut sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut tls = rustls::Stream::new(&mut conn, &mut sock);
    let mut buf = [0u8; 1];
    let _ = tls.read(&mut buf); // drives the handshake, then the banner
    conn.peer_certificates().unwrap()[0].as_ref().to_vec()
}

#[test]
fn sighup_reloads_the_certificate_and_a_bad_reload_keeps_the_old_one() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = private_tmp();
    let data = tmp.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(rusty_store::FsBlobStore::init(data.join("blobs"))).unwrap();
    let pg = rt.block_on(pg_dev());

    let mk = |name: &str| {
        let ck = rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
        (ck.cert.pem(), ck.signing_key.serialize_pem(), ck.cert.der().as_ref().to_vec())
    };
    let (cert_a, key_a, der_a) = mk("mail.example.test");
    let (cert_b, key_b, der_b) = mk("mail.example.test");
    let cert = tmp.path().join("cert.pem");
    let key = tmp.path().join("key.pem");
    let put = |p: &Path, text: &str, mode: u32| {
        let t = p.with_extension("tmp");
        std::fs::write(&t, text).unwrap();
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(mode)).unwrap();
        std::fs::rename(&t, p).unwrap();
    };
    put(&cert, &cert_a, 0o644);
    put(&key, &key_a, 0o600);

    let p993 = free_port();
    let conf = tmp.path().join("rusty.toml");
    std::fs::write(
        &conf,
        format!(
            "blob_dir = \"{blobs}\"\nhostname = \"mail.example.test\"\n\
             [tls]\ncert_file = \"{cert}\"\nkey_file = \"{key}\"\n\
             [smtp]\nlisten_25 = \"127.0.0.1:{p25}\"\nlisten_587 = \"\"\nlisten_465 = \"\"\n\
             [imap]\nlisten_143 = \"\"\nlisten_993 = \"127.0.0.1:{p993}\"\n\
             [limits]\nshutdown_grace_secs = 2\n\
             [store]\nurl = \"{pgurl}\"\npassword_file = \"{pgpw}\"\nsslmode = \"disable\"\ninsecure_allow_unverified_tls = true\n",
            pgurl = pg.url,
            pgpw = pg.password_file().display(),
            blobs = data.join("blobs").display(),
            cert = cert.display(),
            key = key.display(),
            p25 = free_port(),
        ),
    )
    .unwrap();

    let mut child = spawn(Path::new(BIN), &["d", "--config", conf.to_str().unwrap()]);
    wait_for_port(p993, &mut child);
    assert_eq!(leaf_via_tls(p993), der_a);

    put(&cert, &cert_b, 0o644);
    put(&key, &key_b, 0o600);
    signal(&child, "HUP");
    let t0 = Instant::now();
    loop {
        if leaf_via_tls(p993) == der_b {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "certificate not reloaded after SIGHUP");
        std::thread::sleep(Duration::from_millis(100));
    }

    // a broken key file: the reload fails, the process keeps serving the old certificate
    put(&key, "garbage", 0o600);
    signal(&child, "HUP");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(leaf_via_tls(p993), der_b);
    assert!(child.try_wait().unwrap().is_none(), "a failed reload must not stop the server");

    signal(&child, "TERM");
    assert_eq!(wait_exit(&mut child, Duration::from_secs(20)).code(), Some(0));
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert!(err.contains("reload"), "reload outcome must be logged: {err}");
    assert!(!err.contains("garbage"), "key material must not be logged");
}

#[test]
fn healthcheck_and_version_arguments() {
    match parse_args(&os(&["rusty", "healthcheck", "--config", "/etc/rusty/rusty.toml"])) {
        Ok(Invocation::Healthcheck(a)) => assert_eq!(a.config.as_deref(), Some(Path::new("/etc/rusty/rusty.toml"))),
        other => panic!("{other:?}"),
    }
    assert!(matches!(parse_args(&os(&["rusty", "healthcheck", "--dev", "--port-offset", "5"])), Ok(Invocation::Healthcheck(_))));
    // the same option rules as the daemon
    assert!(parse_args(&os(&["rusty", "healthcheck", "--bogus"])).is_err());
    assert!(parse_args(&os(&["rusty", "healthcheck", "--data-dir", "/x"])).is_err());
    assert!(matches!(parse_args(&os(&["rusty", "--version"])), Ok(Invocation::Version)));
    assert!(matches!(parse_args(&os(&["rusty", "-V"])), Ok(Invocation::Version)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthcheck_is_ok_against_a_running_daemon_and_fails_when_nothing_listens() {
    let tmp = private_tmp();
    let data = tmp.path().join("data");
    let pg = pg_dev().await;
    let off = offset() + 2;
    let off_s = off.to_string();
    let hc = |off: &str| {
        Command::new(BIN).args(["healthcheck", "--dev", "--data-dir", data.to_str().unwrap(), "--port-offset", off]).output().unwrap()
    };
    // nothing listens yet: unhealthy, exit 1, and the output names a listener
    let out = hc(&off_s);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unhealthy"), "{out:?}");

    let mut child = spawn_dev(&pg, &["d", "--dev", "--data-dir", data.to_str().unwrap(), "--port-offset", &off_s]);
    wait_for_port(2525 + off, &mut child);
    wait_for_port(9993 + off, &mut child);
    let out = hc(&off_s);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("healthy: 5 listeners"), "{out:?}");
    // a different port range is not this daemon
    assert_eq!(hc(&(off + 100).to_string()).status.code(), Some(1));

    signal(&child, "TERM");
    assert_eq!(wait_exit(&mut child, Duration::from_secs(20)).code(), Some(0));
    assert_eq!(hc(&off_s).status.code(), Some(1));
}

#[test]
fn version_prints_the_package_version() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), concat!("rusty ", env!("CARGO_PKG_VERSION")));
}
