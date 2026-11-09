//! The ONLY place `smtp-proto` is named. It translates the library's request grammar into the
//! Rusty-owned [`Command`] / [`ParseError`] of `codec`; nothing of the library appears in any
//! public signature, in the session, or in the tests of the session. Replacing the library by our
//! own parser means rewriting this file and nothing else (the wire conformance tests in
//! `tests/wire_conformance.rs` must pass unchanged).

use smtp_proto::{Error as ProtoError, Request, MAIL_BODY_7BIT, MAIL_BODY_8BITMIME, MAIL_SMTPUTF8};

use crate::codec::{AuthMechanism, Command, MailParams, ParseError};

/// Parse one validated command line (no terminator).
pub(crate) fn parse_request(line: &[u8]) -> Result<Command, ParseError> {
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
        Request::Auth { mechanism, initial_response } => Command::Auth {
            mechanism: if mechanism == smtp_proto::AUTH_PLAIN {
                AuthMechanism::Plain
            } else if mechanism == smtp_proto::AUTH_LOGIN {
                AuthMechanism::Login
            } else {
                AuthMechanism::Other
            },
            initial: match initial_response.as_ref() {
                "" => None,
                "=" => Some(String::new()),
                other => Some(other.to_owned()),
            },
        },
        Request::Lhlo { .. }
        | Request::Etrn { .. }
        | Request::Atrn { .. }
        | Request::Burl { .. } => return Err(ParseError::Unknown),
    })
}
