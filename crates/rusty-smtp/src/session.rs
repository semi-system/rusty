//! The sans-I/O SMTP server state machine.
//!
//! The driver feeds bytes with [`Session::feed`] and executes the returned [`Action`]s: write a
//! reply, close, start TLS, or answer an asynchronous question (is this recipient local? store this
//! message) with [`Session::answer`]. While a question is open, further input is held (pipelined
//! commands wait for the answer, in order). The session never reads a clock, opens a socket, or
//! touches a database, so it is fuzzable and the same machine serves port 25 and, later,
//! submission.

use std::fmt;
use std::net::IpAddr;

use rusty_core::AccountId;

use crate::codec::{
    decode_base64, parse_login_value, parse_plain, parse_command, AuthMechanism, Command, DataEnd, DataReader, LineEvent,
    LineReader, MailParams, ParseError, MAX_IDENTITY_BYTES, MAX_PASSWORD_BYTES, MAX_SASL_LINE,
};
use crate::reply::Reply;

/// Most bytes held for commands that arrive while an answer is pending.
const MAX_PENDING_INPUT: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub hostname: String,
    pub max_message_bytes: u64,
    pub max_recipients: usize,
    /// Most distinct mailboxes one message may be delivered to (a list counts all its members).
    pub max_accounts: usize,
    /// Most transactions (accepted `MAIL FROM`) per connection.
    pub max_transactions: u32,
    /// Most commands without a completed transaction.
    pub max_commands_without_transaction: u32,
    /// Most error replies before the connection is closed.
    pub max_errors: u32,
    /// Longest command line, terminator not counted.
    pub max_line: usize,
    /// STARTTLS can work on this listener (a certificate is configured).
    pub starttls_available: bool,
    /// Submission role (587/465): authentication is offered over TLS only and required for MAIL.
    pub submission: bool,
}

/// What the session may know about the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerFacts {
    pub ip: IpAddr,
    /// The connection is protected by TLS (implicit TLS, or after STARTTLS).
    pub tls: bool,
}

/// An accepted recipient: the address as resolved and the accounts that receive the message
/// (more than one for a list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipient {
    pub address: String,
    pub accounts: Vec<AccountId>,
}

/// A password. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: String) -> Secret {
        Secret(s)
    }

    /// The value, for the one place that verifies it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Who authenticated: the account and the address it logged in with (normalised by the driver).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub account: AccountId,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthVerdict {
    Success(Identity),
    Failure(Reply),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RcptVerdict {
    Accept(Recipient),
    /// Submission only: a recipient outside this server, to be queued for delivery.
    AcceptRemote(String),
    Reject(Reply),
}

/// The driver's answer to a question the session asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Rcpt(RcptVerdict),
    /// The reply to the end of DATA (250 when everything is stored).
    Delivery(Reply),
    Auth(AuthVerdict),
    /// May the authenticated user send as this envelope sender?
    Sender(Result<(), Reply>),
}

/// A complete transaction handed to the driver. `data` is the ORIGINAL content (unstuffed, line
/// endings as sent). Its `Debug` never prints content.
#[derive(Clone, PartialEq, Eq)]
pub struct Transaction {
    pub helo: String,
    pub peer: PeerFacts,
    /// Empty for the null sender.
    pub mail_from: String,
    pub recipients: Vec<Recipient>,
    /// Submission only: recipients outside this server.
    pub remote_recipients: Vec<String>,
    /// The authenticated user (submission), `None` on port 25.
    pub identity: Option<Identity>,
    pub data: Vec<u8>,
    pub declared_size: Option<u64>,
    pub smtputf8: bool,
}

impl fmt::Debug for Transaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transaction")
            .field("recipients", &self.recipients.len())
            .field("data_bytes", &self.data.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write this reply.
    Reply(Reply),
    /// Resolve this forward path and call [`Session::answer`] with [`Answer::Rcpt`].
    CheckRcpt { address: String },
    /// Verify these credentials and call [`Session::answer`] with [`Answer::Auth`].
    Authenticate { identity: String, password: Secret },
    /// May the authenticated user send as this address? Answer with [`Answer::Sender`].
    CheckSender { address: String, identity: Identity },
    /// Store this message and call [`Session::answer`] with [`Answer::Delivery`].
    Deliver(Box<Transaction>),
    /// The `220` was queued just before: flush it, run the TLS handshake, then call
    /// [`Session::tls_started`] (or drop the connection if it fails).
    StartTls,
    /// Close the connection after the replies queued before.
    Close,
}

/// What the driver should wait for next (which idle timeout applies, and whether to read at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Command,
    Data,
    /// An answer is pending: do not wait for the peer on its behalf.
    Waiting,
    /// The TLS handshake is running.
    Tls,
    Closed,
}

enum State {
    Command,
    Data(DataReader),
    WaitingRcpt,
    WaitingDelivery,
    WaitingAuth,
    WaitingSender,
    Tls,
    Closed,
}

struct Txn {
    mail_from: String,
    params: MailParams,
    recipients: Vec<Recipient>,
    remote: Vec<String>,
    rcpt_attempts: u32,
}

pub struct Session {
    cfg: SessionConfig,
    peer: PeerFacts,
    state: State,
    lines: LineReader,
    input: Vec<u8>,
    pos: usize,
    helo: Option<String>,
    txn: Option<Txn>,
    errors: u32,
    commands: u32,
    transactions: u32,
    identity: Option<Identity>,
    auth: AuthStep,
    auth_failures: u32,
    pending_mail: Option<(String, MailParams)>,
}

/// Where an AUTH exchange stands (the next client line is SASL data, not a command).
enum AuthStep {
    None,
    PlainResponse,
    LoginUser,
    LoginPassword(String),
}

/// Failed AUTH attempts per connection before it is closed.
const MAX_AUTH_FAILURES: u32 = 3;

impl Session {
    /// A new session and its greeting.
    pub fn new(cfg: SessionConfig, peer: PeerFacts) -> (Session, Vec<Action>) {
        let greeting = Reply::new(220, "", format!("{} ESMTP", cfg.hostname));
        let lines = LineReader::new(cfg.max_line);
        let s = Session {
            cfg,
            peer,
            state: State::Command,
            lines,
            input: Vec::new(),
            pos: 0,
            helo: None,
            txn: None,
            errors: 0,
            commands: 0,
            transactions: 0,
            identity: None,
            auth: AuthStep::None,
            auth_failures: 0,
            pending_mail: None,
        };
        (s, vec![Action::Reply(greeting)])
    }

    pub fn phase(&self) -> Phase {
        match self.state {
            State::Command => Phase::Command,
            State::Data(_) => Phase::Data,
            State::WaitingRcpt | State::WaitingDelivery | State::WaitingAuth | State::WaitingSender => Phase::Waiting,
            State::Tls => Phase::Tls,
            State::Closed => Phase::Closed,
        }
    }

    /// Bytes from the peer.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Action> {
        if matches!(self.state, State::Closed | State::Tls) {
            return Vec::new();
        }
        self.input.extend_from_slice(bytes);
        if self.input.len().saturating_sub(self.pos) > MAX_PENDING_INPUT {
            let mut actions = Vec::new();
            self.refuse_and_close(&mut actions, Reply::new(421, "4.7.0", "Too much input, closing connection"));
            return actions;
        }
        self.pump()
    }

    /// The answer to an [`Action::CheckRcpt`] or [`Action::Deliver`].
    pub fn answer(&mut self, answer: Answer) -> Vec<Action> {
        let mut actions = Vec::new();
        let waiting = match self.state {
            State::WaitingRcpt => 1,
            State::WaitingDelivery => 2,
            State::WaitingAuth => 3,
            State::WaitingSender => 4,
            _ => 0,
        };
        match (waiting, answer) {
            (1, Answer::Rcpt(verdict)) => {
                self.state = State::Command;
                match verdict {
                    RcptVerdict::Accept(r) => {
                        let max = self.cfg.max_accounts;
                        let mut over = false;
                        if let Some(t) = self.txn.as_mut() {
                            // Lists multiply deliveries: bound the distinct mailboxes of the whole message.
                            let mut seen: std::collections::HashSet<AccountId> =
                                t.recipients.iter().flat_map(|x| x.accounts.iter().copied()).collect();
                            seen.extend(r.accounts.iter().copied());
                            if seen.len() > max {
                                over = true;
                            } else {
                                t.recipients.push(r);
                            }
                        }
                        if over {
                            self.error_reply(&mut actions, Reply::new(452, "4.5.3", "Too many deliveries for one message"));
                        } else {
                            actions.push(Action::Reply(Reply::new(250, "2.1.5", "Recipient ok")));
                        }
                    }
                    RcptVerdict::AcceptRemote(address) => {
                        if let Some(t) = self.txn.as_mut() {
                            t.remote.push(address);
                        }
                        actions.push(Action::Reply(Reply::new(250, "2.1.5", "Recipient ok")));
                    }
                    RcptVerdict::Reject(reply) => self.error_reply(&mut actions, reply),
                }
            }
            (3, Answer::Auth(verdict)) => {
                self.state = State::Command;
                match verdict {
                    AuthVerdict::Success(identity) => {
                        self.identity = Some(identity);
                        self.auth_failures = 0;
                        actions.push(Action::Reply(Reply::new(235, "2.7.0", "Authentication successful")));
                    }
                    AuthVerdict::Failure(reply) => {
                        self.auth_failures += 1;
                        self.error_reply(&mut actions, reply);
                        if self.auth_failures >= MAX_AUTH_FAILURES && !matches!(self.state, State::Closed) {
                            self.refuse_and_close(&mut actions, Reply::new(421, "4.7.0", "Too many failed attempts"));
                        }
                    }
                }
            }
            (4, Answer::Sender(verdict)) => {
                self.state = State::Command;
                let pending = self.pending_mail.take();
                match (verdict, pending) {
                    (Ok(()), Some((mail_from, params))) => {
                        self.transactions += 1;
                        self.txn = Some(Txn { mail_from, params, recipients: Vec::new(), remote: Vec::new(), rcpt_attempts: 0 });
                        actions.push(Action::Reply(Reply::new(250, "2.1.0", "Sender ok")));
                    }
                    (Err(reply), _) => self.error_reply(&mut actions, reply),
                    (Ok(()), None) => {}
                }
            }
            (2, Answer::Delivery(reply)) => {
                self.state = State::Command;
                self.txn = None;
                if reply.is_positive() {
                    self.commands = 0;
                    self.errors = 0;
                    actions.push(Action::Reply(reply));
                } else {
                    self.error_reply(&mut actions, reply);
                }
            }
            // An answer nobody asked for: ignore.
            _ => return actions,
        }
        actions.extend(self.pump());
        actions
    }

    /// The TLS handshake finished: RFC 3207 requires the session to forget everything.
    pub fn tls_started(&mut self) -> Vec<Action> {
        if !matches!(self.state, State::Tls) {
            return Vec::new();
        }
        self.helo = None;
        self.txn = None;
        self.identity = None;
        self.auth = AuthStep::None;
        self.pending_mail = None;
        self.peer.tls = true;
        self.input.clear();
        self.pos = 0;
        self.lines = LineReader::new(self.cfg.max_line);
        self.state = State::Command;
        Vec::new()
    }

    /// The server is shutting down.
    pub fn shutdown(&mut self) -> Vec<Action> {
        self.goodbye(Reply::new(421, "4.3.2", format!("{} Service shutting down", self.cfg.hostname)))
    }

    /// The server cannot take more data right now (memory budget): tell the peer to come back later.
    pub fn busy(&mut self) -> Vec<Action> {
        self.goodbye(Reply::new(421, "4.3.2", format!("{} Server busy, try again later", self.cfg.hostname)))
    }

    /// The peer was idle for too long.
    pub fn timed_out(&mut self) -> Vec<Action> {
        self.goodbye(Reply::new(421, "4.4.2", format!("{} Idle timeout, closing connection", self.cfg.hostname)))
    }

    fn goodbye(&mut self, reply: Reply) -> Vec<Action> {
        if matches!(self.state, State::Closed) {
            return Vec::new();
        }
        self.state = State::Closed;
        vec![Action::Reply(reply), Action::Close]
    }

    fn refuse_and_close(&mut self, actions: &mut Vec<Action>, reply: Reply) {
        self.state = State::Closed;
        actions.push(Action::Reply(reply));
        actions.push(Action::Close);
    }

    /// Queue an error reply; too many errors end the session.
    fn error_reply(&mut self, actions: &mut Vec<Action>, reply: Reply) {
        actions.push(Action::Reply(reply));
        self.errors += 1;
        if self.errors >= self.cfg.max_errors {
            self.refuse_and_close(actions, Reply::new(421, "4.7.0", "Too many errors, closing connection"));
        }
    }

    fn pump(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        loop {
            match self.state {
                State::Command => {
                    let event = {
                        let Session { input, pos, lines, .. } = self;
                        let mut slice: &[u8] = input.get(*pos..).unwrap_or_default();
                        let ev = lines.read_line(&mut slice);
                        *pos = input.len() - slice.len();
                        ev
                    };
                    match event {
                        Some(LineEvent::Line(line)) => self.command(&line, &mut actions),
                        Some(LineEvent::TooLong) => {
                            self.error_reply(&mut actions, Reply::new(500, "5.5.2", "Line too long"));
                        }
                        None => break,
                    }
                }
                State::Data(_) => {
                    let end = {
                        let Session { input, pos, state, .. } = self;
                        let State::Data(reader) = state else { break };
                        let mut slice: &[u8] = input.get(*pos..).unwrap_or_default();
                        let end = reader.feed(&mut slice);
                        *pos = input.len() - slice.len();
                        end
                    };
                    match end {
                        Some(end) => self.data_ended(end, &mut actions),
                        None => break,
                    }
                }
                State::WaitingRcpt
                | State::WaitingDelivery
                | State::WaitingAuth
                | State::WaitingSender
                | State::Tls
                | State::Closed => break,
            }
        }
        if self.pos >= self.input.len() {
            self.input.clear();
            self.pos = 0;
        }
        actions
    }

    fn data_ended(&mut self, end: DataEnd, actions: &mut Vec<Action>) {
        let txn = self.txn.take();
        match end {
            DataEnd::TooLarge => {
                self.refuse_and_close(actions, Reply::new(552, "5.3.4", "Message size exceeds fixed limit"));
            }
            DataEnd::Complete { has_nul: true, .. } => {
                self.state = State::Command;
                self.error_reply(actions, Reply::new(554, "5.6.0", "NUL bytes are not allowed in messages"));
            }
            DataEnd::Complete { data, .. } => {
                let Some(txn) = txn else {
                    self.state = State::Command;
                    return;
                };
                self.state = State::WaitingDelivery;
                actions.push(Action::Deliver(Box::new(Transaction {
                    helo: self.helo.clone().unwrap_or_default(),
                    peer: self.peer,
                    mail_from: txn.mail_from,
                    recipients: txn.recipients,
                    remote_recipients: txn.remote,
                    identity: self.identity.clone(),
                    data,
                    declared_size: txn.params.size,
                    smtputf8: txn.params.smtputf8,
                })));
                // The transaction stays "in flight" until the driver answers; keep a marker so a
                // failed delivery resets cleanly.
                self.txn = None;
            }
        }
    }

    fn command(&mut self, line: &[u8], actions: &mut Vec<Action>) {
        if !matches!(self.auth, AuthStep::None) {
            return self.sasl_line(line, actions);
        }
        let command = match parse_command(line) {
            Ok(c) => c,
            Err(e) => {
                let reply = match e {
                    ParseError::Unknown => Reply::new(500, "5.5.2", "Command not recognized"),
                    ParseError::Syntax(_) => Reply::new(501, "5.5.4", "Syntax error in parameters or arguments"),
                    ParseError::BadAddress => Reply::new(501, "5.1.7", "Bad address syntax"),
                    ParseError::Unsupported(_) => Reply::new(555, "5.5.4", "Parameter not supported"),
                };
                self.error_reply(actions, reply);
                return;
            }
        };
        // The cap is for idle chatter. The commands that build a transaction (MAIL, RCPT, DATA) are bounded by
        // their own limits (recipients, transactions) and must not eat into it, or a legitimate message to the
        // advertised number of recipients would be cut off.
        if !matches!(command, Command::Mail { .. } | Command::Rcpt { .. } | Command::Data) {
            self.commands += 1;
            if self.commands > self.cfg.max_commands_without_transaction {
                return self.refuse_and_close(actions, Reply::new(421, "4.7.0", "Too many commands, closing connection"));
            }
        }
        match command {
            Command::Ehlo(host) => self.ehlo(host, true, actions),
            Command::Helo(host) => self.ehlo(host, false, actions),
            Command::Mail { path, params } => self.mail(path, params, actions),
            Command::Rcpt { path } => self.rcpt(path, actions),
            Command::Data => self.data(actions),
            Command::Rset => {
                self.txn = None;
                actions.push(Action::Reply(Reply::new(250, "2.0.0", "Ok")));
            }
            Command::Noop => actions.push(Action::Reply(Reply::new(250, "2.0.0", "Ok"))),
            Command::Quit => {
                self.state = State::Closed;
                actions.push(Action::Reply(Reply::new(221, "2.0.0", "Bye")));
                actions.push(Action::Close);
            }
            Command::Vrfy => actions.push(Action::Reply(Reply::new(
                252,
                "2.5.2",
                "Cannot VRFY user, but will accept message and attempt delivery",
            ))),
            Command::Expn => self.error_reply(actions, Reply::new(502, "5.5.1", "EXPN is not available")),
            Command::Help => actions.push(Action::Reply(Reply::new(
                214,
                "2.0.0",
                "Commands: EHLO HELO MAIL RCPT DATA RSET NOOP QUIT STARTTLS",
            ))),
            Command::Auth { mechanism, initial } => self.auth_command(mechanism, initial, actions),
            Command::Bdat => self.error_reply(actions, Reply::new(502, "5.5.1", "BDAT is not implemented")),
            Command::StartTls => self.starttls(actions),
        }
    }

    fn ehlo(&mut self, host: String, extended: bool, actions: &mut Vec<Action>) {
        self.txn = None;
        self.helo = Some(host);
        if !extended {
            actions.push(Action::Reply(Reply::new(250, "", self.cfg.hostname.clone())));
            return;
        }
        let mut lines = vec![
            self.cfg.hostname.clone(),
            "PIPELINING".to_owned(),
            "8BITMIME".to_owned(),
            "SMTPUTF8".to_owned(),
            format!("SIZE {}", self.cfg.max_message_bytes),
            "ENHANCEDSTATUSCODES".to_owned(),
        ];
        if self.cfg.starttls_available && !self.peer.tls {
            lines.push("STARTTLS".to_owned());
        }
        // Credentials are never offered, let alone accepted, on a connection without TLS.
        if self.cfg.submission && self.peer.tls {
            lines.push("AUTH PLAIN LOGIN".to_owned());
        }
        actions.push(Action::Reply(Reply::multi(250, "", lines)));
    }

    fn mail(&mut self, path: String, params: MailParams, actions: &mut Vec<Action>) {
        if self.helo.is_none() {
            return self.error_reply(actions, Reply::new(503, "5.5.1", "Send EHLO or HELO first"));
        }
        if self.cfg.submission && self.identity.is_none() {
            return self.error_reply(actions, Reply::new(530, "5.7.0", "Authentication required"));
        }
        if self.txn.is_some() {
            return self.error_reply(actions, Reply::new(503, "5.5.1", "Nested MAIL command"));
        }
        if self.transactions >= self.cfg.max_transactions {
            return self.refuse_and_close(actions, Reply::new(421, "4.7.0", "Too many messages on this connection"));
        }
        if params.size.is_some_and(|s| s > self.cfg.max_message_bytes) {
            return self.error_reply(actions, Reply::new(552, "5.3.4", "Message size exceeds fixed limit"));
        }
        if self.cfg.submission {
            // A null sender is for bounces; a submitting user always has a sender, and it must be one
            // of that user's own addresses (the driver knows the aliases).
            if path.is_empty() {
                return self.error_reply(actions, Reply::new(553, "5.7.1", "Null sender not allowed on submission"));
            }
            self.state = State::WaitingSender;
            self.pending_mail = Some((path.clone(), params));
            let Some(identity) = self.identity.clone() else {
                return self.error_reply(actions, Reply::new(530, "5.7.0", "Authentication required"));
            };
            actions.push(Action::CheckSender { address: path, identity });
            return;
        }
        self.transactions += 1;
        self.txn = Some(Txn { mail_from: path, params, recipients: Vec::new(), remote: Vec::new(), rcpt_attempts: 0 });
        actions.push(Action::Reply(Reply::new(250, "2.1.0", "Sender ok")));
    }

    fn rcpt(&mut self, path: String, actions: &mut Vec<Action>) {
        if self.helo.is_none() {
            return self.error_reply(actions, Reply::new(503, "5.5.1", "Send EHLO or HELO first"));
        }
        let max = self.cfg.max_recipients;
        let Some(txn) = self.txn.as_mut() else {
            return self.error_reply(actions, Reply::new(503, "5.5.1", "Need MAIL command first"));
        };
        if txn.recipients.len() + txn.remote.len() >= max {
            return self.error_reply(actions, Reply::new(452, "4.5.3", "Too many recipients"));
        }
        txn.rcpt_attempts += 1;
        self.state = State::WaitingRcpt;
        actions.push(Action::CheckRcpt { address: path });
    }

    fn data(&mut self, actions: &mut Vec<Action>) {
        let Some(txn) = self.txn.as_ref() else {
            return self.error_reply(actions, Reply::new(503, "5.5.1", "Need MAIL command first"));
        };
        if txn.recipients.is_empty() && txn.remote.is_empty() {
            let reply = if txn.rcpt_attempts == 0 {
                Reply::new(503, "5.5.1", "Need RCPT command first")
            } else {
                Reply::new(554, "5.5.1", "No valid recipients")
            };
            return self.error_reply(actions, reply);
        }
        let max = usize::try_from(self.cfg.max_message_bytes).unwrap_or(usize::MAX);
        self.state = State::Data(DataReader::new(max));
        actions.push(Action::Reply(Reply::new(354, "", "End data with <CR><LF>.<CR><LF>")));
    }

    fn starttls(&mut self, actions: &mut Vec<Action>) {
        if self.helo.is_none() {
            return self.error_reply(actions, Reply::new(503, "5.5.1", "Send EHLO first"));
        }
        if self.peer.tls {
            return self.error_reply(actions, Reply::new(503, "5.5.1", "TLS is already active"));
        }
        if !self.cfg.starttls_available {
            return self.error_reply(actions, Reply::new(454, "4.7.0", "TLS not available"));
        }
        // Bytes behind STARTTLS were sent in clear text before the handshake: a command injection
        // attempt (CVE-2011-0411 class). Never process them; end the connection.
        if self.pos < self.input.len() || self.lines.buffered() > 0 {
            return self.refuse_and_close(actions, Reply::new(554, "5.5.0", "Protocol violation: data after STARTTLS"));
        }
        self.state = State::Tls;
        actions.push(Action::Reply(Reply::new(220, "2.0.0", "Ready to start TLS")));
        actions.push(Action::StartTls);
    }
}

// ------------------------------------------------------------------ authentication (submission)

fn bad_auth_data() -> Reply {
    Reply::new(501, "5.5.2", "Cannot decode the authentication response")
}

impl Session {
    fn finish_auth_exchange(&mut self) {
        self.auth = AuthStep::None;
        self.lines.set_max(self.cfg.max_line);
    }

    fn begin_auth(&mut self, step: AuthStep) {
        self.auth = step;
        self.lines.set_max(MAX_SASL_LINE);
    }

    fn auth_command(&mut self, mechanism: AuthMechanism, initial: Option<String>, actions: &mut Vec<Action>) {
        if !self.cfg.submission {
            return self.error_reply(actions, Reply::new(503, "5.5.1", "Authentication is not available on this port"));
        }
        if self.helo.is_none() {
            return self.error_reply(actions, Reply::new(503, "5.5.1", "Send EHLO first"));
        }
        // Never on a connection without TLS, whatever the client asks for.
        if !self.peer.tls {
            return self.error_reply(
                actions,
                Reply::new(538, "5.7.11", "Encryption required for requested authentication mechanism"),
            );
        }
        if self.identity.is_some() {
            return self.error_reply(actions, Reply::new(503, "5.5.1", "Already authenticated"));
        }
        if self.txn.is_some() {
            return self.error_reply(actions, Reply::new(503, "5.5.1", "AUTH not permitted during a mail transaction"));
        }
        match (mechanism, initial) {
            (AuthMechanism::Plain, Some(data)) => {
                self.begin_auth(AuthStep::PlainResponse);
                self.sasl_text(&data, actions);
            }
            (AuthMechanism::Plain, None) => {
                self.begin_auth(AuthStep::PlainResponse);
                actions.push(Action::Reply(Reply::new(334, "", "")));
            }
            (AuthMechanism::Login, Some(data)) => {
                self.begin_auth(AuthStep::LoginUser);
                self.sasl_text(&data, actions);
            }
            (AuthMechanism::Login, None) => {
                self.begin_auth(AuthStep::LoginUser);
                actions.push(Action::Reply(Reply::new(334, "", "VXNlcm5hbWU6"))); // "Username:"
            }
            (AuthMechanism::Other, _) => {
                self.error_reply(actions, Reply::new(504, "5.5.4", "Unrecognized authentication type"));
            }
        }
    }

    /// A client line while an AUTH exchange is open.
    fn sasl_line(&mut self, line: &[u8], actions: &mut Vec<Action>) {
        match std::str::from_utf8(line) {
            Ok(text) => self.sasl_text(text, actions),
            Err(_) => {
                self.finish_auth_exchange();
                self.error_reply(actions, bad_auth_data());
            }
        }
    }

    fn sasl_text(&mut self, text: &str, actions: &mut Vec<Action>) {
        if text == "*" {
            self.finish_auth_exchange();
            return self.error_reply(actions, Reply::new(501, "5.0.0", "Authentication cancelled"));
        }
        let step = std::mem::replace(&mut self.auth, AuthStep::None);
        match step {
            AuthStep::None => {}
            AuthStep::PlainResponse => {
                let creds = decode_base64(text, MAX_IDENTITY_BYTES * 2 + MAX_PASSWORD_BYTES + 2)
                    .and_then(|raw| parse_plain(&raw));
                self.finish_auth_exchange();
                match creds {
                    // An authorisation identity other than the login identity would be impersonation.
                    Ok(c) if c.authzid.is_empty() || c.authzid == c.authcid => {
                        self.state = State::WaitingAuth;
                        actions.push(Action::Authenticate { identity: c.authcid, password: Secret::new(c.password) });
                    }
                    Ok(_) => self.auth_failed(actions),
                    Err(_) => self.error_reply(actions, bad_auth_data()),
                }
            }
            AuthStep::LoginUser => match parse_login_value(text, MAX_IDENTITY_BYTES) {
                Ok(user) => {
                    self.auth = AuthStep::LoginPassword(user);
                    actions.push(Action::Reply(Reply::new(334, "", "UGFzc3dvcmQ6"))); // "Password:"
                }
                Err(_) => {
                    self.finish_auth_exchange();
                    self.error_reply(actions, bad_auth_data());
                }
            },
            AuthStep::LoginPassword(user) => {
                let password = parse_login_value(text, MAX_PASSWORD_BYTES);
                self.finish_auth_exchange();
                match password {
                    Ok(p) => {
                        self.state = State::WaitingAuth;
                        actions.push(Action::Authenticate { identity: user, password: Secret::new(p) });
                    }
                    Err(_) => self.error_reply(actions, bad_auth_data()),
                }
            }
        }
    }

    fn auth_failed(&mut self, actions: &mut Vec<Action>) {
        self.auth_failures += 1;
        self.error_reply(actions, Reply::new(535, "5.7.8", "Authentication credentials invalid"));
        if self.auth_failures >= MAX_AUTH_FAILURES && !matches!(self.state, State::Closed) {
            self.refuse_and_close(actions, Reply::new(421, "4.7.0", "Too many failed attempts"));
        }
    }
}
