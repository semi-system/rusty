//! What the reader shows of a message (docs/design-webmail.md section 3): its headers, the text of the first `text/plain` part and the list of its attachments. **Plain text only**: a message is hostile input, and
//! HTML (which needs a sanitiser and a sandboxed frame) is a later step of its own; an HTML-only message says so and offers the original. Every limit is explicit: the number of headers and of parts, the depth of
//! the nesting and the size of the text, so that no message can make the service work without bound.
//!
//! The parsing is the minimum the reader needs and is written to fail soft: a message that cannot be understood still has its raw text to show.

/// The most header lines read.
const MAX_HEADERS: usize = 200;
/// The deepest nesting of multipart messages followed.
const MAX_DEPTH: usize = 6;
/// The most parts looked at in one message.
const MAX_PARTS: usize = 200;
/// The most text shown.
pub const MAX_TEXT: usize = 256 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    pub from: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub date: String,
    pub message_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub name: Option<String>,
    pub content_type: String,
    /// The size of the content once decoded (an estimate for base64).
    pub size: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct View {
    pub headers: Headers,
    /// The text of the first `text/plain` part that is not an attachment; empty when there is none.
    pub text: String,
    pub text_truncated: bool,
    /// The message has a body, but only as HTML: the reader says so.
    pub html_only: bool,
    pub attachments: Vec<Attachment>,
}

/// The header section and the body of an entity: split at the first empty line.
fn split(raw: &[u8]) -> (&[u8], &[u8]) {
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'\n' {
            // An empty line: the next byte is a line end (LF), or CR LF.
            if raw.get(i + 1) == Some(&b'\n') {
                return (&raw[..=i], &raw[i + 2..]);
            }
            if raw.get(i + 1) == Some(&b'\r') && raw.get(i + 2) == Some(&b'\n') {
                return (&raw[..=i], &raw[i + 3..]);
            }
        }
        i += 1;
    }
    (raw, &[])
}

/// The header lines of a section as (lower-case name, value), folded lines joined.
fn parse_headers(section: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(section);
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, v)) = out.last_mut() {
                if v.len() < 16 * 1024 {
                    v.push(' ');
                    v.push_str(line.trim());
                }
            }
        } else if let Some((n, v)) = line.split_once(':') {
            if out.len() >= MAX_HEADERS {
                break;
            }
            out.push((n.trim().to_ascii_lowercase(), v.trim().to_owned()));
        }
    }
    out
}

fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
    h.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

/// A parameter of a structured header value: `boundary` in `multipart/mixed; boundary="x"`.
fn param(value: &str, name: &str) -> Option<String> {
    for part in value.split(';').skip(1) {
        let Some((k, v)) = part.split_once('=') else { continue };
        if k.trim().eq_ignore_ascii_case(name) {
            let v = v.trim();
            let v = v.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(v);
            return Some(v.to_owned());
        }
    }
    None
}

fn essence(value: &str) -> String {
    value.split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

// ---- decoders ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn quoted_printable(input: &[u8], header_word: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'=' => {
                if let (Some(h), Some(l)) = (input.get(i + 1).copied().and_then(hex), input.get(i + 2).copied().and_then(hex)) {
                    out.push(h * 16 + l);
                    i += 3;
                } else if input.get(i + 1) == Some(&b'\n') {
                    i += 2;
                } else if input.get(i + 1) == Some(&b'\r') && input.get(i + 2) == Some(&b'\n') {
                    i += 3;
                } else {
                    out.push(b'=');
                    i += 1;
                }
            }
            b'_' if header_word => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

fn base64(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in input {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    out
}

/// Bytes as text in the named character set: UTF-8 and the ISO-8859-1 family are exact, anything else is read as UTF-8 with replacement characters.
fn to_text(bytes: &[u8], charset: &str) -> String {
    match charset.trim().to_ascii_lowercase().as_str() {
        "iso-8859-1" | "latin1" | "windows-1252" | "cp1252" | "iso-8859-15" => bytes.iter().map(|&b| char::from(b)).collect(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// RFC 2047 encoded words (`=?utf-8?B?...?=`) in a header value.
pub fn decode_words(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    let mut last_was_word = false;
    while let Some(start) = rest.find("=?") {
        let before = &rest[..start];
        let tail = &rest[start + 2..];
        let decoded = (|| {
            let (charset, tail) = tail.split_once('?')?;
            let (enc, tail) = tail.split_once('?')?;
            let (text, after) = tail.split_once("?=")?;
            let bytes = match enc {
                "B" | "b" => base64(text.as_bytes()),
                "Q" | "q" => quoted_printable(text.as_bytes(), true),
                _ => return None,
            };
            Some((to_text(&bytes, charset.split('*').next().unwrap_or(charset)), after))
        })();
        match decoded {
            Some((text, after)) => {
                // Whitespace between two encoded words is not shown.
                if !(last_was_word && before.trim().is_empty()) {
                    out.push_str(before);
                }
                out.push_str(&text);
                rest = after;
                last_was_word = true;
            }
            None => {
                out.push_str(before);
                out.push_str("=?");
                rest = tail;
                last_was_word = false;
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_body(body: &[u8], cte: &str) -> Vec<u8> {
    match cte.trim().to_ascii_lowercase().as_str() {
        "base64" => base64(body),
        "quoted-printable" => quoted_printable(body, false),
        _ => body.to_vec(),
    }
}

// ---- the walk ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------

struct Walk {
    parts: usize,
    text: Option<String>,
    truncated: bool,
    html_seen: bool,
    attachments: Vec<Attachment>,
}

/// The parts of a multipart body, between the `--boundary` lines.
fn split_parts<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    let delim = format!("--{boundary}");
    let mut parts = Vec::new();
    let mut start: Option<usize> = None;
    let mut pos = 0;
    while pos <= body.len() {
        let line_end = body[pos..].iter().position(|&b| b == b'\n').map_or(body.len(), |p| pos + p);
        let line = &body[pos..line_end];
        let trimmed = line.strip_suffix(b"\r").unwrap_or(line);
        if trimmed.starts_with(delim.as_bytes()) {
            if let Some(s) = start {
                // The line end before the delimiter (CR LF or LF) belongs to the delimiter, not to the part.
                let mut e = pos.saturating_sub(usize::from(pos > 0 && body[pos - 1] == b'\n')).max(s);
                if e > s && body[e - 1] == b'\r' {
                    e -= 1;
                }
                parts.push(&body[s..e]);
            }
            if trimmed[delim.len()..].starts_with(b"--") {
                return parts;
            }
            start = Some((line_end + 1).min(body.len()));
            if parts.len() >= MAX_PARTS {
                return parts;
            }
        }
        if line_end >= body.len() {
            break;
        }
        pos = line_end + 1;
    }
    parts
}

fn walk(entity: &[u8], depth: usize, w: &mut Walk) {
    if w.parts >= MAX_PARTS {
        return;
    }
    w.parts += 1;
    let (head, body) = split(entity);
    let h = parse_headers(head);
    let ctype = header(&h, "content-type").unwrap_or("text/plain");
    let kind = essence(ctype);
    let disposition = header(&h, "content-disposition").unwrap_or("");
    let is_attachment = essence(disposition) == "attachment";
    let name = param(disposition, "filename").or_else(|| param(ctype, "name")).map(|n| decode_words(&n));
    if kind.starts_with("multipart/") {
        if depth >= MAX_DEPTH {
            return;
        }
        let Some(boundary) = param(ctype, "boundary").filter(|b| !b.is_empty() && b.len() <= 200) else { return };
        for part in split_parts(body, &boundary) {
            walk(part, depth + 1, w);
        }
        return;
    }
    let cte = header(&h, "content-transfer-encoding").unwrap_or("7bit");
    if kind == "text/plain" && !is_attachment && w.text.is_none() {
        let bytes = decode_body(body, cte);
        let mut text = to_text(&bytes, &param(ctype, "charset").unwrap_or_else(|| "utf-8".to_owned()));
        if text.len() > MAX_TEXT {
            let mut cut = MAX_TEXT;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            w.truncated = true;
        }
        w.text = Some(text);
        return;
    }
    if kind == "text/html" && !is_attachment {
        w.html_seen = true;
        return;
    }
    let size = if cte.trim().eq_ignore_ascii_case("base64") { body.iter().filter(|b| b.is_ascii_alphanumeric() || **b == b'+' || **b == b'/').count() / 4 * 3 } else { body.len() };
    w.attachments.push(Attachment { name, content_type: kind, size });
}

/// The reader's view of a message.
pub fn view(raw: &[u8]) -> View {
    let (head, _) = split(raw);
    let h = parse_headers(head);
    let get = |n: &str| decode_words(header(&h, n).unwrap_or(""));
    let headers = Headers { from: get("from"), to: get("to"), cc: get("cc"), subject: get("subject"), date: header(&h, "date").unwrap_or("").to_owned(), message_id: header(&h, "message-id").unwrap_or("").to_owned() };
    let mut w = Walk { parts: 0, text: None, truncated: false, html_seen: false, attachments: Vec::new() };
    walk(raw, 0, &mut w);
    View { headers, html_only: w.text.is_none() && w.html_seen, text_truncated: w.truncated, text: w.text.unwrap_or_default(), attachments: w.attachments }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_message_has_its_headers_and_text() {
        let v = view(b"From: =?UTF-8?B?Tmd1eeG7hW4gVsSDbg==?= <a@x.test>\r\nTo: b@y.test\r\nSubject: =?utf-8?Q?Caf=C3=A9_menu?=\r\nDate: Mon, 23 Nov 2026 10:00:00 +0000\r\nMessage-ID: <1@x>\r\n\r\nHello\r\nworld\r\n");
        assert_eq!(v.headers.from, "Nguyễn Văn <a@x.test>");
        assert_eq!(v.headers.subject, "Café menu");
        assert_eq!((v.headers.to.as_str(), v.headers.message_id.as_str()), ("b@y.test", "<1@x>"));
        assert_eq!(v.text, "Hello\r\nworld\r\n");
        assert!(v.attachments.is_empty() && !v.html_only && !v.text_truncated);
    }

    #[test]
    fn a_multipart_message_gives_the_plain_part_and_lists_the_attachments() {
        let raw = b"Subject: s\r\nContent-Type: multipart/mixed; boundary=\"B1\"\r\n\r\npreamble\r\n--B1\r\nContent-Type: multipart/alternative; boundary=B2\r\n\r\n--B2\r\nContent-Type: text/plain; charset=iso-8859-1\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nCaf=E9 =\r\nopen\r\n--B2\r\nContent-Type: text/html\r\n\r\n<b>x</b>\r\n--B2--\r\n--B1\r\nContent-Type: application/pdf; name=\"a.pdf\"\r\nContent-Disposition: attachment; filename=\"=?utf-8?Q?b=C3=A9.pdf?=\"\r\nContent-Transfer-Encoding: base64\r\n\r\nSGVsbG8gd29ybGQh\r\n--B1--\r\nepilogue\r\n";
        let v = view(raw);
        assert_eq!(v.text, "Café open");
        assert_eq!(v.attachments, vec![Attachment { name: Some("bé.pdf".into()), content_type: "application/pdf".into(), size: 12 }]);
        assert!(!v.html_only);
    }

    #[test]
    fn html_only_is_said_and_never_shown_and_an_inline_text_attachment_is_not_the_body() {
        let html = view(b"Content-Type: text/html\r\n\r\n<script>alert(1)</script>\r\n");
        assert!(html.html_only && html.text.is_empty());
        let att = view(b"Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=notes.txt\r\n\r\nsecret notes\r\n--b--\r\n");
        assert_eq!(att.text, "");
        assert_eq!(att.attachments.len(), 1);
    }

    #[test]
    fn hostile_input_is_bounded_and_never_panics() {
        // Deep nesting, a missing boundary, a boundary that never ends, a huge body, and bytes that are not text.
        let mut deep = String::new();
        for i in 0..50 {
            deep.push_str(&format!("Content-Type: multipart/mixed; boundary=b{i}\r\n\r\n--b{i}\r\n"));
        }
        let _ = view(deep.as_bytes());
        let _ = view(b"Content-Type: multipart/mixed\r\n\r\n--x\r\n");
        let _ = view(b"Content-Type: multipart/mixed; boundary=\"\"\r\n\r\n");
        let _ = view(&[0xff, 0xfe, 0x00, b':', b'\n', 0x80]);
        let big = format!("Subject: x\r\n\r\n{}", "a".repeat(MAX_TEXT * 2));
        let v = view(big.as_bytes());
        assert!(v.text_truncated && v.text.len() <= MAX_TEXT);
        let many: String = (0..1000).map(|i| format!("H{i}: v\r\n")).collect();
        let _ = view(many.as_bytes());
        let _ = decode_words("=?utf-8?B?!!!?= =?x");
    }

    #[test]
    fn encoded_words_join_and_a_broken_one_is_left_alone() {
        assert_eq!(decode_words("=?utf-8?Q?a?= =?utf-8?Q?b?="), "ab");
        assert_eq!(decode_words("plain text"), "plain text");
        assert_eq!(decode_words("=?broken"), "=?broken");
    }
}
