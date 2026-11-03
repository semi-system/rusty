//! Harness for the edge behaviour suite. Product-agnostic: everything that describes the
//! edge implementation lives in `EdgeUnderTest`; the cases only talk to published ports,
//! the container runtime and the fake backends.
#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, StreamOwned, SupportedProtocolVersion,
};

/// Published ports of the edge, as the clients see them.
#[derive(Clone, Copy)]
pub struct Ports {
    pub smtp: u16,
    pub submission: u16,
    pub imap: u16,
    pub pop3: u16,
    pub smtps: u16,
    pub imaps: u16,
    pub pop3s: u16,
    pub http: u16,
    pub https: u16,
}

/// Description of the edge under test (HAProxy today; another implementation can reuse the
/// cases by providing its own description and compose files).
pub struct EdgeUnderTest {
    pub name: String,
    pub repo: PathBuf,
    pub compose_dir: PathBuf,
    pub compose_file: PathBuf,
    pub project: String,
    pub edge_service: String,
    pub host: String,
    pub ports: Ports,
    /// Internal listening port of the edge on `edge-net` (for direct container access).
    pub edge_internal_imap: u16,
    pub generated: PathBuf,
    pub report_path: PathBuf,
    /// Idle bound the configuration promises for pass-through connections.
    pub passthrough_idle: Duration,
    /// Client inactivity / handshake bound the configuration promises on terminated ports.
    pub terminated_idle: Duration,
}

impl EdgeUnderTest {
    #[must_use]
    pub fn haproxy() -> Self {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let repo = repo.canonicalize().unwrap_or(repo);
        let compose_dir = repo.join("deploy/compose");
        let generated = compose_dir.join("generated");
        Self {
            name: "haproxy-3.4.7 (option A)".to_owned(),
            compose_file: compose_dir.join("compose.yaml"),
            report_path: std::env::var_os("EDGE_REPORT")
                .map_or_else(|| generated.join("edge-report.tsv"), PathBuf::from),
            repo,
            compose_dir,
            project: "rusty-edge-h2".to_owned(),
            edge_service: "edge".to_owned(),
            host: "127.0.0.1".to_owned(),
            ports: Ports {
                smtp: 25,
                submission: 587,
                imap: 143,
                pop3: 110,
                smtps: 465,
                imaps: 993,
                pop3s: 995,
                http: 80,
                https: 443,
            },
            edge_internal_imap: 10143,
            generated,
            passthrough_idle: Duration::from_secs(300),
            terminated_idle: Duration::from_secs(30),
        }
    }

    // ---- processes -------------------------------------------------------------------

    pub fn compose_with(&self, extra: &[&Path], args: &[&str]) -> CmdOut {
        let mut c = Command::new("docker");
        c.current_dir(&self.compose_dir)
            .args(["compose", "-p", &self.project, "-f"])
            .arg(&self.compose_file);
        for f in extra {
            c.arg("-f").arg(f);
        }
        c.args(args);
        run(&mut c)
    }

    pub fn compose(&self, args: &[&str]) -> CmdOut {
        self.compose_with(&[], args)
    }

    /// Regenerate throw-away certificates and start the whole project.
    pub fn up(&self) -> Result<(), String> {
        let gen = run(Command::new("bash")
            .current_dir(&self.repo)
            .arg("scripts/gen-test-certs.sh"));
        gen.require("gen-test-certs")?;
        self.compose(&["up", "--build", "--wait", "-d"])
            .require("compose up")
    }

    pub fn down(&self) {
        let _ = self.compose(&["down", "-v", "--remove-orphans", "-t", "2"]);
        let _ = run(Command::new("docker").args(["rm", "-f", &self.probe_name()]));
    }

    pub fn container(&self, service: &str) -> Result<String, String> {
        let out = self.compose(&["ps", "-q", service]);
        let id = out.stdout.trim().to_owned();
        if id.is_empty() {
            Err(format!("no container for {service}: {}", out.stderr))
        } else {
            Ok(id)
        }
    }

    pub fn project_containers(&self) -> Vec<String> {
        self.compose(&["ps", "-a", "-q"])
            .stdout
            .lines()
            .map(str::to_owned)
            .collect()
    }

    pub fn edge_logs(&self) -> String {
        let id = self.container(&self.edge_service).unwrap_or_default();
        let o = run(Command::new("docker").args(["logs", &id]));
        format!("{}{}", o.stdout, o.stderr)
    }

    pub fn edge_ip(&self) -> Result<String, String> {
        let id = self.container(&self.edge_service)?;
        let fmt = format!(
            "{{{{(index .NetworkSettings.Networks \"{}_edge-net\").IPAddress}}}}",
            self.project
        );
        Ok(run(Command::new("docker").args(["inspect", "-f", &fmt, &id]))
            .stdout
            .trim()
            .to_owned())
    }

    /// Replace one backend with a variant described by a compose override, wait until healthy.
    pub fn recreate_backend(&self, service: &str, override_yaml: &str) -> Result<(), String> {
        let path = self.generated.join(format!("override-{service}.yaml"));
        let _ = fs::remove_file(&path);
        fs::write(&path, override_yaml).map_err(|e| e.to_string())?;
        self.compose_with(
            &[&path],
            &["up", "-d", "--no-deps", "--force-recreate", "--wait", service],
        )
        .require("recreate backend")
    }

    pub fn restore_backend(&self, service: &str) -> Result<(), String> {
        self.compose(&["up", "-d", "--no-deps", "--force-recreate", "--wait", service])
            .require("restore backend")
    }

    pub fn addr(&self, port: u16) -> SocketAddr {
        format!("{}:{port}", self.host).parse().expect("addr")
    }

    pub fn connect(&self, port: u16) -> io::Result<TcpStream> {
        let s = TcpStream::connect_timeout(&self.addr(port), Duration::from_secs(5))?;
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        s.set_write_timeout(Some(Duration::from_secs(10)))?;
        Ok(s)
    }

    pub fn ca_path(&self) -> PathBuf {
        self.generated.join("internal-ca.crt")
    }

    pub fn cert_der(&self, file: &str) -> Vec<CertificateDer<'static>> {
        CertificateDer::pem_file_iter(self.generated.join(file))
            .expect("open cert")
            .collect::<Result<Vec<_>, _>>()
            .expect("parse cert")
    }

    pub fn client_config(&self, versions: &[&'static SupportedProtocolVersion]) -> Arc<ClientConfig> {
        let mut roots = RootCertStore::empty();
        for c in self.cert_der("internal-ca.crt") {
            roots.add(c).expect("root");
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Arc::new(
            ClientConfig::builder_with_provider(provider)
                .with_protocol_versions(versions)
                .expect("versions")
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }

    pub fn tls_all(&self) -> Arc<ClientConfig> {
        self.client_config(rustls::ALL_VERSIONS)
    }

    /// Complete a verified TLS handshake over `tcp` for `name`.
    pub fn tls_wrap(
        &self,
        tcp: TcpStream,
        name: &str,
        cfg: Arc<ClientConfig>,
    ) -> Result<StreamOwned<ClientConnection, TcpStream>, String> {
        let server_name = ServerName::try_from(name.to_owned()).map_err(|e| e.to_string())?;
        let mut conn = ClientConnection::new(cfg, server_name).map_err(|e| e.to_string())?;
        let mut tcp = tcp;
        while conn.is_handshaking() {
            conn.complete_io(&mut tcp).map_err(|e| format!("handshake: {e}"))?;
        }
        Ok(StreamOwned::new(conn, tcp))
    }

    pub fn tls_connect(
        &self,
        port: u16,
        name: &str,
    ) -> Result<StreamOwned<ClientConnection, TcpStream>, String> {
        let tcp = self.connect(port).map_err(|e| format!("connect {port}: {e}"))?;
        self.tls_wrap(tcp, name, self.tls_all())
    }

    /// `openssl s_client` against a published port. Returns combined output.
    pub fn s_client(&self, port: u16, args: &[&str]) -> String {
        let mut c = Command::new("timeout");
        c.args(["20", "openssl", "s_client", "-connect"])
            .arg(format!("{}:{port}", self.host))
            .args(["-CAfile"])
            .arg(self.ca_path())
            .args(args)
            .stdin(Stdio::null());
        let o = run(&mut c);
        format!("{}{}", o.stdout, o.stderr)
    }

    /// Which certificate does `port` present for `sni` (None = no SNI)? Returns the SHA-256
    /// fingerprint, or Err(alert/handshake text).
    pub fn presented_cert(&self, port: u16, sni: Option<&str>) -> Result<String, String> {
        let mut args = vec!["-showcerts"];
        let name;
        match sni {
            Some(n) => {
                name = n.to_owned();
                args.extend(["-servername", &name]);
            }
            None => args.push("-noservername"),
        }
        let out = self.s_client(port, &args);
        extract_first_cert(&out)
            .map(|pem| fingerprint_pem(&pem))
            .ok_or_else(|| summarize_failure(&out))
    }

    pub fn file_fingerprint(&self, file: &str) -> String {
        let pem = fs::read_to_string(self.generated.join(file)).expect("read");
        // .pem files hold key then cert; the first CERTIFICATE block is the leaf.
        fingerprint_pem(&extract_first_cert(&pem).expect("cert in file"))
    }

    pub fn probe_name(&self) -> String {
        format!("{}-probe", self.project)
    }

    pub fn start_probe(&self) -> Result<Probe, String> {
        let image = run(Command::new("docker").args([
            "inspect",
            "-f",
            "{{.Image}}",
            &self.container(&self.edge_service)?,
        ]))
        .stdout
        .trim()
        .to_owned();
        let name = self.probe_name();
        let _ = run(Command::new("docker").args(["rm", "-f", &name]));
        run(Command::new("docker").args([
            "run",
            "-d",
            "--name",
            &name,
            "--network",
            &format!("{}_edge-net", self.project),
            "--entrypoint",
            "sleep",
            &image,
            "3600",
        ]))
        .require("start probe")?;
        let ip = run(Command::new("docker").args([
            "inspect",
            "-f",
            &format!(
                "{{{{(index .NetworkSettings.Networks \"{}_edge-net\").IPAddress}}}}",
                self.project
            ),
            &name,
        ]))
        .stdout
        .trim()
        .to_owned();
        Ok(Probe { name, ip })
    }

    pub fn reload_edge(&self) -> Result<(), String> {
        let id = self.container(&self.edge_service)?;
        run(Command::new("docker").args(["kill", "-s", "USR2", &id])).require("reload signal")
    }

    pub fn wait_edge_healthy(&self, timeout: Duration) -> bool {
        poll(timeout, || {
            let Ok(id) = self.container(&self.edge_service) else {
                return false;
            };
            run(Command::new("docker").args([
                "inspect",
                "-f",
                "{{.State.Health.Status}}",
                &id,
            ]))
            .stdout
            .trim()
                == "healthy"
        })
    }
}

/// A throw-away client container on `edge-net` with its own IP address.
pub struct Probe {
    pub name: String,
    pub ip: String,
}

impl Probe {
    /// Connect to `edge:<internal port>` and return what the server sends first.
    #[must_use]
    pub fn banner(&self, internal_port: u16) -> String {
        run(Command::new("docker").args([
            "exec",
            &self.name,
            "sh",
            "-c",
            &format!("(sleep 1; echo) | nc -w 3 edge {internal_port}"),
        ]))
        .stdout
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = run(Command::new("docker").args(["rm", "-f", &self.name]));
    }
}

// ---- generic helpers -----------------------------------------------------------------

pub struct CmdOut {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOut {
    pub fn require(&self, what: &str) -> Result<(), String> {
        if self.ok {
            Ok(())
        } else {
            Err(format!("{what} failed: {} {}", self.stdout, self.stderr))
        }
    }
}

pub fn run(c: &mut Command) -> CmdOut {
    match c.output() {
        Ok(o) => CmdOut {
            ok: o.status.success(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => CmdOut {
            ok: false,
            stdout: String::new(),
            stderr: e.to_string(),
        },
    }
}

pub fn poll(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

pub fn read_line(s: &mut impl Read) -> io::Result<String> {
    let mut buf = Vec::new();
    let mut one = [0u8; 1];
    while s.read(&mut one)? == 1 {
        buf.push(one[0]);
        if one[0] == b'\n' || buf.len() > 4096 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Banner convention of the fake backends: the last token is the PROXY v2 source address.
#[must_use]
pub fn banner_source(banner: &str) -> Option<String> {
    banner.split_whitespace().last().map(str::to_owned)
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut f = fs::File::open("/dev/urandom").expect("urandom");
    let mut v = vec![0u8; n];
    f.read_exact(&mut v).expect("read urandom");
    v
}

fn extract_first_cert(text: &str) -> Option<String> {
    let start = text.find("-----BEGIN CERTIFICATE-----")?;
    let end = text[start..].find("-----END CERTIFICATE-----")? + start;
    Some(format!("{}-----END CERTIFICATE-----\n", &text[start..end]))
}

fn fingerprint_pem(pem: &str) -> String {
    use std::process::Stdio;
    let mut child = Command::new("openssl")
        .args(["x509", "-noout", "-fingerprint", "-sha256"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("openssl x509");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(pem.as_bytes())
        .expect("write pem");
    let out = child.wait_with_output().expect("wait");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn summarize_failure(out: &str) -> String {
    let mut s = String::new();
    for l in out.lines() {
        let low = l.to_ascii_lowercase();
        if low.contains("alert")
            || low.contains("error")
            || low.contains("handshake failure")
            || low.contains("no peer certificate")
            || low.contains("connect:")
        {
            let _ = write!(s, "{} | ", l.trim());
        }
    }
    if s.is_empty() {
        out.lines().take(3).collect::<Vec<_>>().join(" | ")
    } else {
        s
    }
}

/// Minimal TLS 1.2 ClientHello with an arbitrary SNI byte string (for the SNI abuse test).
#[must_use]
pub fn client_hello(sni: &[u8]) -> Vec<u8> {
    fn ext(out: &mut Vec<u8>, id: u16, data: &[u8]) {
        out.extend(id.to_be_bytes());
        out.extend((data.len() as u16).to_be_bytes());
        out.extend(data);
    }
    let mut list = vec![0u8];
    list.extend((sni.len() as u16).to_be_bytes());
    list.extend(sni);
    let mut sni_ext = (list.len() as u16).to_be_bytes().to_vec();
    sni_ext.extend(list);
    let mut exts = Vec::new();
    ext(&mut exts, 0x0000, &sni_ext);
    ext(&mut exts, 0x000a, &[0, 4, 0, 0x17, 0, 0x18]);
    ext(&mut exts, 0x000b, &[1, 0]);
    ext(&mut exts, 0x000d, &[0, 2, 4, 3]);
    let mut body = vec![0x03, 0x03];
    body.extend([7u8; 32]);
    body.push(0);
    body.extend([0, 2, 0xc0, 0x2b]);
    body.extend([1, 0]);
    body.extend((exts.len() as u16).to_be_bytes());
    body.extend(exts);
    let mut hs = vec![1u8];
    hs.extend(&(body.len() as u32).to_be_bytes()[1..]);
    hs.extend(body);
    let mut rec = vec![0x16, 0x03, 0x01];
    rec.extend((hs.len() as u16).to_be_bytes());
    rec.extend(hs);
    rec
}

/// First bytes the server answers to `data`: `Alert(description)`, `ServerHello`, `Closed`, `Other`.
#[derive(Debug, PartialEq, Eq)]
pub enum HelloAnswer {
    Alert(u8),
    ServerHello,
    Closed,
    Other(u8),
}

pub fn send_hello(edge: &EdgeUnderTest, port: u16, data: &[u8]) -> HelloAnswer {
    let Ok(mut s) = edge.connect(port) else {
        return HelloAnswer::Closed;
    };
    if s.write_all(data).is_err() {
        return HelloAnswer::Closed;
    }
    let mut b = [0u8; 7];
    match s.read(&mut b) {
        Ok(n) if n >= 7 && b[0] == 0x15 => HelloAnswer::Alert(b[6]),
        Ok(n) if n >= 6 && b[0] == 0x16 && b[5] == 2 => HelloAnswer::ServerHello,
        Ok(0) | Err(_) => HelloAnswer::Closed,
        Ok(_) => HelloAnswer::Other(b[0]),
    }
}

// ---- reporting -------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    /// Measured information without a pass/fail criterion.
    Info,
    /// A requirement the test topology cannot exercise (reported loudly, never as PASS).
    Gap,
}

impl Status {
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Info => "INFO",
            Status::Gap => "GAP",
        }
    }
}

pub struct Row {
    pub name: String,
    pub status: Status,
    pub detail: String,
    pub secs: f32,
}

pub fn write_report(path: &Path, edge: &str, rows: &[Row]) {
    let mut s = format!("# edge under test: {edge}\n");
    for r in rows {
        let _ = writeln!(
            s,
            "{}\t{}\t{:.1}s\t{}",
            r.name,
            r.status.label(),
            r.secs,
            r.detail.replace(['\n', '\t'], " ")
        );
    }
    let _ = fs::write(path, s);
}
