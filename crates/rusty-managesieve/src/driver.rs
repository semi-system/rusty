//! The ManageSieve service (RFC 5804, docs/design-sieve.md): a person's mail client uploads, lists, reads, activates and removes the Sieve scripts of the account. It is a listener of the IMAP service
//! (port 4190, STARTTLS). A password is taken only inside TLS, by `AUTHENTICATE PLAIN`, with the own password of the account or an app password, through the same check, throttle and rules as IMAP.
//! The parser of Sieve (`rusty-sieve`) is the checker of every script before it is kept; the scripts are kept by [`rusty_store::SieveRepo`].

use std::time::Duration;

use async_trait::async_trait;
use base64::prelude::{Engine, BASE64_STANDARD};
use rusty_core::{AccountId, Error};
use rusty_store::{MAX_SCRIPTS, MAX_SCRIPT_BYTES};
use rusty_svc::{BoxedStream, Context, PeerInfo, ServiceEvent, SessionHandler, Verdict};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::command::{parse, Arg, Command, Limits as ParseLimits, Parsed};

const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
const STARTTLS_TIMEOUT: Duration = Duration::from_secs(15);
/// What the `SIEVE` capability lists: the extensions the parser accepts in `require`.
const EXTENSIONS: &str = "fileinto reject envelope vacation";
/// The longest name of a script (the same as the table's check).
const MAX_NAME_CHARS: usize = 128;
/// The longest base64 of a PLAIN response: a name and a password of a few hundred bytes each.
const MAX_SASL_BYTES: usize = 2048;
/// Failed sign-ins one connection may make before it is closed (the throttle counts them per address and account as well).
const MAX_FAILED_LOGINS: u32 = 5;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub login_timeout: Duration,
    pub idle_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { login_timeout: Duration::from_secs(60), idle_timeout: Duration::from_secs(600) }
    }
}

/// The ManageSieve service: one instance serves every connection of a listener.
pub struct ManageSieveHandler {
    ctx: Context,
    limits: Limits,
    allow_plain: bool,
}

impl ManageSieveHandler {
    pub fn new(ctx: Context) -> ManageSieveHandler {
        ManageSieveHandler { ctx, limits: Limits::default(), allow_plain: false }
    }

    pub fn with_limits(mut self, limits: Limits) -> ManageSieveHandler {
        self.limits = limits;
        self
    }

    /// Accept `AUTHENTICATE` in clear text: for a test network only (nothing in the service sets it).
    pub fn with_plain_login_for_tests(mut self) -> ManageSieveHandler {
        self.allow_plain = true;
        self
    }
}

#[async_trait]
impl SessionHandler for ManageSieveHandler {
    async fn handle(&self, stream: BoxedStream, peer: PeerInfo, shutdown: CancellationToken) {
        let tls = peer.tls;
        let conn = Conn { ctx: self.ctx.clone(), peer, tls, allow_plain: self.allow_plain, account: None, sasl: false, failed: 0 };
        run(conn, stream, shutdown, self.limits).await;
    }
}

struct Conn {
    ctx: Context,
    peer: PeerInfo,
    tls: bool,
    allow_plain: bool,
    account: Option<AccountId>,
    /// `AUTHENTICATE "PLAIN"` was sent without the response: the next message is the response.
    sasl: bool,
    failed: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Continue,
    Close,
    StartTls,
}

/// A quoted string for a reply: the text is ours, so a character that does not belong in one is dropped.
fn quoted(out: &mut Vec<u8>, text: &str) {
    out.push(b'"');
    for c in text.chars().filter(|c| !c.is_control()) {
        if c == '"' || c == '\\' {
            out.push(b'\\');
        }
        let mut b = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
    }
    out.push(b'"');
}

fn reply(out: &mut Vec<u8>, kind: &str, code: Option<&str>, text: Option<&str>) {
    out.extend_from_slice(kind.as_bytes());
    if let Some(c) = code {
        out.extend_from_slice(format!(" ({c})").as_bytes());
    }
    if let Some(t) = text {
        out.push(b' ');
        quoted(out, t);
    }
    out.extend_from_slice(b"\r\n");
}

fn ok(out: &mut Vec<u8>, text: &str) {
    reply(out, "OK", None, Some(text));
}

fn no(out: &mut Vec<u8>, code: Option<&str>, text: &str) {
    reply(out, "NO", code, Some(text));
}

/// The capability response (RFC 5804 section 1.7): what is offered depends on whether the stream is protected.
fn capabilities(out: &mut Vec<u8>, tls: bool, allow_plain: bool) {
    for (k, v) in [("IMPLEMENTATION", "Rusty ManageSieve"), ("SIEVE", EXTENSIONS)] {
        quoted(out, k);
        out.push(b' ');
        quoted(out, v);
        out.extend_from_slice(b"\r\n");
    }
    if tls || allow_plain {
        out.extend_from_slice(b"\"SASL\" \"PLAIN\"\r\n");
    } else {
        out.extend_from_slice(b"\"SASL\" \"\"\r\n");
    }
    if !tls {
        out.extend_from_slice(b"\"STARTTLS\"\r\n");
    }
    out.extend_from_slice(b"\"VERSION\" \"1.0\"\r\n");
}

/// A script name: text of 1 to 128 characters with no control character.
fn script_name(a: Option<&Arg>) -> Option<String> {
    let Some(Arg::Str(b)) = a else { return None };
    let s = std::str::from_utf8(b).ok()?;
    let n = s.chars().count();
    (1..=MAX_NAME_CHARS).contains(&n).then_some(s).filter(|s| !s.chars().any(char::is_control)).map(str::to_owned)
}

/// What a script is: UTF-8 text the parser accepts. `Err` is the reply to send.
fn check_script(a: Option<&Arg>) -> Result<String, (Option<&'static str>, String)> {
    let Some(Arg::Str(b)) = a else { return Err((None, "a script is expected".into())) };
    if b.len() > MAX_SCRIPT_BYTES {
        return Err((Some("QUOTA/MAXSIZE"), "the script is too large".into()));
    }
    let Ok(text) = std::str::from_utf8(b) else { return Err((None, "the script is not UTF-8 text".into())) };
    match rusty_sieve::parse::parse(text) {
        Ok(_) => Ok(text.to_owned()),
        Err(e) => Err((None, e.to_string())),
    }
}

fn storage_failure(out: &mut Vec<u8>, e: &Error) {
    match e {
        Error::Quota(_) => no(out, Some("QUOTA/MAXSCRIPTS"), "too many scripts"),
        Error::Conflict(_) => no(out, Some("ACTIVE"), "the script is active"),
        Error::NotFound(_) => no(out, Some("NONEXISTENT"), "there is no such script"),
        Error::Invalid(_) => no(out, Some("QUOTA/MAXSIZE"), "the script is too large"),
        _ => no(out, Some("TRYLATER"), "temporary failure"),
    }
}

impl Conn {
    async fn handle(&mut self, cmd: Command, out: &mut Vec<u8>) -> Step {
        let args = cmd.args.as_slice();
        match (cmd.name.as_str(), args) {
            ("", _) => return self.sasl_response(args, out).await,
            ("CAPABILITY", []) => {
                capabilities(out, self.tls, self.allow_plain);
                ok(out, "Capability completed");
            }
            ("NOOP", []) => ok(out, "Done"),
            ("NOOP", [Arg::Str(tag)]) => {
                out.extend_from_slice(b"OK (TAG ");
                quoted(out, &String::from_utf8_lossy(tag));
                out.extend_from_slice(b") \"Done\"\r\n");
            }
            ("LOGOUT", []) => {
                ok(out, "Logout completed");
                return Step::Close;
            }
            ("STARTTLS", []) => {
                if self.tls {
                    no(out, None, "TLS is already on");
                } else {
                    ok(out, "Begin TLS negotiation now");
                    return Step::StartTls;
                }
            }
            ("AUTHENTICATE", [Arg::Str(m)] | [Arg::Str(m), _]) => return self.authenticate(m, args.get(1), out).await,
            ("CAPABILITY" | "NOOP" | "LOGOUT" | "STARTTLS" | "AUTHENTICATE", _) => no(out, None, "wrong arguments"),
            _ => {
                let Some(account) = self.account else {
                    no(out, None, "authenticate first");
                    return Step::Continue;
                };
                self.script_command(account, &cmd, out).await;
            }
        }
        Step::Continue
    }

    async fn authenticate(&mut self, mech: &[u8], initial: Option<&Arg>, out: &mut Vec<u8>) -> Step {
        if self.account.is_some() {
            no(out, None, "already authenticated");
            return Step::Continue;
        }
        if !self.tls && !self.allow_plain {
            no(out, Some("ENCRYPT-NEEDED"), "start TLS before the password is sent");
            return Step::Continue;
        }
        if !mech.eq_ignore_ascii_case(b"PLAIN") {
            no(out, None, "only PLAIN is offered");
            return Step::Continue;
        }
        match initial {
            Some(a) => self.sasl_response(std::slice::from_ref(a), out).await,
            None => {
                self.sasl = true;
                out.extend_from_slice(b"\"\"\r\n");
                Step::Continue
            }
        }
    }

    async fn sasl_response(&mut self, args: &[Arg], out: &mut Vec<u8>) -> Step {
        self.sasl = false;
        let Some(Arg::Str(b64)) = args.first() else {
            no(out, None, "a response is expected");
            return Step::Continue;
        };
        if b64.as_slice() == b"*" {
            no(out, None, "cancelled");
            return Step::Continue;
        }
        let decoded = if b64.len() <= MAX_SASL_BYTES { BASE64_STANDARD.decode(b64).ok() } else { None };
        // authzid NUL authcid NUL password (RFC 4616).
        let parts: Vec<&[u8]> = decoded.as_deref().unwrap_or(&[]).split(|b| *b == 0).collect();
        let [authzid, authcid, password] = parts.as_slice() else {
            no(out, None, "the response is not valid");
            return Step::Continue;
        };
        let (Ok(user), Ok(password)) = (std::str::from_utf8(authcid), std::str::from_utf8(password)) else {
            no(out, None, "the response is not valid");
            return Step::Continue;
        };
        if !authzid.is_empty() && authzid != authcid {
            no(out, None, "acting as another user is not offered");
            return Step::Continue;
        }
        if user.is_empty() || password.is_empty() {
            no(out, None, "the response is not valid");
            return Step::Continue;
        }
        match self.login(user, password).await {
            Ok(account) => {
                self.account = Some(account);
                ok(out, "Logged in");
                Step::Continue
            }
            Err(Refused::Bad) => {
                self.failed += 1;
                if self.failed >= MAX_FAILED_LOGINS {
                    reply(out, "BYE", None, Some("too many failed sign-ins"));
                    return Step::Close;
                }
                no(out, None, "the name or the password is not right");
                Step::Continue
            }
            Err(Refused::Throttled) => {
                reply(out, "BYE", Some("TRYLATER"), Some("too many failed sign-ins: try later"));
                Step::Close
            }
            Err(Refused::Busy) => {
                no(out, Some("TRYLATER"), "the server is busy");
                Step::Continue
            }
            Err(Refused::Unavailable) => {
                no(out, Some("TRYLATER"), "temporary failure");
                Step::Continue
            }
        }
    }

    /// The same check as IMAP and POP3: throttle, the password (own or app), the plan of the account, a password that must be changed first.
    async fn login(&self, user: &str, password: &str) -> Result<AccountId, Refused> {
        let ip = self.peer.addr.ip();
        let keys = rusty_svc::throttle_keys(&self.ctx.directory, user).await;
        let mut blocked = false;
        for k in &keys {
            blocked |= matches!(self.ctx.rate.check(ip, k).await, Verdict::Blocked { .. });
        }
        if blocked {
            self.ctx.events.emit(ServiceEvent::AuthResult { service: "managesieve", peer: self.peer.addr, ok: false });
            return Err(Refused::Throttled);
        }
        let Some(permit) = self.ctx.rate.try_begin_attempt() else { return Err(Refused::Busy) };
        let verified = self.ctx.auth.verify(user, password).await;
        drop(permit);
        let account = match verified {
            Ok(Some(a)) => a,
            Ok(None) => {
                for k in &keys {
                    self.ctx.rate.record_failure(ip, k).await;
                }
                self.ctx.events.emit(ServiceEvent::AuthResult { service: "managesieve", peer: self.peer.addr, ok: false });
                return Err(Refused::Bad);
            }
            Err(_) => return Err(Refused::Unavailable),
        };
        // The filters are part of what a person does with the mailbox: the plans that allow IMAP allow them.
        match self.ctx.directory.protocol_allowed(account, "imap").await {
            Ok(true) => {}
            Ok(false) => return Err(Refused::Bad),
            Err(_) => return Err(Refused::Unavailable),
        }
        match self.ctx.plans.must_change_password(account.0).await {
            Ok(false) => {}
            Ok(true) => return Err(Refused::Bad),
            Err(_) => return Err(Refused::Unavailable),
        }
        for k in &keys {
            self.ctx.rate.record_success(ip, k).await;
        }
        self.ctx.events.emit(ServiceEvent::AuthResult { service: "managesieve", peer: self.peer.addr, ok: true });
        Ok(account)
    }

    async fn script_command(&self, account: AccountId, cmd: &Command, out: &mut Vec<u8>) {
        let sieve = &self.ctx.sieve;
        let args = cmd.args.as_slice();
        match (cmd.name.as_str(), args.len()) {
            ("LISTSCRIPTS", 0) => match sieve.list(account).await {
                Ok(list) => {
                    for s in list {
                        quoted(out, &s.name);
                        out.extend_from_slice(if s.active { b" ACTIVE\r\n" } else { b"\r\n" });
                    }
                    ok(out, "Listscripts completed");
                }
                Err(e) => storage_failure(out, &e),
            },
            ("GETSCRIPT", 1) => {
                let Some(name) = script_name(args.first()) else { return no(out, Some("NONEXISTENT"), "there is no such script") };
                match sieve.get(account, &name).await {
                    Ok(Some(body)) => {
                        out.extend_from_slice(format!("{{{}}}\r\n", body.len()).as_bytes());
                        out.extend_from_slice(body.as_bytes());
                        out.extend_from_slice(b"\r\n");
                        ok(out, "Getscript completed");
                    }
                    Ok(None) => no(out, Some("NONEXISTENT"), "there is no such script"),
                    Err(e) => storage_failure(out, &e),
                }
            }
            ("PUTSCRIPT", 2) => {
                let Some(name) = script_name(args.first()) else { return no(out, None, "the name of the script is not valid") };
                match check_script(args.get(1)) {
                    Ok(body) => match sieve.put(account, &name, &body).await {
                        Ok(()) => ok(out, "Putscript completed"),
                        Err(e) => storage_failure(out, &e),
                    },
                    Err((code, text)) => no(out, code, &text),
                }
            }
            ("CHECKSCRIPT", 1) => match check_script(args.first()) {
                Ok(_) => ok(out, "Script is valid"),
                Err((code, text)) => no(out, code, &text),
            },
            ("HAVESPACE", 2) => {
                let Some(name) = script_name(args.first()) else { return no(out, None, "the name of the script is not valid") };
                let size = std::str::from_utf8(args.get(1).map(Arg::bytes).unwrap_or(&[])).ok().and_then(|s| s.parse::<usize>().ok());
                let Some(size) = size else { return no(out, None, "the size is not a number") };
                if size > MAX_SCRIPT_BYTES {
                    return no(out, Some("QUOTA/MAXSIZE"), "the script would be too large");
                }
                match sieve.list(account).await {
                    Ok(list) if list.len() as i64 >= MAX_SCRIPTS && !list.iter().any(|s| s.name == name) => no(out, Some("QUOTA/MAXSCRIPTS"), "too many scripts"),
                    Ok(_) => ok(out, "There is room"),
                    Err(e) => storage_failure(out, &e),
                }
            }
            ("SETACTIVE", 1) => {
                // An empty name turns the active script off.
                let name = match args.first() {
                    Some(Arg::Str(b)) if b.is_empty() => None,
                    a => match script_name(a) {
                        Some(n) => Some(n),
                        None => return no(out, Some("NONEXISTENT"), "there is no such script"),
                    },
                };
                match sieve.set_active(account, name.as_deref()).await {
                    Ok(()) => ok(out, "Setactive completed"),
                    Err(e) => storage_failure(out, &e),
                }
            }
            ("DELETESCRIPT", 1) => {
                let Some(name) = script_name(args.first()) else { return no(out, Some("NONEXISTENT"), "there is no such script") };
                match sieve.delete(account, &name).await {
                    Ok(true) => ok(out, "Deletescript completed"),
                    Ok(false) => no(out, Some("NONEXISTENT"), "there is no such script"),
                    Err(e) => storage_failure(out, &e),
                }
            }
            ("RENAMESCRIPT", 2) => {
                let (Some(from), Some(to)) = (script_name(args.first()), script_name(args.get(1))) else {
                    return no(out, None, "the name of the script is not valid");
                };
                self.rename(account, &from, &to, out).await;
            }
            ("LISTSCRIPTS" | "GETSCRIPT" | "PUTSCRIPT" | "CHECKSCRIPT" | "HAVESPACE" | "SETACTIVE" | "DELETESCRIPT" | "RENAMESCRIPT", _) => no(out, None, "wrong arguments"),
            _ => no(out, None, "unknown command"),
        }
    }

    /// Not one statement: the new name is written, the active flag moves, the old one goes. A failure between two steps leaves both names (never none).
    async fn rename(&self, account: AccountId, from: &str, to: &str, out: &mut Vec<u8>) {
        let sieve = &self.ctx.sieve;
        let list = match sieve.list(account).await {
            Ok(l) => l,
            Err(e) => return storage_failure(out, &e),
        };
        let Some(old) = list.iter().find(|s| s.name == from) else { return no(out, Some("NONEXISTENT"), "there is no such script") };
        if list.iter().any(|s| s.name == to) {
            return no(out, Some("ALREADYEXISTS"), "a script has that name");
        }
        let body = match sieve.get(account, from).await {
            Ok(Some(b)) => b,
            Ok(None) => return no(out, Some("NONEXISTENT"), "there is no such script"),
            Err(e) => return storage_failure(out, &e),
        };
        let step = async {
            sieve.put(account, to, &body).await?;
            if old.active {
                sieve.set_active(account, Some(to)).await?;
            }
            sieve.delete(account, from).await.map(|_| ())
        };
        match step.await {
            Ok(()) => ok(out, "Renamescript completed"),
            Err(e) => storage_failure(out, &e),
        }
    }
}

enum Refused {
    Bad,
    Throttled,
    Busy,
    Unavailable,
}

async fn write_all(stream: &mut BoxedStream, bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    matches!(
        tokio::time::timeout(WRITE_TIMEOUT, async {
            stream.write_all(bytes).await?;
            stream.flush().await
        })
        .await,
        Ok(Ok(()))
    )
}

/// Serve one connection to its end.
async fn run(mut conn: Conn, mut stream: BoxedStream, shutdown: CancellationToken, limits: Limits) {
    let parse_limits = ParseLimits::default();
    let mut greeting = Vec::new();
    capabilities(&mut greeting, conn.tls, conn.allow_plain);
    ok(&mut greeting, "Rusty ManageSieve ready");
    if !write_all(&mut stream, &greeting).await {
        return;
    }
    let started = tokio::time::Instant::now();
    let mut last_input = started;
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 8 * 1024];
    loop {
        // Every complete command already read, in order (a client may pipeline).
        let mut out = Vec::new();
        let mut next = Step::Continue;
        loop {
            if pending.is_empty() {
                break;
            }
            match parse(&pending, &parse_limits, conn.sasl) {
                Parsed::Need => break,
                Parsed::Bad(why) => {
                    let code = (why == "the literal is too large").then_some("QUOTA/MAXSIZE");
                    reply(&mut out, "NO", code, Some(why));
                    reply(&mut out, "BYE", None, Some("the command could not be read"));
                    next = Step::Close;
                    break;
                }
                Parsed::Done(cmd, used) => {
                    pending.drain(..used);
                    next = conn.handle(cmd, &mut out).await;
                    if next != Step::Continue {
                        break;
                    }
                }
            }
        }
        if !write_all(&mut stream, &out).await {
            return;
        }
        match next {
            Step::Close => return,
            Step::StartTls => {
                // Whatever followed STARTTLS in the same read is an injection attempt (command smuggling): the handshake would otherwise carry it into the encrypted session.
                if !pending.is_empty() {
                    return;
                }
                match tokio::time::timeout(STARTTLS_TIMEOUT, conn.ctx.tls.acceptor().accept(stream)).await {
                    Ok(Ok(tls)) => {
                        stream = Box::new(tls);
                        conn.tls = true;
                        conn.peer.tls = true;
                    }
                    _ => return,
                }
                // RFC 5804 section 2.2: the capabilities are sent again once TLS is on.
                let mut caps = Vec::new();
                capabilities(&mut caps, true, conn.allow_plain);
                ok(&mut caps, "Rusty ManageSieve ready");
                if !write_all(&mut stream, &caps).await {
                    return;
                }
                continue;
            }
            Step::Continue => {}
        }
        let mut deadline = last_input + limits.idle_timeout;
        if conn.account.is_none() {
            deadline = deadline.min(started + limits.login_timeout);
        }
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                let mut b = Vec::new();
                reply(&mut b, "BYE", Some("TRYLATER"), Some("the server is shutting down"));
                let _ = write_all(&mut stream, &b).await;
                return;
            }
            () = tokio::time::sleep_until(deadline) => {
                let mut b = Vec::new();
                reply(&mut b, "BYE", None, Some("autologout"));
                let _ = write_all(&mut stream, &b).await;
                return;
            }
            r = stream.read(&mut buf) => match r {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    last_input = tokio::time::Instant::now();
                    pending.extend_from_slice(buf.get(..n).unwrap_or(&[]));
                }
            },
        }
    }
}
