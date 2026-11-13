//! A scripted remote MTA on a loopback port: the test double of the outbound client. It speaks real
//! SMTP (greeting, EHLO with extensions, STARTTLS over rustls, MAIL, RCPT, DATA with dot-unstuffing,
//! RSET, QUIT) and lets a test decide, per connection, what it answers or whether it hangs, drops or
//! breaks the TLS handshake. It records everything it accepted.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusty_svc::BoxedStream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

/// One accepted message.
#[derive(Debug, Clone)]
pub struct Received {
    pub helo: String,
    pub mail_from_line: String,
    pub from: String,
    pub rcpts: Vec<String>,
    pub rcpt_lines: Vec<String>,
    pub data: Vec<u8>,
    pub tls: bool,
    pub auth: Option<String>,
    pub connection: usize,
}

pub enum DataEnd {
    Reply(String),
    /// Close the connection without answering (the message may have been "accepted").
    Drop,
    /// Never answer.
    Hang,
}

/// What the server does on one connection.
pub struct Script {
    pub greeting: String,
    /// Hang before even sending the greeting.
    pub silent: bool,
    pub extensions: Vec<String>,
    pub tls: Option<Arc<rustls::ServerConfig>>,
    /// Advertise STARTTLS but make the handshake fail (garbage instead of a TLS answer).
    pub break_tls: bool,
    pub require_auth: Option<(String, String)>,
    pub mail: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub rcpt: Box<dyn Fn(&str) -> String + Send + Sync>,
    pub data_end: Box<dyn Fn(&Received) -> DataEnd + Send + Sync>,
    /// Delay before answering the end of DATA.
    pub data_delay: Duration,
}

impl Default for Script {
    fn default() -> Script {
        Script {
            greeting: "220 mx.fake.test ESMTP".into(),
            silent: false,
            extensions: vec!["PIPELINING".into(), "ENHANCEDSTATUSCODES".into()],
            tls: None,
            break_tls: false,
            require_auth: None,
            mail: Box::new(|_| "250 2.1.0 sender ok".into()),
            rcpt: Box::new(|_| "250 2.1.5 recipient ok".into()),
            data_end: Box::new(|_| DataEnd::Reply("250 2.0.0 queued as FAKE".into())),
            data_delay: Duration::ZERO,
        }
    }
}

pub struct FakeMta {
    pub addr: SocketAddr,
    pub received: Arc<Mutex<Vec<Received>>>,
    pub connections: Arc<AtomicUsize>,
    pub max_concurrent: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeMta {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeMta {
    pub async fn start(script_for: impl Fn(usize) -> Script + Send + Sync + 'static) -> FakeMta {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let script_for = Arc::new(script_for);
        let (r, c, cur, mx) = (received.clone(), connections.clone(), current, max_concurrent.clone());
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let n = c.fetch_add(1, Ordering::SeqCst) + 1;
                let script = script_for(n);
                let (r, cur, mx) = (r.clone(), cur.clone(), mx.clone());
                tokio::spawn(async move {
                    let now = cur.fetch_add(1, Ordering::SeqCst) + 1;
                    mx.fetch_max(now, Ordering::SeqCst);
                    session(stream, script, n, r).await;
                    cur.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        FakeMta { addr, received, connections, max_concurrent, task }
    }

    pub fn messages(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    pub fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

async fn say(w: &mut BufReader<BoxedStream>, text: &str) {
    let s = w.get_mut();
    let _ = s.write_all(format!("{text}\r\n").as_bytes()).await;
    let _ = s.flush().await;
}

async fn session(tcp: TcpStream, script: Script, n: usize, received: Arc<Mutex<Vec<Received>>>) {
    let mut io = BufReader::new(Box::new(tcp) as BoxedStream);
    if script.silent {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        return;
    }
    say(&mut io, &script.greeting).await;
    let mut cur = Received { helo: String::new(), mail_from_line: String::new(), from: String::new(), rcpts: vec![], rcpt_lines: vec![], data: vec![], tls: false, auth: None, connection: n };
    let mut tls_on = false;
    let mut authed = script.require_auth.is_none();
    loop {
        let mut line = String::new();
        match io.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let line = line.trim_end().to_owned();
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("EHLO") {
            cur.helo = line.get(5..).unwrap_or_default().to_owned();
            let mut ext = script.extensions.clone();
            if script.tls.is_some() && !tls_on {
                ext.push("STARTTLS".into());
            }
            if script.require_auth.is_some() && tls_on {
                ext.push("AUTH PLAIN".into());
            }
            let mut out = String::from("250-mx.fake.test greets you\r\n");
            for (i, e) in ext.iter().enumerate() {
                let sep = if i + 1 == ext.len() { ' ' } else { '-' };
                out.push_str(&format!("250{sep}{e}\r\n"));
            }
            if ext.is_empty() {
                out = "250 mx.fake.test greets you\r\n".into();
            }
            let _ = io.get_mut().write_all(out.as_bytes()).await;
        } else if upper.starts_with("HELO") {
            say(&mut io, "250 mx.fake.test").await;
        } else if upper == "STARTTLS" {
            if script.break_tls {
                say(&mut io, "220 2.0.0 Ready to start TLS").await;
                let _ = io.get_mut().write_all(b"this is not a TLS server hello\r\n").await;
                return;
            }
            let Some(cfg) = script.tls.clone() else {
                say(&mut io, "502 5.5.1 not offered").await;
                continue;
            };
            say(&mut io, "220 2.0.0 Ready to start TLS").await;
            let raw = io.into_inner();
            match TlsAcceptor::from(cfg).accept(raw).await {
                Ok(t) => {
                    io = BufReader::new(Box::new(t) as BoxedStream);
                    tls_on = true;
                    cur.tls = true;
                }
                Err(_) => return,
            }
        } else if upper.starts_with("AUTH PLAIN") {
            let blob = line.get(11..).unwrap_or_default().trim().to_owned();
            use base64::Engine;
            let decoded = base64::engine::general_purpose::STANDARD.decode(blob.as_bytes()).unwrap_or_default();
            let text = String::from_utf8_lossy(&decoded).replace('\0', "|");
            let want = script.require_auth.as_ref().map(|(u, p)| format!("|{u}|{p}"));
            cur.auth = Some(text.clone());
            if want.as_deref() == Some(text.as_str()) {
                authed = true;
                say(&mut io, "235 2.7.0 authenticated").await;
            } else {
                say(&mut io, "535 5.7.8 bad credentials").await;
            }
        } else if upper.starts_with("MAIL FROM:") {
            if !authed {
                say(&mut io, "530 5.7.0 authentication required").await;
                continue;
            }
            cur.mail_from_line = line.clone();
            cur.from = line.get(10..).unwrap_or_default().split('>').next().unwrap_or_default().trim_start_matches([' ', '<']).to_owned();
            cur.rcpts.clear();
            cur.rcpt_lines.clear();
            let reply = (script.mail)(&line);
            say(&mut io, &reply).await;
        } else if upper.starts_with("RCPT TO:") {
            let addr = line.get(8..).unwrap_or_default().split('>').next().unwrap_or_default().trim_start_matches([' ', '<']).to_owned();
            let reply = (script.rcpt)(&addr);
            if reply.starts_with('2') {
                cur.rcpts.push(addr);
                cur.rcpt_lines.push(line.clone());
            }
            say(&mut io, &reply).await;
        } else if upper == "DATA" {
            if cur.rcpts.is_empty() {
                say(&mut io, "503 5.5.1 no valid recipients").await;
                continue;
            }
            say(&mut io, "354 go ahead").await;
            let mut data = Vec::new();
            loop {
                let mut l = Vec::new();
                match io.read_until(b'\n', &mut l).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if l == b".\r\n" {
                    break;
                }
                if l.starts_with(b"..") {
                    l.remove(0);
                }
                data.extend_from_slice(&l);
            }
            cur.data = data;
            if script.data_delay > Duration::ZERO {
                tokio::time::sleep(script.data_delay).await;
            }
            match (script.data_end)(&cur) {
                DataEnd::Reply(r) => {
                    if r.starts_with('2') {
                        received.lock().unwrap().push(cur.clone());
                    }
                    say(&mut io, &r).await;
                }
                DataEnd::Drop => {
                    // The message was "accepted" in the sense that matters for duplicates.
                    received.lock().unwrap().push(cur.clone());
                    return;
                }
                DataEnd::Hang => {
                    received.lock().unwrap().push(cur.clone());
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    return;
                }
            }
        } else if upper == "RSET" {
            cur.rcpts.clear();
            say(&mut io, "250 2.0.0 reset").await;
        } else if upper == "QUIT" {
            say(&mut io, "221 2.0.0 bye").await;
            return;
        } else {
            say(&mut io, "500 5.5.2 unknown").await;
        }
    }
}

// ---------------------------------------------------------------------------- test PKI

pub struct TestPki {
    pub ca_pem: String,
    pub server: Arc<rustls::ServerConfig>,
    /// A server certificate from a CA the client does not know.
    pub stranger: Arc<rustls::ServerConfig>,
}

fn server_config(cert_pem: &str, key_pem: &str, chain_extra: Option<&str>) -> Arc<rustls::ServerConfig> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let mut certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem.as_bytes()).map(|c| c.unwrap()).collect();
    if let Some(extra) = chain_extra {
        certs.extend(CertificateDer::pem_slice_iter(extra.as_bytes()).map(|c| c.unwrap()));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    Arc::new(cfg)
}

impl TestPki {
    /// A CA, a certificate for `host` signed by it, and a certificate for `host` signed by another CA.
    pub fn new(host: &str) -> TestPki {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
        let make = |host: &str| {
            let ca_key = KeyPair::generate().unwrap();
            let mut ca_params = CertificateParams::new(vec![]).unwrap();
            ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
            ca_params.distinguished_name.push(rcgen::DnType::CommonName, "Rusty test CA");
            let ca = ca_params.self_signed(&ca_key).unwrap();
            let leaf_key = KeyPair::generate().unwrap();
            let issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
            let leaf = CertificateParams::new(vec![host.to_owned()]).unwrap().signed_by(&leaf_key, &issuer).unwrap();
            (ca.pem(), leaf.pem(), leaf_key.serialize_pem())
        };
        let (ca_pem, leaf, key) = make(host);
        let (_, other_leaf, other_key) = make(host);
        TestPki { ca_pem, server: server_config(&leaf, &key, None), stranger: server_config(&other_leaf, &other_key, None) }
    }
}


// ---------------------------------------------------------------------------- dialer double

use std::collections::HashMap;
use std::net::IpAddr;

/// Maps the addresses the resolver returns to the loopback ports of fake servers; an unmapped address
/// refuses the connection. Records every address dialed, in order.
pub struct MapDialer {
    pub map: Mutex<HashMap<IpAddr, SocketAddr>>,
    pub dialed: Mutex<Vec<IpAddr>>,
}

impl MapDialer {
    pub fn new(pairs: &[(&str, SocketAddr)]) -> Arc<MapDialer> {
        Arc::new(MapDialer {
            map: Mutex::new(pairs.iter().map(|(ip, a)| (ip.parse().unwrap(), *a)).collect()),
            dialed: Mutex::new(Vec::new()),
        })
    }
    pub fn dialed(&self) -> Vec<IpAddr> {
        self.dialed.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl rusty_smtp::outbound::dial::Dialer for MapDialer {
    async fn dial(&self, addr: IpAddr, _port: u16, _bind: Option<IpAddr>, limit: Duration) -> std::io::Result<BoxedStream> {
        self.dialed.lock().unwrap().push(addr);
        let target = self.map.lock().unwrap().get(&addr).copied();
        match target {
            None => Err(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "no such host")),
            Some(a) => {
                let s = tokio::time::timeout(limit, TcpStream::connect(a)).await.map_err(|_| std::io::ErrorKind::TimedOut)??;
                Ok(Box::new(s))
            }
        }
    }
}
