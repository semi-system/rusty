#![forbid(unsafe_code)]

//! Envelope extraction from raw RFC 5322 messages, wrapping `mail-parser`.
//!
//! Bounds (hostile input):
//! - Only the header section is parsed (`MessageParser::parse_headers`); MIME parts, nested
//!   multiparts and `message/rfc822` bodies are never built. The header section is the
//!   input up to and including the first empty line, and it must fit in
//!   [`MAX_HEADER_BYTES`] (256 KiB); a longer one is `Error::Invalid` and nothing is parsed.
//!   CPU and memory are therefore O(header bytes), independent of body size, part count and
//!   nesting depth.
//! - Every output field is sanitized and capped: no C0/C1 control characters anywhere,
//!   [`MAX_SUBJECT_BYTES`] for the subject, [`MAX_ADDRESS_FIELD_BYTES`] and
//!   [`MAX_ADDRESSES`] for `from`/`to` (an address with a format or line-separator
//!   character, or one larger than the byte cap on its own, is dropped and later ones are
//!   still kept), subject line separators become spaces and bidi overrides are removed, [`MAX_ID_BYTES`] per message id, the last
//!   [`MAX_REFERENCES`] references.

use mail_parser::{Address, HeaderValue, MessageParser};
use rusty_core::{Error, Result};

/// Largest accepted header section (including the empty line that ends it), in bytes.
pub const MAX_HEADER_BYTES: usize = 256 * 1024;
/// `Envelope::subject` is cut to at most this many bytes (on a char boundary).
pub const MAX_SUBJECT_BYTES: usize = 2048;
/// `Envelope::from` / `Envelope::to` are at most this many bytes; trailing whole addresses
/// are dropped to fit.
pub const MAX_ADDRESS_FIELD_BYTES: usize = 4096;
/// `Envelope::from` / `Envelope::to` hold at most this many addresses.
pub const MAX_ADDRESSES: usize = 100;
/// Longest kept message id (RFC 5322 line length limit); longer ids are dropped.
pub const MAX_ID_BYTES: usize = 998;
/// `Envelope::references` keeps only the last this many ids.
pub const MAX_REFERENCES: usize = 32;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Envelope {
    /// Decoded subject without control characters (TAB becomes a space), runs of spaces
    /// collapsed, trimmed, at most [`MAX_SUBJECT_BYTES`].
    pub subject: String,
    /// Lowercase bare addresses joined with ", " (see [`parse_envelope`]).
    pub from: String,
    pub to: String,
    /// `None` if absent, empty, longer than [`MAX_ID_BYTES`] or containing a control or
    /// whitespace character.
    pub message_id: Option<String>,
    /// The first valid id in the `In-Reply-To` header. Note for threading: the direct
    /// parent is usually the LAST id in `references`, not this value.
    pub in_reply_to: Option<String>,
    /// The last [`MAX_REFERENCES`] valid ids (same rules as `message_id`), in header order.
    pub references: Vec<String>,
    pub date: Option<i64>,
}

/// The header section of `raw`: the bytes up to and including the first empty line
/// (`\n` or `\r\n` at the start of a line), or all of `raw` if it has no empty line and is
/// at most [`MAX_HEADER_BYTES`] long. A header section longer than [`MAX_HEADER_BYTES`] is
/// `Error::Invalid`. Only these bytes are ever handed to the MIME parser.
pub fn header_section(raw: &[u8]) -> Result<&[u8]> {
    let window = &raw[..raw.len().min(MAX_HEADER_BYTES)];
    if let Some(end) = header_end(window) {
        return Ok(&raw[..end]);
    }
    if raw.len() <= MAX_HEADER_BYTES {
        Ok(raw)
    } else {
        Err(Error::Invalid("header section exceeds the size limit".into()))
    }
}

/// Offset just past the first empty line in `w`, if any.
fn header_end(w: &[u8]) -> Option<usize> {
    let mut line = 0;
    loop {
        let rest = &w[line..];
        if rest.starts_with(b"\n") {
            return Some(line + 1);
        }
        if rest.starts_with(b"\r\n") {
            return Some(line + 2);
        }
        line += rest.iter().position(|&b| b == b'\n')? + 1;
    }
}

/// Bidirectional overrides (U+202A..U+202E) and isolates (U+2066..U+2069): they can reorder
/// displayed text (spoofing). ZWJ (U+200D) and ZWNJ (U+200C) are not included: Persian,
/// Arabic and other scripts need them.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Unicode format (Cf) characters and line/paragraph separators (Zl, Zp): none belongs in an
/// email address. Covers the Cf code points of Unicode 15/16 (the standard library has no
/// general-category lookup).
fn is_format_or_line_separator(c: char) -> bool {
    matches!(c,
        '\u{AD}' | '\u{600}'..='\u{605}' | '\u{61C}' | '\u{6DD}' | '\u{70F}' | '\u{890}'..='\u{891}'
        | '\u{8E2}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}'
        | '\u{110BD}' | '\u{110CD}' | '\u{13430}'..='\u{1343F}' | '\u{1BCA0}'..='\u{1BCA3}'
        | '\u{1D173}'..='\u{1D17A}' | '\u{E0001}' | '\u{E0020}'..='\u{E007F}')
}

/// Drop control characters (TAB, NEL, U+2028 and U+2029 become spaces; bidi overrides and
/// isolates are removed), collapse runs of spaces, trim, and cut
/// to at most `max` bytes on a char boundary. Stops reading once `max` is reached.
fn clean_text(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in s.chars() {
        // TAB and the line/paragraph separators (U+0085 NEL, U+2028, U+2029) read as spaces.
        let c = if matches!(c, '\t' | '\u{85}' | '\u{2028}' | '\u{2029}') { ' ' } else { c };
        if c.is_control() || is_bidi_control(c) {
            continue;
        }
        if c == ' ' {
            space = !out.is_empty();
            continue;
        }
        let need = c.len_utf8() + usize::from(space);
        if out.len() + need > max {
            break;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
    }
    out
}

/// A message id without `<>`, or `None` if it is empty, too long, or contains a control
/// or whitespace character.
fn clean_id(s: &str) -> Option<String> {
    let s = s.trim().trim_matches(|c| c == '<' || c == '>').trim();
    let bad = s.is_empty()
        || s.len() > MAX_ID_BYTES
        || s.chars().any(|c| c.is_control() || c.is_whitespace());
    (!bad).then(|| s.to_string())
}

fn raw_ids<'a>(v: &'a HeaderValue<'_>) -> Box<dyn DoubleEndedIterator<Item = &'a str> + 'a> {
    match v {
        HeaderValue::Text(t) => Box::new(std::iter::once(t.as_ref())),
        HeaderValue::TextList(l) => Box::new(l.iter().map(|s| s.as_ref())),
        _ => Box::new(std::iter::empty()),
    }
}

/// Lowercase bare addresses joined with ", ". Addresses that are empty or contain a control
/// character, whitespace, ',', a Unicode format character or a line/paragraph separator are
/// skipped whole; so is an address that alone exceeds [`MAX_ADDRESS_FIELD_BYTES`] (later
/// addresses are still considered). At most [`MAX_ADDRESSES`] addresses and
/// [`MAX_ADDRESS_FIELD_BYTES`] bytes are kept, stopping before the first address that would
/// overflow the remaining room (an address is never cut).
fn join_addresses(addr: Option<&Address<'_>>) -> String {
    let mut out = String::new();
    let Some(a) = addr else { return out };
    let mut n = 0;
    for x in a.iter() {
        let Some(raw) = x.address.as_deref() else {
            continue;
        };
        let s = raw.replace(['<', '>'], "").trim().to_lowercase();
        if s.is_empty()
            || s.len() > MAX_ADDRESS_FIELD_BYTES
            || s.chars().any(|c| {
                c.is_control() || c.is_whitespace() || c == ',' || is_format_or_line_separator(c)
            })
        {
            continue;
        }
        let sep = if out.is_empty() { 0 } else { 2 };
        if n == MAX_ADDRESSES || out.len() + sep + s.len() > MAX_ADDRESS_FIELD_BYTES {
            break;
        }
        if sep > 0 {
            out.push_str(", ");
        }
        out.push_str(&s);
        n += 1;
    }
    out
}

/// Parse the envelope fields of a raw message.
///
/// Behavior:
/// - Empty input is `Error::Invalid`; so is a header section over [`MAX_HEADER_BYTES`]
///   (see [`header_section`]). Any other input returns `Ok`. Only the header section is
///   parsed; the body is never looked at.
/// - Malformed or missing headers yield empty strings / `None` / empty vec.
/// - Address fields are lowercase bare addresses joined with ", "; display-name-only
///   entries, group syntax with no members (`undisclosed-recipients:;`) and empty
///   headers yield an empty string. With several `From` headers the first wins.
/// - Empty ids (`<>`) and invalid ids are dropped, so they yield `None` / are skipped.
/// - Field sanitization and caps: see the crate docs and [`Envelope`].
/// - `date` is unix seconds (negative before 1970), `None` if unparseable.
pub fn parse_envelope(raw: &[u8]) -> Result<Envelope> {
    if raw.is_empty() {
        return Err(Error::Invalid("empty message".into()));
    }
    let head = header_section(raw)?;
    let Some(msg) = MessageParser::default().parse_headers(head) else {
        return Ok(Envelope::default());
    };
    let mut references: Vec<String> = raw_ids(msg.references())
        .rev()
        .filter_map(clean_id)
        .take(MAX_REFERENCES)
        .collect();
    references.reverse();
    let in_reply_to = raw_ids(msg.in_reply_to()).find_map(clean_id);
    Ok(Envelope {
        subject: clean_text(msg.subject().unwrap_or_default(), MAX_SUBJECT_BYTES),
        from: join_addresses(msg.from()),
        to: join_addresses(msg.to()),
        message_id: msg.message_id().and_then(clean_id),
        in_reply_to,
        references,
        date: msg
            .date()
            .filter(|d| d.is_valid())
            .map(|d| d.to_timestamp()),
    })
}
