//! The I/O driver and the Rusty backend of the POP3 session: it reads lines, enforces the line and time limits, upgrades to TLS on `STLS`, and gives the session the password check
//! (throttle, directory) and the Inbox (the mailbox facade) of the IMAP service it runs in. This is the only module that touches a stream or the store.

use std::time::Duration;

use async_trait::async_trait;
use rusty_core::{Error, Flags, MessageId};
use rusty_store::Mailbox;
use rusty_svc::{BoxedStream, Context, MemoryBudget, PeerInfo, ServiceEvent, SessionHandler, Verdict};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::session::{Backend, Entry, FetchError, Fetched, LoginError, Session, Step, Transport};

/// How long a write may make no progress before the peer is given up on.
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the TLS handshake after `STLS` may take.
const STARTTLS_TIMEOUT: Duration = Duration::from_secs(15);
/// Messages asked of the store at a time while the snapshot is taken.
const PAGE: u32 = 1000;
/// Live copies of a message while it is sent: the bytes read from the store, the normalised text, the byte-stuffed reply, and the part of it in the write buffer.
const COPIES: u64 = 4;
/// Message ids sent to one `expunge_many`.
const EXPUNGE_BATCH: usize = 500;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Messages one session is offered (the oldest first); a larger maildrop is downloaded in several sessions as the client deletes.
    pub max_messages: usize,
    /// How long a connection may take to sign in.
    pub login_timeout: Duration,
    /// How long a signed-in connection may be silent (RFC 1939: at least ten minutes).
    pub idle_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { max_messages: 200_000, login_timeout: Duration::from_secs(60), idle_timeout: Duration::from_secs(600) }
    }
}

/// How a listener of this handler is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Port {
    /// 995: the edge ends TLS, the stream is already protected.
    Implicit,
    /// 110: clear text with `STLS`.
    Starttls,
}

/// The POP3 service: one instance serves every connection of a listener.
pub struct Pop3Handler {
    ctx: Context,
    port: Port,
    limits: Limits,
    allow_plain: bool,
    budget: MemoryBudget,
}

impl Pop3Handler {
    pub fn new(ctx: Context, port: Port) -> Pop3Handler {
        // A password is taken only inside TLS, as in the IMAP service (which has no setting for it either).
        Pop3Handler { ctx, port, limits: Limits::default(), allow_plain: false, budget: MemoryBudget::new_mib(1024) }
    }

    /// Share the process-wide memory budget of messages in flight (SMTP DATA and IMAP APPEND draw on it too): a `RETR` reserves what its copies of the message need.
    pub fn with_budget(mut self, budget: MemoryBudget) -> Pop3Handler {
        self.budget = budget;
        self
    }

    pub fn with_limits(mut self, limits: Limits) -> Pop3Handler {
        self.limits = limits;
        self
    }

    /// Accept `USER`/`PASS` in clear text: for a test network only (the tests of the listener use it; nothing in the service sets it).
    pub fn with_plain_login_for_tests(mut self) -> Pop3Handler {
        self.allow_plain = true;
        self
    }
}

#[async_trait]
impl SessionHandler for Pop3Handler {
    async fn handle(&self, stream: BoxedStream, peer: PeerInfo, shutdown: CancellationToken) {
        // The operator can turn POP3 off in the admin web: the listeners stay, a connection is told and closed (RFC 1939: the greeting may be a negative one).
        if !self.ctx.protocols.enabled("pop3").await {
            let mut stream = stream;
            let _ = tokio::time::timeout(Duration::from_secs(5), stream.write_all(b"-ERR POP3 is turned off on this server\r\n")).await;
            let _ = stream.shutdown().await;
            return;
        }
        let transport = if peer.tls || self.port == Port::Implicit { Transport::Tls } else { Transport::Plain { stls: true } };
        let session = Session::new(transport, self.allow_plain);
        let backend = Rusty { ctx: self.ctx.clone(), peer, mailbox: None, max_messages: self.limits.max_messages, budget: self.budget.clone() };
        run(session, backend, stream, shutdown, &self.ctx, self.limits).await;
    }
}

/// The backend over the real directory and mailbox.
struct Rusty {
    ctx: Context,
    peer: PeerInfo,
    mailbox: Option<Mailbox>,
    max_messages: usize,
    budget: MemoryBudget,
}

#[async_trait]
impl Backend for Rusty {
    async fn login(&mut self, user: &str, password: &str) -> Result<(), LoginError> {
        let ip = self.peer.addr.ip();
        let keys = rusty_svc::throttle_keys(&self.ctx.directory, user).await;
        let mut blocked = false;
        for k in &keys {
            blocked |= matches!(self.ctx.rate.check(ip, k).await, Verdict::Blocked { .. });
        }
        if blocked {
            self.ctx.events.emit(ServiceEvent::AuthResult { service: "pop3", peer: self.peer.addr, ok: false });
            return Err(LoginError::Throttled);
        }
        let Some(permit) = self.ctx.rate.try_begin_attempt() else { return Err(LoginError::Busy) };
        let verified = self.ctx.auth.verify(user, password).await;
        drop(permit);
        let account = match verified {
            Ok(Some(a)) => a,
            Ok(None) => {
                for k in &keys {
                    self.ctx.rate.record_failure(ip, k).await;
                }
                self.ctx.events.emit(ServiceEvent::AuthResult { service: "pop3", peer: self.peer.addr, ok: false });
                return Err(LoginError::Bad);
            }
            Err(_) => return Err(LoginError::Unavailable),
        };
        let mailbox = self.ctx.hub.open_for(account).await.map_err(|_| LoginError::Unavailable)?;
        for k in &keys {
            self.ctx.rate.record_success(ip, k).await;
        }
        self.ctx.events.emit(ServiceEvent::AuthResult { service: "pop3", peer: self.peer.addr, ok: true });
        self.mailbox = Some(mailbox);
        Ok(())
    }

    async fn snapshot(&mut self) -> Result<Vec<Entry>, ()> {
        let mailbox = self.mailbox.as_ref().ok_or(())?;
        let folders = mailbox.list_folders().await.map_err(|_| ())?;
        let inbox = folders.iter().find(|f| f.role == "inbox").ok_or(())?;
        let mut entries = Vec::new();
        let mut after = 0u32;
        'pages: loop {
            let page = mailbox.list_folder(inbox.id, after, PAGE).await.map_err(|_| ())?;
            let Some(last) = page.last() else { break };
            after = last.uid;
            for m in page {
                // What the person marked deleted in IMAP and did not expunge is as good as gone.
                if m.flags.bits() & Flags::DELETED.bits() != 0 {
                    continue;
                }
                entries.push(Entry { id: m.id.0, uid: format!("{}.{}", inbox.uid_validity, m.uid), size: u64::try_from(m.size).unwrap_or(0) });
                if entries.len() >= self.max_messages {
                    break 'pages;
                }
            }
        }
        Ok(entries)
    }

    async fn retrieve(&mut self, id: i64, size: u64) -> Result<Fetched, FetchError> {
        let mailbox = self.mailbox.as_ref().ok_or(FetchError::Unavailable)?;
        // Asked for before the message is read: a service that is short of memory says so instead of filling it.
        let permit = self.budget.try_reserve(COPIES.saturating_mul(size)).ok_or(FetchError::Busy)?;
        match mailbox.get_raw(MessageId(id)).await {
            Ok(raw) => Ok(Fetched { raw, hold: Some(Box::new(permit)) }),
            Err(Error::NotFound(_)) => Err(FetchError::Gone),
            Err(_) => Err(FetchError::Unavailable),
        }
    }

    async fn expunge(&mut self, ids: &[i64]) -> Result<(), ()> {
        let mailbox = self.mailbox.as_ref().ok_or(())?;
        for batch in ids.chunks(EXPUNGE_BATCH) {
            let ids: Vec<MessageId> = batch.iter().copied().map(MessageId).collect();
            // A message that is no longer there is not a failure: someone else removed it.
            mailbox.expunge_many(&ids).await.map_err(|_| ())?;
        }
        Ok(())
    }
}

async fn write_all(stream: &mut BoxedStream, bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    matches!(tokio::time::timeout(WRITE_TIMEOUT, async {
        stream.write_all(bytes).await?;
        stream.flush().await
    })
    .await, Ok(Ok(())))
}

/// Serve one connection to its end.
async fn run(mut session: Session, mut backend: Rusty, mut stream: BoxedStream, shutdown: CancellationToken, ctx: &Context, limits: Limits) {
    if !write_all(&mut stream, session.greeting()).await {
        return;
    }
    let started = tokio::time::Instant::now();
    let mut last_input = started;
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 8 * 1024];
    loop {
        // Every complete line already read, in order (a client may pipeline).
        let mut out = Vec::new();
        let mut next = Step::Continue;
        while let Some(at) = pending.iter().position(|b| *b == b'\n') {
            let limit = session.line_limit();
            let mut line: Vec<u8> = pending.drain(..=at).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.len() > limit {
                out.extend_from_slice(b"-ERR line too long\r\n");
                next = Step::Close;
                break;
            }
            next = session.line(&line, &mut backend, &mut out).await;
            if next != Step::Continue {
                break;
            }
        }
        if next == Step::Continue && pending.len() > session.line_limit() {
            out.extend_from_slice(b"-ERR line too long\r\n");
            next = Step::Close;
        }
        if !write_all(&mut stream, &out).await {
            return;
        }
        match next {
            Step::Close => return,
            Step::StartTls => {
                // Whatever followed STLS in the same read is an injection attempt (command smuggling): the handshake would otherwise carry it into the encrypted session.
                if !pending.is_empty() {
                    let _ = write_all(&mut stream, b"-ERR unexpected data after STLS\r\n").await;
                    return;
                }
                match tokio::time::timeout(STARTTLS_TIMEOUT, ctx.tls.acceptor().accept(stream)).await {
                    Ok(Ok(tls)) => {
                        stream = Box::new(tls);
                        session.tls_started();
                        backend.peer.tls = true;
                    }
                    _ => return,
                }
                continue;
            }
            Step::Continue => {}
        }
        let mut deadline = last_input + limits.idle_timeout;
        if !session.is_authenticated() {
            deadline = deadline.min(started + limits.login_timeout);
        }
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                let _ = write_all(&mut stream, b"-ERR [SYS/TEMP] the server is shutting down\r\n").await;
                return;
            }
            () = tokio::time::sleep_until(deadline) => {
                let _ = write_all(&mut stream, b"-ERR [SYS/TEMP] autologout\r\n").await;
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
