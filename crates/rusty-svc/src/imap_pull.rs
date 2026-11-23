//! A small IMAP client for the move of a mailbox from another server (docs/design-imap-migration.md): it signs in, lists the folders and reads the messages one by one with their flags and dates. It
//! only reads (`BODY.PEEK`, `EXAMINE`): nothing on the other server is changed or marked as read. What it needs of the other server is plain IMAP4rev1: `LOGIN`, `LIST`, `EXAMINE`, `UID SEARCH`,
//! `UID FETCH`, `LOGOUT`, and `STARTTLS` when asked for.
//!
//! Every failure is a fixed word (`PullError`): the text a server sends is never kept or shown, and every read has a time limit and a size limit.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use rusty_core::Flags;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

/// How the connection to the other server is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// TLS from the first byte (usually port 993).
    Tls,
    /// A plain connection upgraded with `STARTTLS` before the password is sent (usually port 143).
    StartTls,
    /// No encryption at all: the password and the mail cross the network in the clear.
    Plain,
}

impl Security {
    pub fn parse(s: &str) -> Option<Security> {
        match s {
            "tls" => Some(Security::Tls),
            "starttls" => Some(Security::StartTls),
            "plain" => Some(Security::Plain),
            _ => None,
        }
    }
}

/// The other server.
#[derive(Debug, Clone)]
pub struct Remote {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub user: String,
    pub password: String,
}

/// Why a pull stopped, as a fixed word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullError {
    /// The server could not be reached.
    Connect,
    /// The TLS handshake or the certificate failed, or `STARTTLS` is not offered.
    Tls,
    /// The server refused the login.
    Auth,
    /// The server said something that is not IMAP, or refused a command.
    Protocol,
    /// A read took longer than the limit.
    Timeout,
    /// A message is larger than the limit.
    TooLarge,
}

impl PullError {
    pub fn word(self) -> &'static str {
        match self {
            PullError::Connect => "connect",
            PullError::Tls => "tls",
            PullError::Auth => "auth",
            PullError::Protocol => "protocol",
            PullError::Timeout => "timeout",
            PullError::TooLarge => "too_large",
        }
    }
}

/// The longest wait for the server's next line or block.
const READ_LIMIT: Duration = Duration::from_secs(60);
/// The longest line of a response (a line that is not a literal).
const MAX_LINE: usize = 64 * 1024;
/// The largest message that is read (the larger ones are skipped by the caller).
pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// The connection: plain or TLS, one type to read and write.
enum Transport {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for Transport {
    fn poll_read(self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Transport::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Transport::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Transport {
    fn poll_write(self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Transport::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Transport::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Transport::Plain(s) => Pin::new(s).poll_flush(cx),
            Transport::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Transport::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Transport::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// One response line of the server with any literal (`{n}` and n bytes) it carried, in order.
#[derive(Debug, Default, Clone)]
pub struct Line {
    /// The text with every literal replaced by `{}`.
    pub text: String,
    pub literals: Vec<Vec<u8>>,
}

/// A folder of the other server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFolder {
    pub name: String,
    /// The folder's role by its special-use flag (`\Sent`, `\Trash`, `\Drafts`, `\Junk`, `\Archive`) or its name `INBOX`: `inbox`, `sent`, `drafts`, `trash`, `spam`, `archive`.
    pub role: Option<&'static str>,
}

/// A message as read.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub flags: Flags,
    /// The date the other server kept, unix seconds.
    pub date: Option<i64>,
    pub body: Vec<u8>,
}

pub struct Session {
    io: BufReader<Transport>,
    tag: u32,
}

/// A TLS client configuration that trusts the host's certificate store.
pub fn system_tls() -> Result<Arc<rustls::ClientConfig>, PullError> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().map_err(|_| PullError::Tls)?.with_root_certificates(roots).with_no_client_auth();
    Ok(Arc::new(cfg))
}

async fn within<T>(f: impl std::future::Future<Output = std::io::Result<T>>) -> Result<T, PullError> {
    match tokio::time::timeout(READ_LIMIT, f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(_)) => Err(PullError::Protocol),
        Err(_) => Err(PullError::Timeout),
    }
}

async fn tls_over(tcp: TcpStream, host: &str, tls: &Arc<rustls::ClientConfig>) -> Result<TlsStream<TcpStream>, PullError> {
    let name = ServerName::try_from(host.to_owned()).map_err(|_| PullError::Tls)?;
    match tokio::time::timeout(READ_LIMIT, TlsConnector::from(tls.clone()).connect(name, tcp)).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(_)) => Err(PullError::Tls),
        Err(_) => Err(PullError::Timeout),
    }
}

/// A string as an IMAP quoted string; `None` when it has a character that cannot be in one (a line break or a control character).
pub fn quoted(s: &str) -> Option<String> {
    if s.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
}

impl Session {
    /// Connect, protect the connection as asked, read the greeting and sign in.
    pub async fn connect(remote: &Remote, tls: &Arc<rustls::ClientConfig>) -> Result<Session, PullError> {
        let tcp = match tokio::time::timeout(READ_LIMIT, TcpStream::connect((remote.host.as_str(), remote.port))).await {
            Ok(Ok(s)) => s,
            Ok(Err(_)) => return Err(PullError::Connect),
            Err(_) => return Err(PullError::Timeout),
        };
        let transport = match remote.security {
            Security::Tls => Transport::Tls(Box::new(tls_over(tcp, &remote.host, tls).await?)),
            Security::StartTls | Security::Plain => Transport::Plain(tcp),
        };
        let mut s = Session { io: BufReader::new(transport), tag: 0 };
        let greeting = s.line().await?;
        if !greeting.text.starts_with("* OK") && !greeting.text.starts_with("* PREAUTH") {
            return Err(PullError::Protocol);
        }
        if remote.security == Security::StartTls {
            s.command("STARTTLS").await.map_err(|e| if e == PullError::Protocol { PullError::Tls } else { e })?;
            // Nothing may be buffered between the answer and the handshake.
            let Session { io, tag } = s;
            let Transport::Plain(tcp) = io.into_inner() else { return Err(PullError::Tls) };
            s = Session { io: BufReader::new(Transport::Tls(Box::new(tls_over(tcp, &remote.host, tls).await?))), tag };
        }
        let (Some(user), Some(pass)) = (quoted(&remote.user), quoted(&remote.password)) else { return Err(PullError::Auth) };
        s.command(&format!("LOGIN {user} {pass}")).await.map_err(|e| if e == PullError::Protocol { PullError::Auth } else { e })?;
        Ok(s)
    }

    /// One line of the server; a literal in it is read and kept, and the line goes on after it.
    async fn line(&mut self) -> Result<Line, PullError> {
        let mut out = Line::default();
        loop {
            let mut raw = Vec::new();
            let n = within(async { (&mut self.io).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut raw).await }).await?;
            if n == 0 || raw.len() > MAX_LINE || raw.last() != Some(&b'\n') {
                return Err(PullError::Protocol);
            }
            while matches!(raw.last(), Some(b'\n' | b'\r')) {
                raw.pop();
            }
            let text = String::from_utf8_lossy(&raw).into_owned();
            // `... {123}` at the end of a line: 123 bytes follow, then the rest of the line.
            if let Some(size) = literal_size(&text) {
                if size > MAX_MESSAGE {
                    return Err(PullError::TooLarge);
                }
                let mut data = vec![0u8; size];
                within(self.io.read_exact(&mut data)).await?;
                out.text.push_str(&text[..text.rfind('{').unwrap_or(text.len())]);
                out.text.push_str("{}");
                out.literals.push(data);
                continue;
            }
            out.text.push_str(&text);
            return Ok(out);
        }
    }

    /// Send a command and read until its tagged answer: the lines before it, or the word of why it failed.
    pub async fn command(&mut self, cmd: &str) -> Result<Vec<Line>, PullError> {
        self.tag += 1;
        let tag = format!("t{}", self.tag);
        within(async {
            self.io.get_mut().write_all(format!("{tag} {cmd}\r\n").as_bytes()).await?;
            self.io.get_mut().flush().await
        })
        .await?;
        let mut untagged = Vec::new();
        loop {
            let line = self.line().await?;
            if let Some(rest) = line.text.strip_prefix(&format!("{tag} ")) {
                return if rest.starts_with("OK") { Ok(untagged) } else { Err(PullError::Protocol) };
            }
            untagged.push(line);
            if untagged.len() > 2_000_000 {
                return Err(PullError::Protocol);
            }
        }
    }

    /// Every folder that can be opened, with the role its flags or its name give it.
    pub async fn folders(&mut self) -> Result<Vec<RemoteFolder>, PullError> {
        let lines = self.command("LIST \"\" \"*\"").await?;
        Ok(lines.iter().filter_map(|l| parse_list(&l.text)).collect())
    }

    /// Open a folder read-only; how many messages it says it has.
    pub async fn examine(&mut self, name: &str) -> Result<u32, PullError> {
        let q = quoted(name).ok_or(PullError::Protocol)?;
        let lines = self.command(&format!("EXAMINE {q}")).await?;
        Ok(lines.iter().find_map(|l| l.text.strip_prefix("* ").and_then(|r| r.strip_suffix(" EXISTS")).and_then(|n| n.trim().parse().ok())).unwrap_or(0))
    }

    /// The UIDs of the open folder, ascending.
    pub async fn uids(&mut self) -> Result<Vec<u32>, PullError> {
        let lines = self.command("UID SEARCH ALL").await?;
        let mut all: Vec<u32> = lines.iter().filter_map(|l| l.text.strip_prefix("* SEARCH")).flat_map(|r| r.split_whitespace().filter_map(|n| n.parse().ok()).collect::<Vec<u32>>()).collect();
        all.sort_unstable();
        all.dedup();
        Ok(all)
    }

    /// One message with its flags and date, without marking it as read; `None` when the server has no such message any more.
    pub async fn fetch(&mut self, uid: u32) -> Result<Option<Fetched>, PullError> {
        let lines = self.command(&format!("UID FETCH {uid} (FLAGS INTERNALDATE BODY.PEEK[])")).await?;
        for l in lines {
            if !l.text.contains(" FETCH ") {
                continue;
            }
            let Some(body) = l.literals.into_iter().next() else { continue };
            return Ok(Some(Fetched { flags: parse_flags(&l.text), date: parse_internal_date(&l.text), body }));
        }
        Ok(None)
    }

    pub async fn logout(mut self) {
        let _ = self.command("LOGOUT").await;
    }
}

/// `{123}` or `{123+}` at the end of a line: 123.
fn literal_size(line: &str) -> Option<usize> {
    let rest = line.strip_suffix('}')?;
    let open = rest.rfind('{')?;
    rest[open + 1..].trim_end_matches('+').parse().ok()
}

/// `* LIST (\HasNoChildren \Sent) "/" "Sent Items"` into a folder; `None` for a folder that cannot be opened (`\Noselect`, `\NonExistent`) or that has no readable name.
pub fn parse_list(line: &str) -> Option<RemoteFolder> {
    let rest = line.strip_prefix("* LIST ")?;
    let open = rest.find('(')?;
    let close = rest.find(')')?;
    let attrs = rest.get(open + 1..close)?.to_ascii_lowercase();
    if attrs.contains("\\noselect") || attrs.contains("\\nonexistent") {
        return None;
    }
    // After the attributes: the delimiter (a quoted character or NIL), then the name (a quoted string or an atom).
    let after = rest.get(close + 1..)?.trim_start();
    let after = if let Some(r) = after.strip_prefix("NIL") { r } else { after.strip_prefix('"').and_then(|r| r.split_once('"')).map(|(_, r)| r)? };
    let name = after.trim();
    let name = if let Some(q) = name.strip_prefix('"') { unescape(q.strip_suffix('"')?) } else { name.to_owned() };
    if name.is_empty() {
        return None;
    }
    let role = if name.eq_ignore_ascii_case("INBOX") {
        Some("inbox")
    } else if attrs.contains("\\sent") {
        Some("sent")
    } else if attrs.contains("\\drafts") {
        Some("drafts")
    } else if attrs.contains("\\trash") {
        Some("trash")
    } else if attrs.contains("\\junk") {
        Some("spam")
    } else if attrs.contains("\\archive") || attrs.contains("\\all") {
        Some("archive")
    } else {
        None
    };
    Some(RemoteFolder { name, role })
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The flags of a `FETCH` line as ours: `\Seen`, `\Flagged`, `\Answered`, `\Draft`; the others (including `\Deleted`, which the caller treats on its own) are not kept.
pub fn parse_flags(line: &str) -> Flags {
    let mut out = Flags::empty();
    if let Some(start) = line.find("FLAGS (") {
        let after = &line[start + 7..];
        if let Some(end) = after.find(')') {
            for f in after[..end].split_whitespace() {
                match f.to_ascii_lowercase().as_str() {
                    "\\seen" => out |= Flags::SEEN,
                    "\\flagged" => out |= Flags::FLAGGED,
                    "\\answered" => out |= Flags::ANSWERED,
                    "\\draft" => out |= Flags::DRAFT,
                    "\\deleted" => out |= Flags::DELETED,
                    _ => {}
                }
            }
        }
    }
    out
}

/// `INTERNALDATE "17-Jul-1996 02:44:25 -0700"` as unix seconds.
pub fn parse_internal_date(line: &str) -> Option<i64> {
    let start = line.find("INTERNALDATE \"")? + 14;
    let end = line[start..].find('"')? + start;
    let s = line.get(start..end)?.trim();
    let (date, rest) = s.split_once(' ')?;
    let (time, zone) = rest.split_once(' ').unwrap_or((rest, "+0000"));
    let mut d = date.split('-');
    let day: i64 = d.next()?.trim().parse().ok()?;
    let name = d.next()?;
    let month = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"].iter().position(|m| m.eq_ignore_ascii_case(name))? as i64 + 1;
    let year: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let (h, m, sec): (i64, i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    if !(1..=31).contains(&day) || !(0..24).contains(&h) || !(0..60).contains(&m) || !(0..=60).contains(&sec) {
        return None;
    }
    let sign = if zone.starts_with('-') { -1 } else { 1 };
    let z: i64 = zone.get(1..)?.parse().ok()?;
    let offset = sign * ((z / 100) * 3600 + (z % 100) * 60);
    // Days from 1970-01-01 (the civil-from-days algorithm, inverted).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + m * 60 + sec - offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_line_gives_the_name_and_the_role() {
        assert_eq!(parse_list("* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent Items\""), Some(RemoteFolder { name: "Sent Items".into(), role: Some("sent") }));
        assert_eq!(parse_list("* LIST () \".\" INBOX"), Some(RemoteFolder { name: "INBOX".into(), role: Some("inbox") }));
        assert_eq!(parse_list("* LIST (\\Junk) NIL \"Spam\\\"y\""), Some(RemoteFolder { name: "Spam\"y".into(), role: Some("spam") }));
        assert_eq!(parse_list("* LIST (\\Noselect \\HasChildren) \"/\" \"[Gmail]\""), None);
        assert_eq!(parse_list("* LIST (\\Trash) \"/\" Trash"), Some(RemoteFolder { name: "Trash".into(), role: Some("trash") }));
        assert_eq!(parse_list("* OK nothing"), None);
        assert_eq!(parse_list("* LIST (\\Marked) \"/\" \"\""), None);
    }

    #[test]
    fn flags_and_dates_of_a_fetch_line() {
        let l = "* 1 FETCH (UID 5 FLAGS (\\Seen \\Flagged $label1) INTERNALDATE \"17-Jul-1996 02:44:25 -0700\" BODY[] {}";
        assert_eq!(parse_flags(l), Flags::SEEN | Flags::FLAGGED);
        assert_eq!(parse_internal_date(l), Some(837_596_665));
        assert_eq!(parse_internal_date("INTERNALDATE \" 1-Jan-1970 00:00:00 +0000\""), Some(0));
        assert_eq!(parse_internal_date("INTERNALDATE \"29-Feb-2024 12:00:00 +0100\""), Some(1_709_204_400));
        assert_eq!(parse_internal_date("INTERNALDATE \"31-Foo-2024 12:00:00 +0100\""), None);
        assert_eq!(parse_flags("* 2 FETCH (UID 6)"), Flags::empty());
    }

    #[test]
    fn a_literal_marker_and_a_quoted_string() {
        assert_eq!(literal_size("* 1 FETCH (BODY[] {123}"), Some(123));
        assert_eq!(literal_size("* 1 FETCH (BODY[] {45+}"), Some(45));
        assert_eq!(literal_size("* OK done"), None);
        assert_eq!(quoted("a\"b\\c").as_deref(), Some("\"a\\\"b\\\\c\""));
        assert_eq!(quoted("line\nbreak"), None);
    }
}
