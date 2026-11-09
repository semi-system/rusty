//! Command parsing: one framed line to a typed [`Command`]. The grammar is `smtp-proto`'s
//! (RFC 5321 plus the extensions); this layer adds what a server must decide on top: control
//! bytes and invalid UTF-8 are refused before the library sees them, and any parameter the
//! server does not advertise is refused, never silently ignored.

use smtp_proto::{
    Error as ProtoError, Request, MAIL_BODY_7BIT, MAIL_BODY_8BITMIME, MAIL_SMTPUTF8,
};

/// Parameters of `MAIL FROM` the server understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MailParams {
    /// Declared message size (`SIZE=`), if any.
    pub size: Option<u64>,
    /// `BODY=8BITMIME` (otherwise 7BIT or absent).
    pub body_8bit: bool,
    pub smtputf8: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Ehlo(String),
    Helo(String),
    /// `path` is the reverse path without the angle brackets; empty is the null sender `<>`.
    Mail { path: String, params: MailParams },
    /// `path` is the forward path without the angle brackets, as the client wrote it.
    Rcpt { path: String },
    Data,
    Bdat,
    Rset,
    Noop,
    Quit,
    Vrfy,
    Expn,
    Help,
    StartTls,
    Auth,
}

/// Why a line is not a command. The texts are fixed; input is never echoed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Not a command this server knows (500 5.5.2).
    Unknown,
    /// A known command with bad syntax (501 5.5.4).
    Syntax(&'static str),
    /// A bad address in `MAIL` / `RCPT` (501 5.1.7 / 5.1.3).
    BadAddress,
    /// A well-formed parameter the server does not support (555 5.5.4).
    Unsupported(&'static str),
}

const REFUSED_BYTES: &str = "line contains control characters or invalid text";

/// Parse one command line (terminator already removed).
pub fn parse_command(line: &[u8]) -> Result<Command, ParseError> {
    let text = std::str::from_utf8(line).map_err(|_| ParseError::Syntax(REFUSED_BYTES))?;
    if text.chars().any(|c| c.is_control() && c != '\t') {
        return Err(ParseError::Syntax(REFUSED_BYTES));
    }
    if text.trim().is_empty() {
        return Err(ParseError::Unknown);
    }
    let mut buf = Vec::with_capacity(line.len() + 1);
    buf.extend_from_slice(line);
    buf.push(b'\n');
    let request = Request::parse(&mut buf.iter()).map_err(map_error)?;
    convert(request)
}

fn map_error(e: ProtoError) -> ParseError {
    match e {
        ProtoError::UnknownCommand => ParseError::Unknown,
        ProtoError::InvalidSenderAddress | ProtoError::InvalidRecipientAddress => ParseError::BadAddress,
        ProtoError::SyntaxError { syntax } => ParseError::Syntax(syntax),
        ProtoError::InvalidParameter { param } => ParseError::Syntax(param),
        ProtoError::UnsupportedParameter { .. } => ParseError::Unsupported("parameter not supported"),
        ProtoError::NeedsMoreData { .. } | ProtoError::ResponseTooLong | ProtoError::InvalidResponse { .. } => {
            ParseError::Syntax("incomplete command")
        }
    }
}

fn convert(request: Request<std::borrow::Cow<'_, str>>) -> Result<Command, ParseError> {
    Ok(match request {
        Request::Ehlo { host } => Command::Ehlo(host.into_owned()),
        Request::Helo { host } => Command::Helo(host.into_owned()),
        Request::Mail { from } => {
            let understood = MAIL_BODY_7BIT | MAIL_BODY_8BITMIME | MAIL_SMTPUTF8;
            let extras = from.trans_id.is_some()
                || from.env_id.is_some()
                || from.solicit.is_some()
                || from.mtrk.is_some()
                || from.by != 0
                || from.hold_for != 0
                || from.hold_until != 0
                || from.mt_priority != 0;
            if from.flags & !understood != 0 || extras {
                return Err(ParseError::Unsupported("parameter not supported"));
            }
            Command::Mail {
                path: from.address.into_owned(),
                params: MailParams {
                    size: (from.size != 0).then_some(from.size as u64),
                    body_8bit: from.flags & MAIL_BODY_8BITMIME != 0,
                    smtputf8: from.flags & MAIL_SMTPUTF8 != 0,
                },
            }
        }
        Request::Rcpt { to } => {
            if to.orcpt.is_some() || to.rrvs != 0 || to.flags != 0 {
                return Err(ParseError::Unsupported("parameter not supported"));
            }
            // A forward path is never empty (`<>` is for MAIL only); `smtp-proto` also reports an address
            // without a domain (`<postmaster>`) this way. A bare postmaster is not supported yet.
            if to.address.is_empty() {
                return Err(ParseError::BadAddress);
            }
            Command::Rcpt { path: to.address.into_owned() }
        }
        Request::Data => Command::Data,
        Request::Bdat { .. } => Command::Bdat,
        Request::Rset => Command::Rset,
        Request::Noop { .. } => Command::Noop,
        Request::Quit => Command::Quit,
        Request::Vrfy { .. } => Command::Vrfy,
        Request::Expn { .. } => Command::Expn,
        Request::Help { .. } => Command::Help,
        Request::StartTls => Command::StartTls,
        Request::Auth { .. } => Command::Auth,
        Request::Lhlo { .. }
        | Request::Etrn { .. }
        | Request::Atrn { .. }
        | Request::Burl { .. } => return Err(ParseError::Unknown),
    })
}
