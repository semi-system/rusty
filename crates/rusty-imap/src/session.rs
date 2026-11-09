//! The session state machine. It takes decoded commands and writes the reply into an [`Out`]; it
//! owns no socket, reads no clock (idle limits belong to the driver) and talks to storage only
//! through the `Mailbox` facade that `MailboxHub` hands out, and to the directory through the
//! injected `Authenticator`. The command handlers live in `cmds_*` modules as further `impl
//! Session` blocks.

use std::sync::Arc;
use std::time::Duration;

use imap_codec::imap_types::auth::AuthMechanism;
use imap_codec::imap_types::command::{Command, CommandBody};
use imap_codec::imap_types::mailbox::Mailbox as WireMailbox;
use rusty_core::{AccountId, Error};
use rusty_store::Mailbox;
use rusty_svc::{Context, PeerInfo, ServiceEvent, Verdict};

use crate::auth;
use crate::errors::{status_for, Op, Status};
use crate::folders::FolderResolver;
use crate::out::{Closed, Out};
use crate::select::Selected;
use crate::utf7;

/// Per-connection limits that are not framing limits.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Failed authentications in one connection before the server hangs up.
    pub auth_failures: u32,
    /// How long an unauthenticated connection may idle.
    pub idle_unauthenticated: Duration,
    /// How long an authenticated connection may idle (RFC 3501: at least 30 minutes).
    pub idle_authenticated: Duration,
    /// SEARCH loads message bodies for criteria the snapshot cannot decide; at most this many
    /// messages and bytes per command, else `NO [LIMIT]` (fails closed, never a partial answer).
    pub search_scan_messages: usize,
    pub search_scan_bytes: u64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            auth_failures: 5,
            idle_unauthenticated: Duration::from_secs(120),
            idle_authenticated: Duration::from_secs(30 * 60),
            search_scan_messages: 20_000,
            search_scan_bytes: 512 * 1024 * 1024,
        }
    }
}

/// What the driver does after the reply has been written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Continue,
    /// Upgrade the connection to TLS now (the OK was already written).
    StartTls,
    /// The next client line is the AUTHENTICATE payload (a continuation was written).
    AuthLine,
    /// Close the connection.
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Ok,
    No,
    Bad,
}

/// The tagged completion of a command.
pub(crate) struct Done {
    pub kind: Kind,
    pub code: Option<String>,
    pub text: String,
    pub action: Action,
}

impl Done {
    pub fn ok(text: &str) -> Done {
        Done { kind: Kind::Ok, code: None, text: text.to_owned(), action: Action::Continue }
    }

    pub fn ok_code(code: &str, text: &str) -> Done {
        Done { kind: Kind::Ok, code: Some(code.to_owned()), text: text.to_owned(), action: Action::Continue }
    }

    pub fn no(code: &str, text: &str) -> Done {
        Done { kind: Kind::No, code: Some(code.to_owned()), text: text.to_owned(), action: Action::Continue }
    }

    pub fn no_plain(text: &str) -> Done {
        Done { kind: Kind::No, code: None, text: text.to_owned(), action: Action::Continue }
    }

    pub fn bad(text: &str) -> Done {
        Done { kind: Kind::Bad, code: None, text: text.to_owned(), action: Action::Continue }
    }

    /// The `NO` of a store error (the one mapping table of `errors.rs`).
    pub fn store(op: Op, e: &Error) -> Done {
        let Status { code, text } = status_for(op, e);
        Done { kind: Kind::No, code: code.map(str::to_owned), text: text.to_owned(), action: Action::Continue }
    }

    pub fn with(mut self, action: Action) -> Done {
        self.action = action;
        self
    }
}

pub(crate) struct User {
    pub account: AccountId,
    pub mailbox: Mailbox,
}

struct PendingAuth {
    tag: String,
}

pub struct Session {
    pub(crate) ctx: Context,
    pub(crate) peer: PeerInfo,
    /// The transport is protected by TLS (implicit TLS port or after STARTTLS).
    pub(crate) tls: bool,
    pub(crate) resolver: Arc<dyn FolderResolver>,
    pub(crate) limits: Limits,
    pub(crate) user: Option<User>,
    pub(crate) sel: Option<Selected>,
    pending: Option<PendingAuth>,
    failed_auths: u32,
}

/// Protocol name of a command (for the log: verbs only, never arguments).
fn verb(body: &CommandBody<'_>) -> &'static str {
    body.name()
}

impl Session {
    pub fn new(ctx: Context, peer: PeerInfo, resolver: Arc<dyn FolderResolver>, limits: Limits) -> Session {
        Session { tls: peer.tls, ctx, peer, resolver, limits, user: None, sel: None, pending: None, failed_auths: 0 }
    }

    pub fn is_authenticated(&self) -> bool {
        self.user.is_some()
    }

    pub fn tls_active(&self) -> bool {
        self.tls
    }

    /// The driver finished a STARTTLS handshake.
    pub fn tls_started(&mut self) {
        self.tls = true;
    }

    pub(crate) fn capabilities(&self) -> String {
        let mut caps = vec!["IMAP4rev1", "SASL-IR", "UNSELECT", "CHILDREN", "MOVE"];
        if self.user.is_none() {
            if self.tls {
                caps.push("AUTH=PLAIN");
            } else {
                if self.ctx.tls.is_available() {
                    caps.push("STARTTLS");
                }
                caps.push("LOGINDISABLED");
            }
        }
        caps.join(" ")
    }

    pub fn greeting(&self, out: &mut Out<'_>) {
        out.untagged(&format!("OK [CAPABILITY {}] Rusty IMAP ready", self.capabilities()));
    }

    /// The server is shutting down: say so.
    pub fn bye_shutdown(&self, out: &mut Out<'_>) {
        out.untagged("BYE Server shutting down");
    }

    /// The idle limit of this connection's current state.
    pub fn idle_limit(&self) -> Duration {
        if self.user.is_some() {
            self.limits.idle_authenticated
        } else {
            self.limits.idle_unauthenticated
        }
    }

    /// A line the framer rejected before it became a command.
    pub fn framing_error(&self, tag: Option<&str>, out: &mut Out<'_>, kind: FramingError) {
        let (status, code, text) = match kind {
            FramingError::LiteralTooLarge => ("NO", Some("LIMIT"), "Literal too large"),
            FramingError::Unparsable => ("BAD", None, "Command not understood"),
        };
        match tag {
            Some(t) => out.tagged(t, status, code, text),
            None => out.untagged(&format!("BAD {text}")),
        }
    }

    pub(crate) fn mailbox(&self) -> Option<Mailbox> {
        self.user.as_ref().map(|u| u.mailbox.clone())
    }

    /// Run one command to completion: handler, pending updates, tagged reply.
    pub async fn command(&mut self, cmd: Command<'_>, out: &mut Out<'_>) -> Result<Action, Closed> {
        let tag = cmd.tag.as_ref().to_owned();
        tracing::debug!(verb = verb(&cmd.body), "imap command");
        let non_uid_scan = matches!(
            &cmd.body,
            CommandBody::Fetch { uid: false, .. } | CommandBody::Store { uid: false, .. } | CommandBody::Search { uid: false, .. }
        );
        let in_selected_only = !matches!(
            &cmd.body,
            CommandBody::Select { .. }
                | CommandBody::Examine { .. }
                | CommandBody::Close
                | CommandBody::Unselect
                | CommandBody::Logout
                | CommandBody::Login { .. }
                | CommandBody::Authenticate { .. }
                | CommandBody::StartTLS
        );
        let done = self.dispatch(&tag, cmd.body, out).await?;
        if done.action == Action::AuthLine {
            // The continuation request is the whole reply for now; the tagged status follows the payload.
            out.flush().await?;
            return Ok(Action::AuthLine);
        }
        if in_selected_only && done.action != Action::Close {
            self.send_updates(out, !non_uid_scan).await?;
        }
        self.finish(&tag, done, out).await
    }

    pub(crate) async fn finish(&mut self, tag: &str, done: Done, out: &mut Out<'_>) -> Result<Action, Closed> {
        let status = match done.kind {
            Kind::Ok => "OK",
            Kind::No => "NO",
            Kind::Bad => "BAD",
        };
        if done.action == Action::Close && done.kind != Kind::Ok {
            out.untagged("BYE Too many failed attempts");
        }
        out.tagged(tag, status, done.code.as_deref(), &done.text);
        out.flush().await?;
        Ok(done.action)
    }

    async fn dispatch(&mut self, tag: &str, body: CommandBody<'_>, out: &mut Out<'_>) -> Result<Done, Closed> {
        use CommandBody as C;
        let authed = self.user.is_some();
        let selected = self.sel.is_some();
        // Commands that are valid in any state.
        match &body {
            C::Capability => {
                out.untagged(&format!("CAPABILITY {}", self.capabilities()));
                return Ok(Done::ok("CAPABILITY completed"));
            }
            C::Noop => return Ok(Done::ok("NOOP completed")),
            C::Logout => {
                out.untagged("BYE Logging out");
                return Ok(Done::ok("LOGOUT completed").with(Action::Close));
            }
            _ => {}
        }
        if !authed {
            return match body {
                C::StartTLS => Ok(self.cmd_starttls(out)),
                C::Login { username, password } => {
                    let name = String::from_utf8_lossy(username.as_ref()).into_owned();
                    let pass = String::from_utf8_lossy(password.declassify().as_ref()).into_owned();
                    Ok(self.cmd_login(&name, &pass).await)
                }
                C::Authenticate { mechanism, initial_response } => {
                    let ir = initial_response.map(|s| s.declassify().to_vec());
                    Ok(self.cmd_authenticate(tag, mechanism, ir, out).await)
                }
                _ => Ok(Done::no("AUTHENTICATIONREQUIRED", "Log in first")),
            };
        }
        if !selected
            && matches!(
                body,
                C::Check | C::Close | C::Unselect | C::Expunge | C::Search { .. } | C::Fetch { .. } | C::Store { .. } | C::Copy { .. } | C::Move { .. }
            )
        {
            return Ok(Done::bad("No mailbox selected"));
        }
        match body {
            C::StartTLS | C::Login { .. } | C::Authenticate { .. } => Ok(Done::bad("Already authenticated")),
            C::Enable { .. } => {
                out.untagged("ENABLED");
                Ok(Done::ok("ENABLE completed"))
            }
            C::Select { mailbox } => self.cmd_select(&mailbox, false, out).await,
            C::Examine { mailbox } => self.cmd_select(&mailbox, true, out).await,
            C::Create { mailbox } => self.cmd_create(&mailbox).await,
            C::Delete { mailbox } => self.cmd_delete(&mailbox).await,
            C::Rename { from, to } => self.cmd_rename(&from, &to).await,
            C::Subscribe { mailbox } => self.cmd_subscribe(&mailbox, true).await,
            C::Unsubscribe { mailbox } => self.cmd_subscribe(&mailbox, false).await,
            C::List { reference, mailbox_wildcard } => self.cmd_list(&reference, &mailbox_wildcard, false, out).await,
            C::Lsub { reference, mailbox_wildcard } => self.cmd_list(&reference, &mailbox_wildcard, true, out).await,
            C::Status { mailbox, item_names } => self.cmd_status(&mailbox, &item_names, out).await,
            C::Append { mailbox, flags, date: _, message } => self.cmd_append(&mailbox, &flags, message.as_ref()).await,
            C::Idle => Ok(Done::no("CANNOT", "IDLE is not available")),
            C::Check => Ok(Done::ok("CHECK completed")),
            C::Close => Ok(self.cmd_close().await),
            C::Unselect => {
                self.sel = None;
                Ok(Done::ok("UNSELECT completed"))
            }
            C::Expunge => self.cmd_expunge(out).await,
            C::Search { charset, criteria, uid } => self.cmd_search(charset.as_ref(), &criteria, uid, out).await,
            C::Fetch { sequence_set, macro_or_item_names, uid } => {
                self.cmd_fetch(&sequence_set, &macro_or_item_names, uid, out).await
            }
            C::Store { sequence_set, kind, response, flags, uid } => {
                self.cmd_store(&sequence_set, kind, response, &flags, uid, out).await
            }
            C::Copy { sequence_set, mailbox, uid } => self.cmd_copy(&sequence_set, &mailbox, uid, false, out).await,
            C::Move { sequence_set, mailbox, uid } => self.cmd_copy(&sequence_set, &mailbox, uid, true, out).await,
            _ => Ok(Done::bad("Command not available")),
        }
    }

    fn cmd_starttls(&mut self, out: &mut Out<'_>) -> Done {
        let _ = out;
        if self.tls {
            return Done::bad("TLS is already active");
        }
        if !self.ctx.tls.is_available() {
            return Done::no("CANNOT", "TLS is not available");
        }
        Done::ok("Begin TLS negotiation now").with(Action::StartTls)
    }

    /// Throttle, verify, open the mailbox. `Ok` carries the account; the `Err` is the tagged `NO`.
    async fn authenticate(&mut self, identity: &str, password: &str) -> Result<(), Done> {
        if !self.tls {
            return Err(Done::no("PRIVACYREQUIRED", "Plaintext authentication is disabled; use TLS"));
        }
        let ip = self.peer.addr.ip();
        if let Verdict::Blocked { .. } = self.ctx.rate.check(ip, identity) {
            self.ctx.events.emit(ServiceEvent::AuthResult { service: "imap", peer: self.peer.addr, ok: false });
            self.failed_auths += 1;
            return Err(Done::no("AUTHENTICATIONFAILED", "Too many failed attempts, try again later"));
        }
        let Some(permit) = self.ctx.rate.try_begin_attempt() else {
            return Err(Done::no("UNAVAILABLE", "Server busy, try again later"));
        };
        let verified = self.ctx.auth.verify(identity, password).await;
        drop(permit);
        let account = match verified {
            Ok(Some(a)) => a,
            Ok(None) => {
                self.ctx.rate.record_failure(ip, identity);
                self.ctx.events.emit(ServiceEvent::AuthResult { service: "imap", peer: self.peer.addr, ok: false });
                self.failed_auths += 1;
                return Err(Done::no("AUTHENTICATIONFAILED", "Authentication failed"));
            }
            Err(e) => return Err(Done::store(Op::Mailbox, &e)),
        };
        let mailbox = match self.ctx.hub.open_for(account).await {
            Ok(m) => m,
            Err(e) => return Err(Done::store(Op::Mailbox, &e)),
        };
        self.ctx.rate.record_success(ip, identity);
        self.ctx.events.emit(ServiceEvent::AuthResult { service: "imap", peer: self.peer.addr, ok: true });
        self.user = Some(User { account, mailbox });
        Ok(())
    }

    fn logged_in(&self) -> Done {
        Done::ok_code(&format!("CAPABILITY {}", self.capabilities()), "Logged in")
    }

    async fn cmd_login(&mut self, user: &str, pass: &str) -> Done {
        match self.authenticate(user, pass).await {
            Ok(()) => self.logged_in(),
            Err(d) => self.after_failure(d),
        }
    }

    /// A failed attempt; too many in one connection close it.
    fn after_failure(&self, d: Done) -> Done {
        if self.failed_auths >= self.limits.auth_failures {
            return d.with(Action::Close);
        }
        d
    }

    async fn cmd_authenticate(
        &mut self,
        tag: &str,
        mechanism: AuthMechanism<'_>,
        initial: Option<Vec<u8>>,
        out: &mut Out<'_>,
    ) -> Done {
        if !self.tls {
            return Done::no("PRIVACYREQUIRED", "Plaintext authentication is disabled; use TLS");
        }
        if mechanism != AuthMechanism::Plain {
            return Done::no("CANNOT", "Unsupported authentication mechanism");
        }
        match initial {
            // SASL-IR: the decoder already decoded the base64 of the initial response.
            Some(raw) => self.plain_payload(&raw).await,
            None => {
                self.pending = Some(PendingAuth { tag: tag.to_owned() });
                out.raw(b"+ \r\n");
                Done::ok("").with(Action::AuthLine)
            }
        }
    }

    /// The decoded PLAIN message.
    async fn plain_payload(&mut self, raw: &[u8]) -> Done {
        let Some(p) = auth::parse_plain_bytes(raw) else {
            self.failed_auths += 1;
            return self.after_failure(Done::no("AUTHENTICATIONFAILED", "Authentication failed"));
        };
        if !p.authzid.is_empty() && !p.authzid.eq_ignore_ascii_case(&p.authcid) {
            self.failed_auths += 1;
            return self.after_failure(Done::no("AUTHENTICATIONFAILED", "Authentication failed"));
        }
        match self.authenticate(&p.authcid, &p.password).await {
            Ok(()) => self.logged_in(),
            Err(d) => self.after_failure(d),
        }
    }

    /// The client's answer to the `+` of AUTHENTICATE.
    pub async fn auth_line(&mut self, line: &[u8], out: &mut Out<'_>) -> Result<Action, Closed> {
        let Some(p) = self.pending.take() else {
            out.untagged("BAD Unexpected data");
            out.flush().await?;
            return Ok(Action::Continue);
        };
        let done = if line == b"*" {
            Done::bad("Authentication cancelled")
        } else if line.len() > auth::MAX_PLAIN_B64 {
            self.failed_auths += 1;
            self.after_failure(Done::bad("Authentication data too long"))
        } else {
            match auth::base64_decode(line) {
                Some(raw) => self.plain_payload(&raw).await,
                None => Done::bad("Invalid base64"),
            }
        };
        self.finish(&p.tag, done, out).await
    }

    /// Resolve a client mailbox name to UTF-8.
    pub(crate) fn name_of(m: &WireMailbox<'_>) -> Option<String> {
        match m {
            WireMailbox::Inbox => Some("INBOX".to_owned()),
            WireMailbox::Other(o) => utf7::decode(std::str::from_utf8(o.as_ref()).ok()?),
        }
    }
}

/// Which framing problem was reported by the framer or the decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramingError {
    LiteralTooLarge,
    Unparsable,
}

