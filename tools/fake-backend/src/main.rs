#![forbid(unsafe_code)]

use std::env;
use std::fs::File;
use std::io::BufReader;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{RootCertStore, ServerConfig};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

const PROXY_V2: [u8; 12] = [
    0x0d, 0x0a, 0x0d, 0x0a, 0x00, 0x0d, 0x0a, 0x51, 0x55, 0x49, 0x54, 0x0a,
];

#[tokio::main]
async fn main() {
    if let Err(message) = run().await {
        eprintln!("fake-backend: {message}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), &'static str> {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("health") if args.len() == 3 => {
            tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(&args[2]))
                .await
                .map_err(|_| "health timeout")?
                .map_err(|_| "health connect failed")?;
            Ok(())
        }
        Some("mail") if args.len() == 6 => {
            let starttls = serve("starttls", &args[2], args.get(4));
            let implicit = serve("tls-echo", &args[3], args.get(5));
            tokio::try_join!(starttls, implicit).map(|_| ())
        }
        Some("serve") if args.len() >= 4 => serve(&args[2], &args[3], args.get(4)).await,
        _ => Err(
            "usage: fake-backend health <addr> | serve <mode> <addr> [banner] | mail <starttls-addr> <tls-addr> <starttls-banner> <tls-banner>",
        ),
    }
}

async fn serve(mode: &str, addr: &str, banner: Option<&String>) -> Result<(), &'static str> {
    let listener = TcpListener::bind(addr).await.map_err(|_| "bind failed")?;
    let tls = if matches!(mode, "tls-echo" | "http-whoami") {
        Some(TlsAcceptor::from(Arc::new(load_tls(true)?)))
    } else if mode == "starttls" {
        Some(TlsAcceptor::from(Arc::new(load_tls(false)?)))
    } else {
        None
    };
    loop {
        let (stream, _) = listener.accept().await.map_err(|_| "accept failed")?;
        let mode = mode.to_owned();
        let banner = banner.cloned();
        let tls = tls.clone();
        tokio::spawn(async move {
            let _ = handle(stream, &mode, banner.as_deref(), tls).await;
        });
    }
}

async fn handle(
    mut stream: TcpStream,
    mode: &str,
    banner: Option<&str>,
    tls: Option<TlsAcceptor>,
) -> Result<(), &'static str> {
    match mode {
        "tcp-echo" => {
            let _ = optional_proxy_header(&mut stream).await?;
            echo(stream).await
        }
        "banner" => {
            let _ = required_proxy_header(&mut stream).await?;
            stream
                .write_all(banner.unwrap_or("220 fake ready\r\n").as_bytes())
                .await
                .map_err(|_| "write failed")?;
            echo(stream).await
        }
        "proxy-v2-report" => {
            let source = required_proxy_header(&mut stream).await?;
            stream
                .write_all(format!("{source}\n").as_bytes())
                .await
                .map_err(|_| "write failed")
        }
        "starttls" => starttls(stream, banner, tls.ok_or("TLS unavailable")?).await,
        "tls-echo" => {
            let _ = required_proxy_header(&mut stream).await?;
            let tls_stream = tls
                .ok_or("TLS unavailable")?
                .accept(stream)
                .await
                .map_err(|_| "TLS handshake failed")?;
            echo(tls_stream).await
        }
        "http-whoami" => {
            let source = required_proxy_header(&mut stream).await?;
            let mut tls_stream = tls
                .ok_or("TLS unavailable")?
                .accept(stream)
                .await
                .map_err(|_| "TLS handshake failed")?;
            let mut request = vec![0u8; 16 * 1024];
            let _ = tls_stream
                .read(&mut request)
                .await
                .map_err(|_| "read failed")?;
            let body = format!("source={source}\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            tls_stream
                .write_all(response.as_bytes())
                .await
                .map_err(|_| "write failed")
        }
        _ => Err("unknown mode"),
    }
}

async fn starttls(
    mut stream: TcpStream,
    banner: Option<&str>,
    tls: TlsAcceptor,
) -> Result<(), &'static str> {
    let _ = required_proxy_header(&mut stream).await?;
    stream
        .write_all(banner.unwrap_or("220 fake ready\r\n").as_bytes())
        .await
        .map_err(|_| "write failed")?;
    let mut line = Vec::with_capacity(128);
    loop {
        let byte = stream.read_u8().await.map_err(|_| "read failed")?;
        if line.len() == 1024 {
            return Err("command too long");
        }
        line.push(byte);
        if line.ends_with(b"\n") {
            if String::from_utf8_lossy(&line)
                .trim()
                .eq_ignore_ascii_case("STARTTLS")
            {
                break;
            }
            line.clear();
        }
    }
    stream
        .write_all(b"220 begin TLS\r\n")
        .await
        .map_err(|_| "write failed")?;
    let tls_stream = tls
        .accept(stream)
        .await
        .map_err(|_| "TLS handshake failed")?;
    echo(tls_stream).await
}

async fn echo<S>(mut stream: S) -> Result<(), &'static str>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = stream.read(&mut buffer).await.map_err(|_| "read failed")?;
        if read == 0 {
            return Ok(());
        }
        stream
            .write_all(&buffer[..read])
            .await
            .map_err(|_| "write failed")?;
    }
}

async fn optional_proxy_header(stream: &mut TcpStream) -> Result<Option<String>, &'static str> {
    let mut prefix = [0u8; 12];
    let read = stream.peek(&mut prefix).await.map_err(|_| "peek failed")?;
    if read >= prefix.len() && prefix == PROXY_V2 {
        required_proxy_header(stream).await.map(Some)
    } else {
        Ok(None)
    }
}

async fn required_proxy_header<S>(stream: &mut S) -> Result<String, &'static str>
where
    S: AsyncRead + Unpin,
{
    let mut header = [0u8; 16];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|_| "missing PROXY v2 header")?;
    if header[..12] != PROXY_V2 || header[12] >> 4 != 2 {
        return Err("invalid PROXY v2 header");
    }
    let len = u16::from_be_bytes([header[14], header[15]]) as usize;
    if len > 512 {
        return Err("PROXY v2 header too large");
    }
    let mut address = vec![0u8; len];
    stream
        .read_exact(&mut address)
        .await
        .map_err(|_| "short PROXY v2 header")?;
    match header[13] {
        0x11 if address.len() >= 12 => {
            Ok(Ipv4Addr::new(address[0], address[1], address[2], address[3]).to_string())
        }
        0x21 if address.len() >= 36 => {
            let octets: [u8; 16] = address[..16].try_into().map_err(|_| "bad IPv6 address")?;
            Ok(Ipv6Addr::from(octets).to_string())
        }
        0x00 => Ok("local".to_owned()),
        _ => Err("unsupported PROXY v2 family"),
    }
}

fn load_tls(require_client: bool) -> Result<ServerConfig, &'static str> {
    let cert_path = env::var("RUSTY_CERT").map_err(|_| "RUSTY_CERT missing")?;
    let key_path = env::var("RUSTY_KEY").map_err(|_| "RUSTY_KEY missing")?;
    let certs = load_certs(&cert_path)?;
    let key = load_key(&key_path)?;
    if require_client {
        let ca_path = env::var("RUSTY_CLIENT_CA").map_err(|_| "RUSTY_CLIENT_CA missing")?;
        let mut roots = RootCertStore::empty();
        for cert in load_certs(&ca_path)? {
            roots.add(cert).map_err(|_| "invalid client CA")?;
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|_| "client verifier failed")?;
        ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)
            .map_err(|_| "invalid server certificate")
    } else {
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|_| "invalid server certificate")
    }
}

fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>, &'static str> {
    let file = File::open(path).map_err(|_| "certificate open failed")?;
    rustls_pemfile::certs(&mut BufReader::new(file))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "certificate parse failed")
}

fn load_key(path: &str) -> Result<PrivateKeyDer<'static>, &'static str> {
    let file = File::open(path).map_err(|_| "key open failed")?;
    rustls_pemfile::private_key(&mut BufReader::new(file))
        .map_err(|_| "key parse failed")?
        .ok_or("private key missing")
}
