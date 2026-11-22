//! The certificates page (docs/design-certificates.md): what a client connecting to the public names of this server is handed, read by connecting the way a client does. The page shows the
//! subject, the issuer, the validity and the days left, so an expiring or wrong certificate is seen before the users see it. A platform administrator only.
//!
//! The probe does not verify the chain (it asks what is presented, not whether this server trusts it); it reads the leaf certificate with a small DER reader instead of a certificate crate,
//! because it needs four fields and nothing else.

use std::sync::Arc;
use std::time::Duration;

use rusty_http::{handler, Method, Request, Response, Router};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde::Serialize;
use tokio::net::TcpStream;

use crate::api::AdminApp;

const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// What the page needs of a certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    pub subject: String,
    pub issuer: String,
    /// Seconds since the epoch.
    pub not_before: i64,
    pub not_after: i64,
}

// ------------------------------------------------------------------------------------------------ DER

/// One element: its tag, its content and what follows it.
fn tlv(buf: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *buf.first()?;
    let first = *buf.get(1)?;
    let (len, header) = if first < 0x80 {
        (usize::from(first), 2)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for b in buf.get(2..2 + n)? {
            len = len.checked_mul(256)?.checked_add(usize::from(*b))?;
        }
        (len, 2 + n)
    };
    let end = header.checked_add(len)?;
    Some((tag, buf.get(header..end)?, buf.get(end..)?))
}

/// The common name of a Name (a sequence of sets of attribute type and value), or the first value when there is no common name.
fn name_text(name: &[u8]) -> String {
    let (mut cn, mut any): (Option<String>, Option<String>) = (None, None);
    let mut sets = name;
    while let Some((_, set, rest)) = tlv(sets) {
        sets = rest;
        let mut attrs = set;
        while let Some((_, attr, more)) = tlv(attrs) {
            attrs = more;
            if let Some((_, oid, value)) = tlv(attr) {
                if let Some((_, text, _)) = tlv(value) {
                    let text = String::from_utf8_lossy(text).into_owned();
                    if oid == [0x55, 0x04, 0x03] {
                        cn = Some(text);
                    } else if any.is_none() {
                        any = Some(text);
                    }
                }
            }
        }
    }
    cn.or(any).unwrap_or_default()
}

/// Days from 1970-01-01 to a civil date (the proleptic Gregorian calendar).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A UTCTime (`YYMMDDHHMMSSZ`) or a GeneralizedTime (`YYYYMMDDHHMMSSZ`) in seconds since the epoch.
fn time_of(tag: u8, text: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(text).ok()?.strip_suffix('Z')?;
    let (year, rest) = match tag {
        0x17 if s.len() == 12 => {
            let yy: i64 = s.get(..2)?.parse().ok()?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, s.get(2..)?)
        }
        0x18 if s.len() == 14 => (s.get(..4)?.parse().ok()?, s.get(4..)?),
        _ => return None,
    };
    let n = |i: usize| -> Option<i64> { rest.get(i..i + 2)?.parse().ok() };
    let (mo, d, h, mi, sec) = (n(0)?, n(2)?, n(4)?, n(6)?, n(8)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    Some(days_from_civil(year, mo, d) * 86_400 + h * 3600 + mi * 60 + sec)
}

/// The subject, issuer and validity of a DER certificate; `None` for anything that is not shaped like one.
pub fn parse_cert(der: &[u8]) -> Option<CertInfo> {
    let (_, cert, _) = tlv(der)?;
    let (_, tbs, _) = tlv(cert)?;
    let mut rest = tbs;
    let (mut tag, mut item, mut after) = tlv(rest)?;
    if tag == 0xA0 {
        // The version.
        rest = after;
        (tag, item, after) = tlv(rest)?;
    }
    let _ = (tag, item); // the serial number
    let (_, _, rest) = tlv(after)?; // the signature algorithm
    let (_, issuer, rest) = tlv(rest)?;
    let (_, validity, rest) = tlv(rest)?;
    let (_, subject, _) = tlv(rest)?;
    let (t1, nb, more) = tlv(validity)?;
    let (t2, na, _) = tlv(more)?;
    Some(CertInfo { subject: name_text(subject), issuer: name_text(issuer), not_before: time_of(t1, nb)?, not_after: time_of(t2, na)? })
}

// ------------------------------------------------------------------------------------------------ the probe

/// Accepts whatever is presented: the probe asks what a client is handed, not whether it is trusted here.
#[derive(Debug)]
struct TakeAny(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for TakeAny {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, s: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, s, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, s: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, s, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Connect to `connect` (`host:port`) asking for the name `server_name` and read the leaf certificate that is presented.
pub async fn probe(connect: &str, server_name: &str) -> Result<CertInfo, String> {
    let work = async {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(TakeAny(provider)))
            .with_no_client_auth();
        let name = ServerName::try_from(server_name.to_owned()).map_err(|_| "not a valid host name".to_owned())?;
        let tcp = TcpStream::connect(connect).await.map_err(|e| format!("cannot connect: {e}"))?;
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp).await.map_err(|e| format!("TLS failed: {e}"))?;
        let (_, conn) = tls.get_ref();
        let leaf = conn.peer_certificates().and_then(|c| c.first()).ok_or_else(|| "no certificate was presented".to_owned())?;
        parse_cert(leaf.as_ref()).ok_or_else(|| "the certificate could not be read".to_owned())
    };
    tokio::time::timeout(PROBE_TIMEOUT, work).await.unwrap_or_else(|_| Err("timed out".to_owned()))
}

// ------------------------------------------------------------------------------------------------ the endpoint

#[derive(Serialize)]
struct CertJson {
    host: String,
    port: u16,
    /// What the server is used for at this address: `web`, `imap`, `pop3` or `submission`.
    purpose: &'static str,
    subject: Option<String>,
    issuer: Option<String>,
    not_before: Option<i64>,
    not_after: Option<i64>,
    /// Whole days from now to the end of the validity (negative: expired).
    days_left: Option<i64>,
    /// Why the probe failed, when it did.
    error: Option<String>,
}

#[derive(Serialize)]
struct CertList {
    items: Vec<CertJson>,
}

/// The public names and ports a client uses: the admin web's host, and the mail host's implicit-TLS ports.
fn targets(app: &AdminApp) -> Vec<(String, u16, &'static str)> {
    let mut out: Vec<(String, u16, &'static str)> = Vec::new();
    if let Some(rest) = app.public_origin.strip_prefix("https://") {
        let authority = rest.split('/').next().unwrap_or_default();
        match authority.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => out.push((h.to_owned(), p.parse().unwrap_or(443), "web")),
            _ => out.push((authority.to_owned(), 443, "web")),
        }
    }
    if let Some(m) = app.services.as_ref().and_then(|s| s.mail.as_ref()) {
        for (port, purpose) in [(993, "imap"), (995, "pop3"), (465, "submission")] {
            out.push((m.host.clone(), port, purpose));
        }
    }
    out
}

async fn list(app: &AdminApp, req: Request) -> Response {
    let (_, p) = match app.require(&req, false).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    if !p.scope.is_platform() {
        return Response::error(403, "forbidden").with_header("cache-control", "no-store");
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let probes = targets(app).into_iter().map(|(host, port, purpose)| async move {
        let found = probe(&format!("{host}:{port}"), &host).await;
        match found {
            Ok(c) => CertJson { host, port, purpose, subject: Some(c.subject), issuer: Some(c.issuer), not_before: Some(c.not_before), not_after: Some(c.not_after), days_left: Some((c.not_after - now).div_euclid(86_400)), error: None },
            Err(e) => CertJson { host, port, purpose, subject: None, issuer: None, not_before: None, not_after: None, days_left: None, error: Some(e) },
        }
    });
    // All of them at once: a silent host costs one timeout, not one per address.
    let mut items = Vec::new();
    let mut set = tokio::task::JoinSet::new();
    for (i, f) in probes.enumerate() {
        set.spawn(async move { (i, f.await) });
    }
    while let Some(Ok(done)) = set.join_next().await {
        items.push(done);
    }
    items.sort_by_key(|(i, _)| *i);
    Response::json(200, &CertList { items: items.into_iter().map(|(_, c)| c).collect() }).with_header("cache-control", "no-store")
}

pub fn routes(app: Arc<AdminApp>, router: Router) -> Router {
    router.route(Method::Get, "/api/v1/certificates", handler(move |req| {
        let app = app.clone();
        async move { list(&app, req).await }
    }))
}

#[cfg(test)]
mod tests {
    use super::{days_from_civil, parse_cert, probe, time_of};

    fn make(cn: &str, san: &str) -> (Vec<u8>, rcgen::KeyPair) {
        let mut params = rcgen::CertificateParams::new(vec![san.to_owned()]).unwrap();
        params.distinguished_name.push(rcgen::DnType::CommonName, cn);
        params.not_before = rcgen::date_time_ymd(2026, 1, 2);
        params.not_after = rcgen::date_time_ymd(2026, 4, 5);
        let key = rcgen::KeyPair::generate().unwrap();
        (params.self_signed(&key).unwrap().der().to_vec(), key)
    }

    #[test]
    fn the_dates_of_the_civil_calendar_count_from_the_epoch() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(time_of(0x17, b"260102000000Z"), Some(days_from_civil(2026, 1, 2) * 86_400));
        assert_eq!(time_of(0x18, b"20260405123000Z"), Some(days_from_civil(2026, 4, 5) * 86_400 + 12 * 3600 + 30 * 60));
        assert_eq!(time_of(0x17, b"261302000000Z"), None, "month 13");
        assert_eq!(time_of(0x17, b"2601020000Z"), None, "short");
    }

    #[test]
    fn the_subject_issuer_and_validity_are_read_from_a_certificate() {
        let (der, _) = make("mail.one.test", "mail.one.test");
        let info = parse_cert(&der).expect("a certificate");
        assert_eq!(info.subject, "mail.one.test");
        assert_eq!(info.issuer, "mail.one.test", "self-signed");
        assert_eq!(info.not_before, days_from_civil(2026, 1, 2) * 86_400);
        assert_eq!(info.not_after, days_from_civil(2026, 4, 5) * 86_400);
        assert!(parse_cert(b"not a certificate").is_none());
        assert!(parse_cert(&der[..der.len() / 2]).is_none(), "cut short");
    }

    #[tokio::test]
    async fn a_server_is_probed_the_way_a_client_connects_and_a_dead_port_is_an_error_text() {
        let (der, key) = make("probe.test", "probe.test");
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![rustls::pki_types::CertificateDer::from(der)], rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap())
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((tcp, _)) = listener.accept().await {
                let _ = acceptor.accept(tcp).await;
            }
        });
        let got = probe(&addr.to_string(), "probe.test").await.expect("the certificate is read");
        assert_eq!(got.subject, "probe.test");
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);
        assert!(probe(&dead_addr.to_string(), "probe.test").await.unwrap_err().starts_with("cannot connect"));
    }
}
