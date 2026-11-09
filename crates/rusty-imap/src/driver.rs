//! The I/O driver: reads a stream, frames and decodes commands, hands them to the [`Session`],
//! upgrades to TLS on request and applies the idle and write timeouts. This is the only module
//! that touches the stream.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_svc::{BoxedStream, Context, PeerInfo, SessionHandler};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::command::ParseError;
use rusty_svc::MemoryBudget;
use crate::folders::{FolderResolver, PersonalNamespace};
use crate::frame::{tag_of, FatalReason, Frame, FrameLimits, Framer, Mode, RejectReason};
use crate::out::{Closed, Out, Sink};
use crate::session::{Action, FramingError, IdleCaps, Limits, Session};

/// How long a write may block before the peer is given up on.
/// Live copies of an APPEND literal while it is handled: the framer buffer, the frame, the parsed
/// command (the store takes the last one by move).
const COPIES: u64 = 3;
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a STARTTLS handshake may take.
const STARTTLS_TIMEOUT: Duration = Duration::from_secs(15);

struct StreamSink<'a>(&'a mut BoxedStream);

#[async_trait]
impl Sink for StreamSink<'_> {
    async fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match tokio::time::timeout(WRITE_TIMEOUT, async {
            self.0.write_all(bytes).await?;
            self.0.flush().await
        })
        .await
        {
            Ok(r) => r,
            Err(_) => Err(std::io::ErrorKind::TimedOut.into()),
        }
    }
}

/// The IMAP service: one instance serves every connection of a listener.
pub struct ImapHandler {
    ctx: Context,
    resolver: Arc<dyn FolderResolver>,
    limits: Limits,
    caps: Arc<IdleCaps>,
    budget: MemoryBudget,
}

impl ImapHandler {
    /// Built from the shared `Context`; the folder namespace defaults to the personal one.
    pub fn new(ctx: Context) -> ImapHandler {
        ImapHandler { ctx, resolver: Arc::new(PersonalNamespace), limits: Limits::default(), caps: Arc::default(), budget: MemoryBudget::new_mib(1024) }
    }

    pub fn with_resolver(mut self, resolver: Arc<dyn FolderResolver>) -> ImapHandler {
        self.resolver = resolver;
        self
    }

    /// Share the process-wide in-flight message memory budget (SMTP DATA draws on it too).
    pub fn with_budget(mut self, budget: MemoryBudget) -> ImapHandler {
        self.budget = budget;
        self
    }

    pub fn with_limits(mut self, limits: Limits) -> ImapHandler {
        self.limits = limits;
        self
    }
}

fn frame_limits(ctx: &Context) -> FrameLimits {
    FrameLimits::new(usize::try_from(ctx.cfg.max_message_bytes).unwrap_or(usize::MAX))
}

#[async_trait]
impl SessionHandler for ImapHandler {
    async fn handle(&self, stream: BoxedStream, peer: PeerInfo, shutdown: CancellationToken) {
        let session = Session::new(self.ctx.clone(), peer, self.resolver.clone(), self.limits, self.caps.clone());
        run(session, frame_limits(&self.ctx), stream, shutdown, &self.ctx, &self.budget).await;
    }
}

async fn write_bye(stream: &mut BoxedStream, text: &str) {
    let _ = StreamSink(stream).write(format!("* BYE {text}\r\n").as_bytes()).await;
}

/// Serve one connection to its end.
async fn run(mut session: Session, limits: FrameLimits, stream: BoxedStream, shutdown: CancellationToken, ctx: &Context, budget: &MemoryBudget) {
    // Memory reserved for the message literal of the command in flight (about three copies of it).
    let mut reserved: Option<tokio::sync::OwnedSemaphorePermit> = None;
    let mut stream = stream;
    let mut framer = Framer::new(limits);
    {
        let mut sink = StreamSink(&mut stream);
        let mut out = Out::new(&mut sink);
        session.greeting(&mut out);
        if out.flush().await.is_err() {
            return;
        }
    }
    let mut buf = vec![0u8; 16 * 1024];
    let mut last_input = tokio::time::Instant::now();
    let started = last_input;
    let mut unauth_seen: u32 = 0;
    let (unauth_total, unauth_max) = session.unauth_limits();
    loop {
        framer.set_authenticated(session.is_authenticated());
        match framer.next() {
            Frame::NeedMore => {
                let mut deadline = last_input + session.idle_limit();
                if !session.is_authenticated() {
                    deadline = deadline.min(started + unauth_total);
                }
                let poll = session.idle_poll_every();
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => {
                        write_bye(&mut stream, "Server shutting down").await;
                        return;
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        write_bye(&mut stream, "Autologout; idle for too long").await;
                        return;
                    }
                    r = stream.read(&mut buf) => match r {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            last_input = tokio::time::Instant::now();
                            framer.push(buf.get(..n).unwrap_or(&[]));
                        }
                    },
                    _ = session.idle_wait() => {
                        if !push_updates(&mut session, &mut stream).await {
                            return;
                        }
                    }
                    _ = tokio::time::sleep(poll), if session.is_idling() => {
                        if !push_updates(&mut session, &mut stream).await {
                            return;
                        }
                    }
                }
            }
            Frame::SendContinuation { len, append } => {
                if append {
                    match budget.try_reserve(COPIES * len as u64) {
                        Some(p) => reserved = Some(p),
                        None => {
                            // Refused before the client sends the data.
                            let tag = framer.reject_pending();
                            let mut sink = StreamSink(&mut stream);
                            let mut out = Out::new(&mut sink);
                            session.busy(tag.as_deref(), &mut out);
                            if out.flush().await.is_err() {
                                return;
                            }
                            continue;
                        }
                    }
                }
                let mut sink = StreamSink(&mut stream);
                if sink.write(b"+ Ready for literal data\r\n").await.is_err() {
                    return;
                }
            }
            Frame::NonSyncLiteral { len, append } => {
                if append {
                    match budget.try_reserve(COPIES * len as u64) {
                        Some(p) => reserved = Some(p),
                        None => {
                            write_bye(&mut stream, "Server busy").await;
                            return;
                        }
                    }
                }
            }
            Frame::Fatal(why) => {
                let text = match why {
                    FatalReason::LineTooLong => "Command line too long",
                    FatalReason::LiteralTooLarge => "Literal too large",
                    FatalReason::CommandTooLarge => "Command too large",
                };
                write_bye(&mut stream, text).await;
                return;
            }
            Frame::Reject { tag, reason } => {
                let RejectReason::LiteralTooLarge = reason;
                let mut sink = StreamSink(&mut stream);
                let mut out = Out::new(&mut sink);
                session.framing_error(tag.as_deref(), &mut out, FramingError::LiteralTooLarge);
                if out.flush().await.is_err() {
                    return;
                }
            }
            Frame::Line(line) => {
                let action = {
                    let mut sink = StreamSink(&mut stream);
                    let mut out = Out::new(&mut sink);
                    if session.is_idling() {
                        session.idle_line(&line, &mut out).await
                    } else {
                        session.auth_line(&line, &mut out).await
                    }
                };
                match action {
                    Ok(Action::Close) | Err(Closed) => return,
                    Ok(_) => {}
                }
                // IDLE ended: back to commands.
            }
            Frame::Command(bytes) => {
                if !session.is_authenticated() {
                    unauth_seen += 1;
                    if unauth_seen > unauth_max {
                        write_bye(&mut stream, "Too many commands before login").await;
                        return;
                    }
                }
                let _held = reserved.take();
                let decoded = crate::adapter::parse_command(&bytes);
                let action = {
                    let mut sink = StreamSink(&mut stream);
                    let mut out = Out::new(&mut sink);
                    match decoded {
                        Ok(cmd) => session.command(cmd, &mut out).await,
                        Err(e) => {
                            let tag = tag_of(&bytes);
                            let kind = if e == ParseError::BadName { FramingError::BadName } else { FramingError::Unparsable };
                            session.framing_error(tag.as_deref(), &mut out, kind);
                            out.flush().await.map(|()| Action::Continue)
                        }
                    }
                };
                match action {
                    Err(Closed) | Ok(Action::Close) => return,
                    Ok(Action::Continue) => {}
                    Ok(Action::AuthLine | Action::Idle) => framer.set_mode(Mode::Line),
                    Ok(Action::StartTls) => {
                        // Anything the client sent after STARTTLS before the handshake is an
                        // injection attempt (CVE-style command smuggling): refuse to continue.
                        if framer.buffered() != 0 {
                            write_bye(&mut stream, "Unexpected data after STARTTLS").await;
                            return;
                        }
                        let acceptor = ctx.tls.acceptor();
                        match tokio::time::timeout(STARTTLS_TIMEOUT, acceptor.accept(stream)).await {
                            Ok(Ok(tls)) => {
                                stream = Box::new(tls);
                                session.tls_started();
                            }
                            _ => return,
                        }
                    }
                }
            }
        }
    }
}

/// Send pending updates while idling; `false` when the peer is gone.
async fn push_updates(session: &mut Session, stream: &mut BoxedStream) -> bool {
    let mut sink = StreamSink(stream);
    let mut out = Out::new(&mut sink);
    session.idle_poll(&mut out).await.is_ok()
}
