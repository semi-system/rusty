//! The POP3 session machine: a line in, bytes out, a decision for the driver. It owns no socket and reads no clock (the timeouts are the driver's); it reaches the mailbox and the password
//! check only through the [`Backend`] it is handed, so the whole protocol is tested without a database.
//!
//! States (RFC 1939): AUTHORIZATION until `PASS` or `AUTH` succeeds, TRANSACTION on the snapshot of the maildrop taken at that moment, then `QUIT` is the UPDATE state: what was marked with
//! `DELE` is removed in one batch. A connection that ends in any other way (`Closed` here is the driver giving up) removes nothing: the session never deletes anything but at `QUIT`.

use async_trait::async_trait;
use bytes::Bytes;

use crate::command::{parse, Command, ParseError};

/// Bad passwords one session may try before it is ended.
pub const MAX_BAD_LOGINS: u8 = 3;
/// Commands it does not understand, or whose arguments do not fit, before it is ended.
pub const MAX_ERRORS: u8 = 20;

/// One message of the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// What the backend calls the message (the database id); the session never looks inside it.
    pub id: i64,
    /// The `UIDL` text: `<UIDVALIDITY>.<UID>` (letters, digits and a dot, at most 70 characters).
    pub uid: String,
    /// The size in octets as stored.
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginError {
    /// The credentials are wrong (or the account may not sign in): always the same answer.
    Bad,
    /// Too many failures from this address or for this account.
    Throttled,
    /// Too many sign-ins are being checked at once.
    Busy,
    /// A store could not answer.
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchError {
    /// Another protocol removed the message since the snapshot.
    Gone,
    Unavailable,
    /// The memory the service may spend on messages in flight is used up for now.
    Busy,
}

/// A message read from the store, with whatever the backend wants held while the session works on it (a reservation of memory: the message is copied while it is made ready to send).
pub struct Fetched {
    pub raw: Bytes,
    pub hold: Option<Box<dyn std::any::Any + Send>>,
}

impl Fetched {
    pub fn plain(raw: Bytes) -> Fetched {
        Fetched { raw, hold: None }
    }
}

/// What the session needs of the outside.
#[async_trait]
pub trait Backend: Send {
    /// Check the credentials (throttle, password, status of the account). On `Ok` the backend holds the account.
    async fn login(&mut self, user: &str, password: &str) -> Result<(), LoginError>;
    /// The Inbox of the signed-in account, oldest first.
    async fn snapshot(&mut self) -> Result<Vec<Entry>, ()>;
    /// The raw bytes of one message (`size` is what the snapshot says it is, for the memory it will need).
    async fn retrieve(&mut self, id: i64, size: u64) -> Result<Fetched, FetchError>;
    /// Remove these messages (the UPDATE state).
    async fn expunge(&mut self, ids: &[i64]) -> Result<(), ()>;
}

/// How the connection reaches the server: already inside TLS (995), or in clear text with `STLS` on offer (110).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Tls,
    Plain { stls: bool },
}

/// What the driver does after a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Continue,
    /// The reply is written; end the connection.
    Close,
    /// The reply is written; negotiate TLS now, then call [`Session::tls_started`].
    StartTls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Live,
    Marked,
    /// Removed by someone else since the snapshot.
    Gone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Authorization,
    Transaction,
}

pub struct Session {
    tls: bool,
    stls: bool,
    allow_plain: bool,
    state: State,
    user: Option<String>,
    awaiting_plain: bool,
    entries: Vec<Entry>,
    slots: Vec<Slot>,
    bad_logins: u8,
    errors: u8,
}

fn ok(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(b"+OK");
    if !text.is_empty() {
        out.push(b' ');
        out.extend_from_slice(text.as_bytes());
    }
    out.extend_from_slice(b"\r\n");
}

fn err(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(b"-ERR ");
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(b"\r\n");
}

/// The message with every line ending a CRLF and a CRLF at its end (what POP3 carries; the store keeps CRLF, so for a stored message this changes nothing).
pub(crate) fn normalise(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 2);
    let mut rest = raw;
    while !rest.is_empty() {
        let (line, tail) = match rest.iter().position(|b| *b == b'\n') {
            Some(i) => (rest.get(..i).unwrap_or(&[]), rest.get(i + 1..).unwrap_or(&[])),
            None => (rest, &[][..]),
        };
        out.extend_from_slice(line.strip_suffix(b"\r").unwrap_or(line));
        out.extend_from_slice(b"\r\n");
        rest = tail;
    }
    out
}

/// Byte-stuff a normalised message (a line that starts with a dot gets another) and end it with the lone dot.
pub(crate) fn stuff(normalised: &[u8], out: &mut Vec<u8>) {
    let mut at_line_start = true;
    for b in normalised {
        if at_line_start && *b == b'.' {
            out.push(b'.');
        }
        out.push(*b);
        at_line_start = *b == b'\n';
    }
    out.extend_from_slice(b".\r\n");
}

/// The first `lines` lines of the body after the header section (RFC 1939 `TOP`), the header section and its blank line always.
pub(crate) fn top_of(normalised: &[u8], lines: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut in_body = false;
    let mut sent = 0u32;
    for line in normalised.split_inclusive(|b| *b == b'\n') {
        if in_body {
            if sent >= lines {
                break;
            }
            sent += 1;
            out.extend_from_slice(line);
        } else {
            out.extend_from_slice(line);
            if line == b"\r\n" {
                in_body = true;
            }
        }
    }
    out
}

/// Standard base64 with padding, strictly (an authentication payload takes no leniency).
fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    if !input.len().is_multiple_of(4) {
        return None;
    }
    let val = |b: u8| -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some(u32::from(b - b'A')),
            b'a'..=b'z' => Some(u32::from(b - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(b - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let chunks: Vec<&[u8]> = input.chunks(4).collect();
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    for (ci, c) in chunks.iter().enumerate() {
        let pad = c.iter().rev().take_while(|b| **b == b'=').count();
        if pad > 2 || (pad > 0 && ci + 1 != chunks.len()) {
            return None;
        }
        let mut acc: u32 = 0;
        for (i, b) in c.iter().enumerate() {
            acc = (acc << 6) | if i >= 4 - pad { 0 } else { val(*b)? };
        }
        let bytes = [(acc >> 16) as u8, (acc >> 8) as u8, acc as u8];
        if (pad == 1 && bytes[2] != 0) || (pad == 2 && (bytes[1] != 0 || bytes[2] != 0)) {
            return None;
        }
        out.extend_from_slice(bytes.get(..3 - pad)?);
    }
    Some(out)
}

/// `authzid NUL authcid NUL password` (RFC 4616): the user name and the password, when the authorization identity is empty or the same name.
fn plain_credentials(raw: &[u8]) -> Option<(String, String)> {
    let mut parts = raw.split(|b| *b == 0);
    let (authzid, authcid, password) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || authcid.is_empty() || password.is_empty() || (!authzid.is_empty() && authzid != authcid) {
        return None;
    }
    Some((String::from_utf8(authcid.to_vec()).ok()?, String::from_utf8(password.to_vec()).ok()?))
}

impl Session {
    pub fn new(transport: Transport, allow_plain: bool) -> Session {
        let (tls, stls) = match transport {
            Transport::Tls => (true, false),
            Transport::Plain { stls } => (false, stls),
        };
        Session { tls, stls, allow_plain, state: State::Authorization, user: None, awaiting_plain: false, entries: Vec::new(), slots: Vec::new(), bad_logins: 0, errors: 0 }
    }

    pub fn greeting(&self) -> &'static [u8] {
        b"+OK rusty POP3 ready\r\n"
    }

    /// The driver calls this when the TLS handshake after `STLS` is done: the session starts over, as RFC 2595 says.
    pub fn tls_started(&mut self) {
        self.tls = true;
        self.stls = false;
        self.state = State::Authorization;
        self.user = None;
        self.awaiting_plain = false;
    }

    pub fn is_authenticated(&self) -> bool {
        self.state == State::Transaction
    }

    /// The longest line the next read may hold: the continuation of `AUTH PLAIN` is longer than a command.
    pub fn line_limit(&self) -> usize {
        if self.awaiting_plain {
            crate::command::MAX_AUTH_LINE
        } else {
            crate::command::MAX_LINE
        }
    }

    fn live(&self) -> impl Iterator<Item = (usize, &Entry)> {
        self.entries.iter().enumerate().filter(|(i, _)| self.slots.get(*i) == Some(&Slot::Live))
    }

    fn totals(&self) -> (usize, u64) {
        self.live().fold((0, 0), |(n, s), (_, e)| (n + 1, s + e.size))
    }

    /// The entry of message number `n` (1-based), if it is there and not marked.
    fn entry(&self, n: u32) -> Result<(usize, &Entry), &'static str> {
        let i = usize::try_from(n).ok().and_then(|n| n.checked_sub(1)).ok_or("no such message")?;
        match (self.entries.get(i), self.slots.get(i)) {
            (Some(e), Some(Slot::Live)) => Ok((i, e)),
            (Some(_), Some(Slot::Marked)) => Err("message already deleted"),
            _ => Err("no such message"),
        }
    }

    fn mistake(&mut self, out: &mut Vec<u8>, text: &str) -> Step {
        err(out, text);
        self.errors += 1;
        if self.errors >= MAX_ERRORS {
            Step::Close
        } else {
            Step::Continue
        }
    }

    /// One line (without its line ending): the reply is appended to `out`.
    pub async fn line(&mut self, raw: &[u8], backend: &mut dyn Backend, out: &mut Vec<u8>) -> Step {
        if self.awaiting_plain {
            self.awaiting_plain = false;
            return self.plain_response(raw, backend, out).await;
        }
        let command = match parse(raw) {
            Ok(c) => c,
            Err(ParseError::Unknown) => return self.mistake(out, "unknown command"),
            Err(ParseError::Syntax) => return self.mistake(out, "bad arguments"),
        };
        match (self.state, command) {
            (_, Command::Capa) => {
                self.capa(out);
                Step::Continue
            }
            (_, Command::Noop) => {
                ok(out, "");
                Step::Continue
            }
            (_, Command::Utf8) => {
                ok(out, "UTF-8 is on");
                Step::Continue
            }
            (State::Authorization, Command::Quit) => {
                ok(out, "bye");
                Step::Close
            }
            (State::Authorization, Command::Stls) => {
                if self.stls && !self.tls && self.user.is_none() {
                    ok(out, "begin TLS negotiation");
                    Step::StartTls
                } else {
                    self.mistake(out, "STLS is not available")
                }
            }
            (State::Authorization, Command::User(name)) => {
                if !self.tls && !self.allow_plain {
                    return self.mistake(out, "[AUTH] TLS is required before a password");
                }
                self.user = Some(name);
                ok(out, "");
                Step::Continue
            }
            (State::Authorization, Command::Pass(password)) => {
                let Some(user) = self.user.take() else { return self.mistake(out, "USER first") };
                if !self.tls && !self.allow_plain {
                    return self.mistake(out, "[AUTH] TLS is required before a password");
                }
                self.sign_in(&user, &password, backend, out).await
            }
            (State::Authorization, Command::Auth { mechanism, initial }) => {
                if !self.tls && !self.allow_plain {
                    return self.mistake(out, "[AUTH] TLS is required before a password");
                }
                match (mechanism.as_deref(), initial) {
                    (None, _) => {
                        ok(out, "");
                        out.extend_from_slice(b"PLAIN\r\n.\r\n");
                        Step::Continue
                    }
                    (Some("PLAIN"), Some(first)) => self.plain_response(first.as_bytes(), backend, out).await,
                    (Some("PLAIN"), None) => {
                        out.extend_from_slice(b"+ \r\n");
                        self.awaiting_plain = true;
                        Step::Continue
                    }
                    (Some(_), _) => self.mistake(out, "unsupported mechanism"),
                }
            }
            (State::Authorization, _) => self.mistake(out, "[AUTH] sign in first"),
            (State::Transaction, Command::Quit) => {
                let ids: Vec<i64> = self.entries.iter().zip(&self.slots).filter(|(_, s)| **s == Slot::Marked).map(|(e, _)| e.id).collect();
                if ids.is_empty() || backend.expunge(&ids).await.is_ok() {
                    ok(out, "bye");
                } else {
                    err(out, "[SYS/TEMP] some deleted messages were not removed");
                }
                Step::Close
            }
            (State::Transaction, Command::Stat) => {
                let (n, size) = self.totals();
                ok(out, &format!("{n} {size}"));
                Step::Continue
            }
            (State::Transaction, Command::List(arg)) => self.listing(arg, false, out),
            (State::Transaction, Command::Uidl(arg)) => self.listing(arg, true, out),
            (State::Transaction, Command::Retr(n)) => self.retrieve(n, None, backend, out).await,
            (State::Transaction, Command::Top(n, lines)) => self.retrieve(n, Some(lines), backend, out).await,
            (State::Transaction, Command::Dele(n)) => {
                match self.entry(n) {
                    Ok((i, _)) => {
                        if let Some(slot) = self.slots.get_mut(i) {
                            *slot = Slot::Marked;
                        }
                        ok(out, &format!("message {n} deleted"));
                        Step::Continue
                    }
                    Err(text) => self.mistake(out, text),
                }
            }
            (State::Transaction, Command::Rset) => {
                for slot in &mut self.slots {
                    if *slot == Slot::Marked {
                        *slot = Slot::Live;
                    }
                }
                let (n, size) = self.totals();
                ok(out, &format!("maildrop has {n} messages ({size} octets)"));
                Step::Continue
            }
            (State::Transaction, _) => self.mistake(out, "already signed in"),
        }
    }

    fn capa(&self, out: &mut Vec<u8>) {
        ok(out, "capability list follows");
        for cap in ["TOP", "UIDL", "RESP-CODES", "PIPELINING", "UTF8", "EXPIRE NEVER", "IMPLEMENTATION rusty"] {
            out.extend_from_slice(cap.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        if self.state == State::Authorization {
            if self.stls && !self.tls {
                out.extend_from_slice(b"STLS\r\n");
            }
            if self.tls || self.allow_plain {
                out.extend_from_slice(b"USER\r\nSASL PLAIN\r\n");
            }
        }
        out.extend_from_slice(b".\r\n");
    }

    async fn plain_response(&mut self, raw: &[u8], backend: &mut dyn Backend, out: &mut Vec<u8>) -> Step {
        if raw == b"*" {
            return self.mistake(out, "authentication cancelled");
        }
        // `=` is an empty initial response, which PLAIN never accepts.
        let credentials = base64_decode(raw).and_then(|b| plain_credentials(&b));
        match credentials {
            Some((user, password)) => self.sign_in(&user, &password, backend, out).await,
            None => {
                self.bad_logins += 1;
                err(out, "[AUTH] invalid credentials");
                if self.bad_logins >= MAX_BAD_LOGINS {
                    Step::Close
                } else {
                    Step::Continue
                }
            }
        }
    }

    async fn sign_in(&mut self, user: &str, password: &str, backend: &mut dyn Backend, out: &mut Vec<u8>) -> Step {
        match backend.login(user, password).await {
            Ok(()) => match backend.snapshot().await {
                Ok(entries) => {
                    self.slots = vec![Slot::Live; entries.len()];
                    self.entries = entries;
                    self.state = State::Transaction;
                    let (n, size) = self.totals();
                    ok(out, &format!("maildrop has {n} messages ({size} octets)"));
                    Step::Continue
                }
                Err(()) => {
                    err(out, "[SYS/TEMP] the maildrop cannot be read now, try again later");
                    Step::Close
                }
            },
            Err(LoginError::Bad) => {
                self.bad_logins += 1;
                err(out, "[AUTH] invalid credentials");
                if self.bad_logins >= MAX_BAD_LOGINS {
                    Step::Close
                } else {
                    Step::Continue
                }
            }
            Err(LoginError::Throttled) => {
                err(out, "[LOGIN-DELAY] too many failed attempts, try again later");
                Step::Close
            }
            Err(LoginError::Busy) => {
                err(out, "[SYS/TEMP] busy, try again in a moment");
                Step::Continue
            }
            Err(LoginError::Unavailable) => {
                err(out, "[SYS/TEMP] try again later");
                Step::Close
            }
        }
    }

    fn listing(&mut self, arg: Option<u32>, uidl: bool, out: &mut Vec<u8>) -> Step {
        let line = |i: usize, e: &Entry| if uidl { format!("{} {}\r\n", i + 1, e.uid) } else { format!("{} {}\r\n", i + 1, e.size) };
        match arg {
            Some(n) => match self.entry(n) {
                Ok((i, e)) => {
                    ok(out, line(i, e).trim_end());
                    Step::Continue
                }
                Err(text) => self.mistake(out, text),
            },
            None => {
                if uidl {
                    ok(out, "");
                } else {
                    let (n, size) = self.totals();
                    ok(out, &format!("{n} messages ({size} octets)"));
                }
                for (i, e) in self.live() {
                    out.extend_from_slice(line(i, e).as_bytes());
                }
                out.extend_from_slice(b".\r\n");
                Step::Continue
            }
        }
    }

    async fn retrieve(&mut self, n: u32, top: Option<u32>, backend: &mut dyn Backend, out: &mut Vec<u8>) -> Step {
        let (i, id, size) = match self.entry(n) {
            Ok((i, e)) => (i, e.id, e.size),
            Err(text) => return self.mistake(out, text),
        };
        match backend.retrieve(id, size).await {
            Ok(Fetched { raw, hold }) => {
                let whole = normalise(&raw);
                drop(raw);
                let shown = match top {
                    Some(lines) => top_of(&whole, lines),
                    None => whole,
                };
                if top.is_some() {
                    ok(out, "top of message follows");
                } else {
                    ok(out, &format!("{} octets", shown.len()));
                }
                stuff(&shown, out);
                // The reservation is kept until the reply is made: the copies of the message are gone with `shown`.
                drop(shown);
                drop(hold);
                Step::Continue
            }
            Err(FetchError::Busy) => {
                err(out, "[SYS/TEMP] busy, try again in a moment");
                Step::Continue
            }
            Err(FetchError::Gone) => {
                if let Some(slot) = self.slots.get_mut(i) {
                    *slot = Slot::Gone;
                }
                self.mistake(out, "no such message")
            }
            Err(FetchError::Unavailable) => {
                err(out, "[SYS/TEMP] the message cannot be read now, try again later");
                Step::Continue
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A maildrop in memory: the account `alice` with the password `secret`.
    struct Fake {
        messages: Vec<(i64, Vec<u8>)>,
        gone: Vec<i64>,
        removed: Vec<i64>,
        logins: Vec<(String, String)>,
        login_result: Option<LoginError>,
        fail_expunge: bool,
        fail_snapshot: bool,
        unavailable: Vec<i64>,
        busy: Vec<i64>,
    }

    impl Fake {
        fn with(messages: Vec<(i64, &[u8])>) -> Fake {
            Fake { messages: messages.into_iter().map(|(i, m)| (i, m.to_vec())).collect(), gone: Vec::new(), removed: Vec::new(), logins: Vec::new(), login_result: None, fail_expunge: false, fail_snapshot: false, unavailable: Vec::new(), busy: Vec::new() }
        }
    }

    #[async_trait]
    impl Backend for Fake {
        async fn login(&mut self, user: &str, password: &str) -> Result<(), LoginError> {
            self.logins.push((user.to_owned(), password.to_owned()));
            if let Some(e) = self.login_result {
                return Err(e);
            }
            if user == "alice" && password == "secret" {
                Ok(())
            } else {
                Err(LoginError::Bad)
            }
        }
        async fn snapshot(&mut self) -> Result<Vec<Entry>, ()> {
            if self.fail_snapshot {
                return Err(());
            }
            Ok(self.messages.iter().map(|(id, m)| Entry { id: *id, uid: format!("7.{id}"), size: m.len() as u64 }).collect())
        }
        async fn retrieve(&mut self, id: i64, _size: u64) -> Result<Fetched, FetchError> {
            if self.gone.contains(&id) {
                return Err(FetchError::Gone);
            }
            if self.unavailable.contains(&id) {
                return Err(FetchError::Unavailable);
            }
            if self.busy.contains(&id) {
                return Err(FetchError::Busy);
            }
            self.messages.iter().find(|(i, _)| *i == id).map(|(_, m)| Fetched::plain(Bytes::from(m.clone()))).ok_or(FetchError::Gone)
        }
        async fn expunge(&mut self, ids: &[i64]) -> Result<(), ()> {
            if self.fail_expunge {
                return Err(());
            }
            self.removed.extend_from_slice(ids);
            Ok(())
        }
    }

    /// Run lines through a session and return the text of every reply, with the last step.
    async fn run(session: &mut Session, backend: &mut Fake, lines: &[&str]) -> (String, Step) {
        let mut out = Vec::new();
        let mut step = Step::Continue;
        for l in lines {
            step = session.line(l.as_bytes(), backend, &mut out).await;
            if step != Step::Continue {
                break;
            }
        }
        (String::from_utf8(out).unwrap(), step)
    }

    fn tls() -> Session {
        Session::new(Transport::Tls, false)
    }

    const M1: &[u8] = b"Subject: one\r\nFrom: a@b.test\r\n\r\nHello\r\n.dot\r\nlast\r\n";
    const M2: &[u8] = b"Subject: two\r\n\r\nB\r\n";

    #[tokio::test]
    async fn a_login_gives_the_snapshot_and_stat_list_and_uidl_describe_it() {
        let mut b = Fake::with(vec![(10, M1), (11, M2)]);
        let mut s = tls();
        let (text, step) = run(&mut s, &mut b, &["USER alice", "PASS secret", "STAT", "LIST", "LIST 2", "UIDL", "UIDL 1"]).await;
        assert_eq!(step, Step::Continue);
        let total = M1.len() + M2.len();
        let want = format!(
            "+OK\r\n+OK maildrop has 2 messages ({total} octets)\r\n+OK 2 {total}\r\n+OK 2 messages ({total} octets)\r\n1 {}\r\n2 {}\r\n.\r\n+OK 2 {}\r\n+OK\r\n1 7.10\r\n2 7.11\r\n.\r\n+OK 1 7.10\r\n",
            M1.len(),
            M2.len(),
            M2.len()
        );
        assert_eq!(text, want);
        assert!(s.is_authenticated());
    }

    #[tokio::test]
    async fn retr_dot_stuffs_ends_with_a_lone_dot_and_says_the_size() {
        let mut b = Fake::with(vec![(10, M1)]);
        let mut s = tls();
        let (text, _) = run(&mut s, &mut b, &["USER alice", "PASS secret", "RETR 1"]).await;
        let body = text.split("\r\n").skip(2).collect::<Vec<_>>().join("\r\n");
        assert_eq!(body, format!("+OK {} octets\r\nSubject: one\r\nFrom: a@b.test\r\n\r\nHello\r\n..dot\r\nlast\r\n.\r\n", M1.len()));
    }

    #[tokio::test]
    async fn a_message_with_bare_lf_or_no_final_newline_is_sent_with_crlf() {
        let mut b = Fake::with(vec![(1, b"A: b\n\nline one\nlast no newline")]);
        let mut s = tls();
        let (text, _) = run(&mut s, &mut b, &["USER alice", "PASS secret", "RETR 1"]).await;
        assert!(text.ends_with("A: b\r\n\r\nline one\r\nlast no newline\r\n.\r\n"), "{text:?}");
        assert!(!text.contains("\n\n") && !text.replace("\r\n", "").contains('\n'));
    }

    #[tokio::test]
    async fn top_sends_the_headers_and_that_many_lines() {
        let mut b = Fake::with(vec![(1, M1)]);
        let mut s = tls();
        let login = ["USER alice", "PASS secret"];
        let mut lines: Vec<&str> = login.to_vec();
        lines.extend(["TOP 1 0", "TOP 1 1", "TOP 1 99"]);
        let (text, _) = run(&mut s, &mut b, &lines).await;
        let replies: Vec<&str> = text.split("+OK top of message follows\r\n").skip(1).collect();
        assert_eq!(replies[0], "Subject: one\r\nFrom: a@b.test\r\n\r\n.\r\n", "zero lines: the headers and the blank line");
        assert!(replies[1].starts_with("Subject: one\r\nFrom: a@b.test\r\n\r\nHello\r\n.\r\n"), "{}", replies[1]);
        assert!(replies[2].contains("..dot\r\nlast\r\n.\r\n"), "more lines than there are: all of them, dot-stuffed");
    }

    #[tokio::test]
    async fn dele_marks_rset_unmarks_and_quit_removes_exactly_the_marked_ones() {
        let mut b = Fake::with(vec![(10, M1), (11, M2), (12, M2)]);
        let mut s = tls();
        let (text, step) = run(&mut s, &mut b, &["USER alice", "PASS secret", "DELE 1", "DELE 1", "RETR 1", "STAT", "DELE 3", "RSET", "DELE 2", "QUIT"]).await;
        assert_eq!(step, Step::Close);
        assert!(text.contains("+OK message 1 deleted\r\n-ERR message already deleted\r\n-ERR message already deleted\r\n"), "{text}");
        assert!(text.contains(&format!("+OK maildrop has 3 messages ({} octets)\r\n", M1.len() + 2 * M2.len())), "RSET unmarks everything");
        assert!(text.ends_with("+OK bye\r\n"));
        assert_eq!(b.removed, vec![11], "only message 2 was marked when QUIT came");
    }

    #[tokio::test]
    async fn a_connection_that_ends_without_quit_removes_nothing() {
        let mut b = Fake::with(vec![(10, M1)]);
        let mut s = tls();
        run(&mut s, &mut b, &["USER alice", "PASS secret", "DELE 1"]).await;
        drop(s);
        assert!(b.removed.is_empty());
    }

    #[tokio::test]
    async fn a_failed_expunge_is_told_and_a_quit_without_marks_does_not_ask() {
        let mut b = Fake::with(vec![(10, M1)]);
        b.fail_expunge = true;
        let mut s = tls();
        let (text, step) = run(&mut s, &mut b, &["USER alice", "PASS secret", "DELE 1", "QUIT"]).await;
        assert_eq!(step, Step::Close);
        assert!(text.ends_with("-ERR [SYS/TEMP] some deleted messages were not removed\r\n"), "{text}");
        let mut s = tls();
        let (text, _) = run(&mut s, &mut b, &["USER alice", "PASS secret", "QUIT"]).await;
        assert!(text.ends_with("+OK bye\r\n"), "nothing was marked, so nothing failed");
    }

    #[tokio::test]
    async fn a_message_another_protocol_removed_is_gone_for_the_rest_of_the_session_and_not_removed_again() {
        let mut b = Fake::with(vec![(10, M1), (11, M2)]);
        b.gone.push(10);
        let mut s = tls();
        let (text, _) = run(&mut s, &mut b, &["USER alice", "PASS secret", "RETR 1", "STAT", "LIST 1", "QUIT"]).await;
        assert!(text.contains("-ERR no such message\r\n"));
        assert!(text.contains(&format!("+OK 1 {}\r\n", M2.len())), "STAT no longer counts it: {text}");
        assert!(b.removed.is_empty());
        let mut b = Fake::with(vec![(10, M1)]);
        b.unavailable.push(10);
        let mut s = tls();
        let (text, step) = run(&mut s, &mut b, &["USER alice", "PASS secret", "RETR 1", "STAT"]).await;
        assert_eq!(step, Step::Continue, "a store that cannot answer does not end the session or drop the message");
        assert!(text.contains("-ERR [SYS/TEMP] the message cannot be read now") && text.ends_with(&format!("+OK 1 {}\r\n", M1.len())));
        // No memory to read it with right now: told so, and the session goes on with the message still there.
        let mut b = Fake::with(vec![(10, M1)]);
        b.busy.push(10);
        let mut s = tls();
        let (text, step) = run(&mut s, &mut b, &["USER alice", "PASS secret", "RETR 1", "STAT"]).await;
        assert_eq!(step, Step::Continue);
        assert!(text.contains("-ERR [SYS/TEMP] busy, try again in a moment\r\n") && text.ends_with(&format!("+OK 1 {}\r\n", M1.len())), "{text}");
    }

    #[tokio::test]
    async fn a_password_is_never_taken_in_clear_text_unless_the_test_setting_says_so() {
        let mut b = Fake::with(vec![]);
        let mut s = Session::new(Transport::Plain { stls: true }, false);
        let (text, _) = run(&mut s, &mut b, &["USER alice", "PASS secret", "AUTH PLAIN AGFsaWNlAHNlY3JldA=="]).await;
        assert_eq!(text.matches("-ERR [AUTH] TLS is required before a password\r\n").count(), 2, "{text}");
        assert!(text.contains("-ERR USER first\r\n"), "the refused USER stored nothing: {text}");
        assert!(b.logins.is_empty(), "the password never reached the check");
        let mut s = Session::new(Transport::Plain { stls: true }, true);
        let (text, _) = run(&mut s, &mut b, &["USER alice", "PASS secret"]).await;
        assert!(text.contains("maildrop has 0 messages"), "{text}");
    }

    #[tokio::test]
    async fn stls_starts_tls_once_and_the_session_starts_over_after_it() {
        let mut b = Fake::with(vec![]);
        let mut s = Session::new(Transport::Plain { stls: true }, false);
        let (text, step) = run(&mut s, &mut b, &["CAPA", "STLS"]).await;
        assert!(text.contains("STLS\r\n") && !text.contains("USER\r\n") && !text.contains("SASL PLAIN"), "before TLS: STLS is offered and a password is not: {text}");
        assert_eq!(step, Step::StartTls);
        s.tls_started();
        let (text, _) = run(&mut s, &mut b, &["CAPA", "STLS", "USER alice", "PASS secret"]).await;
        assert!(text.contains("USER\r\nSASL PLAIN\r\n") && !text.contains("\r\nSTLS\r\n"), "{text}");
        assert!(text.contains("-ERR STLS is not available\r\n") && text.contains("maildrop has 0 messages"));
        // On the implicit-TLS port, and after signing in, STLS is refused.
        let mut s = tls();
        let (text, step) = run(&mut s, &mut b, &["STLS"]).await;
        assert_eq!((text.as_str(), step), ("-ERR STLS is not available\r\n", Step::Continue));
    }

    #[tokio::test]
    async fn auth_plain_works_with_an_initial_response_and_with_a_continuation_and_can_be_cancelled() {
        let mut b = Fake::with(vec![]);
        // "\0alice\0secret"
        let mut s = tls();
        let (text, _) = run(&mut s, &mut b, &["AUTH", "AUTH PLAIN AGFsaWNlAHNlY3JldA=="]).await;
        assert!(text.starts_with("+OK\r\nPLAIN\r\n.\r\n+OK maildrop has 0 messages"), "{text}");
        let mut s = tls();
        let (text, _) = run(&mut s, &mut b, &["AUTH PLAIN", "AGFsaWNlAHNlY3JldA=="]).await;
        assert_eq!(text, "+ \r\n+OK maildrop has 0 messages (0 octets)\r\n");
        let mut s = tls();
        assert_eq!(s.line_limit(), crate::command::MAX_LINE);
        let mut out = Vec::new();
        s.line(b"AUTH PLAIN", &mut b, &mut out).await;
        assert_eq!(s.line_limit(), crate::command::MAX_AUTH_LINE, "the continuation is allowed to be longer");
        let (text, _) = run(&mut s, &mut b, &["*", "AUTH PLAIN YWxpY2U=", "AUTH PLAIN =", "AUTH LOGIN"]).await;
        assert_eq!(text.matches("-ERR").count(), 4, "{text}");
        assert!(!s.is_authenticated());
    }

    #[tokio::test]
    async fn three_bad_passwords_end_the_session_and_the_throttle_ends_it_at_once() {
        let mut b = Fake::with(vec![]);
        let mut s = tls();
        let (text, step) = run(&mut s, &mut b, &["USER alice", "PASS no", "USER alice", "PASS no", "USER alice", "PASS no"]).await;
        assert_eq!(step, Step::Close);
        assert_eq!(text.matches("-ERR [AUTH] invalid credentials\r\n").count(), 3);
        let mut b = Fake::with(vec![]);
        b.login_result = Some(LoginError::Throttled);
        let mut s = tls();
        let (text, step) = run(&mut s, &mut b, &["USER alice", "PASS secret"]).await;
        assert_eq!(step, Step::Close);
        assert!(text.ends_with("-ERR [LOGIN-DELAY] too many failed attempts, try again later\r\n"));
        let mut b = Fake::with(vec![]);
        b.login_result = Some(LoginError::Busy);
        let mut s = tls();
        let (text, step) = run(&mut s, &mut b, &["USER alice", "PASS secret"]).await;
        assert_eq!(step, Step::Continue, "a busy answer lets the client try again");
        assert!(text.contains("[SYS/TEMP] busy"));
        let mut b = Fake::with(vec![]);
        b.fail_snapshot = true;
        let mut s = tls();
        let (_, step) = run(&mut s, &mut b, &["USER alice", "PASS secret"]).await;
        assert_eq!(step, Step::Close);
    }

    #[tokio::test]
    async fn commands_of_the_other_state_unknown_commands_and_too_many_mistakes_are_refused() {
        let mut b = Fake::with(vec![(1, M2)]);
        let mut s = tls();
        let (text, _) = run(&mut s, &mut b, &["STAT", "RETR 1", "DELE 1", "QUIT"]).await;
        assert_eq!(text.matches("-ERR [AUTH] sign in first\r\n").count(), 3);
        assert!(text.ends_with("+OK bye\r\n"), "QUIT before signing in is fine and removes nothing");
        let mut s = tls();
        let (text, _) = run(&mut s, &mut b, &["PASS secret", "USER alice", "PASS secret", "USER alice", "PASS x", "RETR 9", "RETR 0", "RETR 1 2", "HELO", "LIST 5", "DELE 4", "TOP 1", "USER bob"]).await;
        assert!(text.contains("-ERR USER first\r\n") && text.contains("-ERR no such message\r\n") && text.contains("-ERR unknown command\r\n") && text.contains("-ERR already signed in\r\n"), "{text}");
        let mut s = tls();
        let junk: Vec<&str> = std::iter::repeat_n("WHAT", usize::from(MAX_ERRORS)).collect();
        let (_, step) = run(&mut s, &mut b, &junk).await;
        assert_eq!(step, Step::Close, "a client that only makes mistakes is ended");
    }

    #[test]
    fn dot_stuffing_round_trips_and_the_helpers_are_exact() {
        assert_eq!(normalise(b"a\nb\r\nc"), b"a\r\nb\r\nc\r\n");
        assert_eq!(normalise(b""), b"");
        assert_eq!(normalise(b"\n"), b"\r\n");
        let mut out = Vec::new();
        stuff(b".\r\n..x\r\ny.\r\n", &mut out);
        assert_eq!(out, b"..\r\n...x\r\ny.\r\n.\r\n");
        let mut out = Vec::new();
        stuff(b"", &mut out);
        assert_eq!(out, b".\r\n");
        assert_eq!(top_of(b"H: 1\r\n\r\na\r\nb\r\n", 1), b"H: 1\r\n\r\na\r\n");
        assert_eq!(top_of(b"H: 1\r\nonly headers\r\n", 5), b"H: 1\r\nonly headers\r\n");
    }

    proptest::proptest! {
        /// Whatever the bytes, the stuffed text un-stuffs to the normalised message, and the lone dot ends it and appears nowhere else.
        #[test]
        fn stuffing_is_reversible_and_the_terminator_is_unique(raw in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512)) {
            let whole = normalise(&raw);
            let mut out = Vec::new();
            stuff(&whole, &mut out);
            proptest::prop_assert!(out.ends_with(b".\r\n"));
            let body = &out[..out.len() - 3];
            let mut back = Vec::new();
            for line in body.split_inclusive(|b| *b == b'\n') {
                back.extend_from_slice(line.strip_prefix(b".").unwrap_or(line));
            }
            proptest::prop_assert_eq!(&back, &whole);
            // No line of the stuffed body is a lone dot.
            proptest::prop_assert!(!body.split(|b| *b == b'\n').any(|l| l == b".\r" || l == b"."));
        }
    }
}
