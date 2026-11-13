//! The parser for replies of a REMOTE server (client side of SMTP). Owned code (the library parser
//! is request-oriented and its response receiver is awkward with partial reads), pure and stateless:
//! give it the bytes read so far and it returns one complete reply and how many bytes it used, or asks
//! for more. Hostile servers are the threat model: every limit is enforced before anything grows.
//! Fuzzed by `fuzz_smtp_response`.

/// Longest single reply line accepted (RFC 5321 says 512; some servers write more).
pub const MAX_LINE: usize = 2048;
/// Most lines in one reply.
pub const MAX_LINES: usize = 64;
/// Most bytes in one reply.
pub const MAX_REPLY: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub code: u16,
    /// RFC 3463 status of the first line (`5.1.1`), when the server sent one.
    pub enhanced: Option<String>,
    /// The text of every line (code and status removed), joined with a space.
    pub text: String,
    /// Each line's text, for EHLO capability parsing.
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyError {
    /// A line is not `NNN`, then a space or hyphen.
    Malformed,
    /// Lines of one reply carry different codes.
    CodeChanged,
    LineTooLong,
    TooManyLines,
    TooLong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// 2yz
    Positive,
    /// 3yz
    Intermediate,
    /// 4yz
    Transient,
    /// 5yz
    Permanent,
    /// anything else (1yz, 6yz...): treated as a protocol violation by the client
    Other,
}

impl Reply {
    pub fn class(&self) -> Class {
        match self.code / 100 {
            2 => Class::Positive,
            3 => Class::Intermediate,
            4 => Class::Transient,
            5 => Class::Permanent,
            _ => Class::Other,
        }
    }

    /// `code status text`, cut and cleaned, for a log line or a DSN diagnostic.
    pub fn summary(&self) -> String {
        let status = self.enhanced.as_deref().map(|s| format!("{s} ")).unwrap_or_default();
        let text: String = self.text.chars().filter(|c| !c.is_control()).take(200).collect();
        format!("{} {status}{text}", self.code)
    }
}

/// The enhanced status code at the start of `text` (`5.1.1 ...`), and the rest.
fn split_enhanced(text: &str) -> (Option<String>, &str) {
    let mut it = text.splitn(2, ' ');
    let first = it.next().unwrap_or_default();
    let ok = {
        let parts: Vec<&str> = first.split('.').collect();
        parts.len() == 3
            && parts.first().is_some_and(|c| matches!(*c, "2" | "4" | "5"))
            && parts.iter().skip(1).all(|p| !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit()))
    };
    if ok {
        (Some(first.to_owned()), it.next().unwrap_or_default())
    } else {
        (None, text)
    }
}

/// Parse one reply from the start of `buf`.
///
/// * `Ok(Some((reply, used)))`: a complete reply that took `used` bytes (more bytes may follow).
/// * `Ok(None)`: not complete yet; read more.
/// * `Err(_)`: the server is not speaking SMTP (or is hostile); drop the connection.
pub fn parse_reply(buf: &[u8]) -> Result<Option<(Reply, usize)>, ReplyError> {
    let mut pos = 0usize;
    let mut code: Option<u16> = None;
    let mut lines: Vec<String> = Vec::new();
    let mut enhanced: Option<String> = None;
    loop {
        let rest = buf.get(pos..).unwrap_or_default();
        let Some(nl) = rest.iter().position(|&b| b == b'\n') else {
            // No end of line yet: too much without one is an attack, not slowness.
            if rest.len() > MAX_LINE {
                return Err(ReplyError::LineTooLong);
            }
            return Ok(None);
        };
        if nl + 1 > MAX_LINE {
            return Err(ReplyError::LineTooLong);
        }
        let raw = rest.get(..nl).unwrap_or_default();
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        pos += nl + 1;
        if pos > MAX_REPLY {
            return Err(ReplyError::TooLong);
        }
        let digits = raw.get(..3).ok_or(ReplyError::Malformed)?;
        if !digits.iter().all(u8::is_ascii_digit) {
            return Err(ReplyError::Malformed);
        }
        let this = digits.iter().fold(0u16, |a, d| a * 10 + u16::from(d - b'0'));
        if !(200..600).contains(&this) && !(100..200).contains(&this) {
            return Err(ReplyError::Malformed);
        }
        match code {
            None => code = Some(this),
            Some(c) if c != this => return Err(ReplyError::CodeChanged),
            Some(_) => {}
        }
        let last = match raw.get(3) {
            None => true,
            Some(b' ') => true,
            Some(b'-') => false,
            Some(_) => return Err(ReplyError::Malformed),
        };
        let text = String::from_utf8_lossy(raw.get(4..).unwrap_or_default()).into_owned();
        let text = if lines.is_empty() {
            let (e, rest) = split_enhanced(&text);
            enhanced = e;
            rest.to_owned()
        } else {
            text
        };
        if lines.len() >= MAX_LINES {
            return Err(ReplyError::TooManyLines);
        }
        lines.push(text);
        if last {
            let text = lines.join(" ");
            return Ok(Some((Reply { code: code.unwrap_or(0), enhanced, text, lines }, pos)));
        }
    }
}
