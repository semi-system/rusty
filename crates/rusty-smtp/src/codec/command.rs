//! Command parsing: one framed line to a typed [`Command`]. The grammar is delegated to the
//! protocol-library adapter (`adapter/`; RFC 5321 plus the extensions); this layer adds what a server must decide on top: control
//! bytes and invalid UTF-8 are refused before the library sees them, and any parameter the
//! server does not advertise is refused, never silently ignored.

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
    /// `AUTH mechanism [initial-response]`. `initial` is `Some("")` for the `=` empty response.
    Auth { mechanism: AuthMechanism, initial: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMechanism {
    Plain,
    Login,
    Other,
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
    // The one forward path without a domain that RFC 5321 requires: `RCPT TO:<postmaster>`. The library
    // reports it as an empty path, so it is recognised here, in the codec, and the library stays interchangeable.
    let trimmed = text.trim();
    if trimmed.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("rcpt to:"))
        && trimmed.get(8..).is_some_and(|r| r.trim().eq_ignore_ascii_case("<postmaster>"))
    {
        return Ok(Command::Rcpt { path: "postmaster".to_owned() });
    }
    crate::adapter::smtp::parse_request(line)
}
