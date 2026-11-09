//! What is stored for an accepted message, as pure functions over bytes: line endings normalised
//! to CRLF, our trace headers on top, forged trace headers removed, and the delivery id.
//!
//! The delivery id is derived from the ORIGINAL data (after dot-unstuffing, before anything here
//! touches it), so a remote MTA that retries after a crash between commit and `250` produces the
//! same id and lands on the message that is already stored.

use std::net::IpAddr;

use rusty_core::{BlobHash, DeliveryId};

/// Facts for the trace headers. `mail_from` is empty for the null sender.
#[derive(Debug, Clone, Copy)]
pub struct Trace<'a> {
    pub hostname: &'a str,
    pub helo: &'a str,
    pub peer: IpAddr,
    pub tls: bool,
    pub mail_from: &'a str,
    pub queue_id: &'a str,
    pub received_unix: i64,
    /// The sender authenticated (submission): the client address is not written to the trace.
    pub authenticated: bool,
    /// The `Authentication-Results` value (after the field name) from the inbound SPF/DKIM/DMARC
    /// checks; `None` writes `<hostname>; none`.
    pub auth_results: Option<&'a str>,
}

/// The delivery id of `original` for the recipient account `account` from envelope sender
/// `mail_from` (empty: null sender).
pub fn delivery_id_for(mail_from: &str, account: i64, original: &[u8]) -> DeliveryId {
    DeliveryId::derive(&BlobHash::of(original), &format!("account:{account}"), &format!("from:{mail_from}"))
}

/// Make every line break CRLF: a bare LF gets its CR, and a bare CR gets its LF. A lone CR is a line
/// break to some readers and not to others; leaving it would let a sender hide a forged header from
/// the header filter below.
pub fn normalise_crlf(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 32);
    let mut i = 0;
    while let Some(&b) = data.get(i) {
        match b {
            // CRLF is consumed as a pair; any CR or LF reaching the next arm is a bare one.
            b'\r' if data.get(i + 1) == Some(&b'\n') => {
                out.extend_from_slice(b"\r\n");
                i += 1;
            }
            b'\r' | b'\n' => out.extend_from_slice(b"\r\n"),
            _ => out.push(b),
        }
        i += 1;
    }
    out
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// RFC 5322 date in UTC: `Sun, 8 Nov 2026 12:00:00 +0000`.
pub fn rfc5322_date(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let weekday = DAYS.get(usize::try_from((days + 4).rem_euclid(7)).unwrap_or(0)).copied().unwrap_or("Sun");
    let month = MONTHS.get(usize::try_from(m - 1).unwrap_or(0)).copied().unwrap_or("Jan");
    format!("{weekday}, {d} {month} {y} {:02}:{:02}:{:02} +0000", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// Days since 1970-01-01 to (year, month 1..=12, day 1..=31). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// One header value that cannot break the framing: control characters and parentheses become `?`,
/// and it is cut at `max` characters.
fn sanitize(s: &str, max: usize) -> String {
    s.chars().take(max).map(|c| if c.is_control() || c == '(' || c == ')' { '?' } else { c }).collect()
}

/// A header value on one line: control characters dropped, length capped (comments and parentheses stay).
fn one_line(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

fn is_stripped(name: &[u8]) -> bool {
    let lower: Vec<u8> = name.iter().map(u8::to_ascii_lowercase).collect();
    // `Return-Path` is added by the final delivery step, so the sender's own is forged by definition.
    lower == b"authentication-results" || lower == b"received-spf" || lower == b"return-path" || lower.starts_with(b"x-rusty-")
}

/// Drop the logical headers (a header line plus its folded continuation lines) that this server
/// reserves for itself. `headers` is the header block without the blank line, CRLF-terminated.
fn strip_reserved(headers: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(headers.len());
    let mut dropping = false;
    let mut seen_field = false;
    for line in headers.split_inclusive(|&b| b == b'\n') {
        let continuation = matches!(line.first(), Some(b' ' | b'\t'));
        if continuation && !seen_field {
            // A "continuation" with nothing to continue (the first line of the message) would be glued
            // to whatever header precedes it once our trace headers go on top. Drop it.
            continue;
        }
        if !continuation {
            seen_field = true;
            // The name is what precedes the first colon (the whole line if there is none, so a reserved
            // prefix is dropped even when the colon was damaged), without trailing blanks that lenient
            // parsers ignore.
            let name = line.split(|&b| b == b':').next().unwrap_or_default();
            dropping = is_stripped(name.trim_ascii_end());
        }
        if !dropping {
            out.extend_from_slice(line);
        }
    }
    out
}

/// Build the bytes to store: `Return-Path`, `Received` and `Authentication-Results` on top, then the
/// message with forged trace headers removed and line endings normalised. No recipient address
/// appears in any added header (one stored copy serves every recipient).
pub fn prepare_message(original: &[u8], trace: &Trace<'_>) -> Vec<u8> {
    prepare_message_with(original, trace, &[])
}

/// [`prepare_message`] plus extra headers (filter tags), placed under the trace headers. The
/// caller validated names and values.
pub fn prepare_message_with(original: &[u8], trace: &Trace<'_>, extra: &[(String, String)]) -> Vec<u8> {
    let norm = normalise_crlf(original);
    let (headers, rest): (&[u8], &[u8]) = if norm.starts_with(b"\r\n") {
        (&[], &norm)
    } else if let Some(p) = norm.windows(4).position(|w| w == b"\r\n\r\n") {
        norm.split_at(p + 2)
    } else {
        (&norm, &[])
    };
    let mut kept = strip_reserved(headers);
    if !kept.is_empty() && !kept.ends_with(b"\r\n") {
        kept.extend_from_slice(b"\r\n");
    }

    let ip = match trace.peer {
        IpAddr::V4(a) => format!(" ([{a}])"),
        IpAddr::V6(a) => format!(" ([IPv6:{a}])"),
    };
    // A submitting user's address is not published in the headers of the mail they send.
    let ip = if trace.authenticated { String::new() } else { ip };
    let mut out = Vec::with_capacity(norm.len() + 512);
    out.extend_from_slice(format!("Return-Path: <{}>\r\n", sanitize(trace.mail_from, 320)).as_bytes());
    out.extend_from_slice(
        format!(
            "Received: from {}{ip}\r\n\tby {} with {} id {};\r\n\t{}\r\n",
            sanitize(trace.helo, 255),
            sanitize(trace.hostname, 255),
            match (trace.tls, trace.authenticated) {
                (true, true) => "ESMTPSA",
                (true, false) => "ESMTPS",
                (false, true) => "ESMTPA",
                (false, false) => "ESMTP",
            },
            sanitize(trace.queue_id, 32),
            rfc5322_date(trace.received_unix),
        )
        .as_bytes(),
    );
    match trace.auth_results {
        Some(v) => out.extend_from_slice(format!("Authentication-Results: {}\r\n", one_line(v, 900)).as_bytes()),
        None => out.extend_from_slice(format!("Authentication-Results: {}; none\r\n", sanitize(trace.hostname, 255)).as_bytes()),
    }
    for (name, value) in extra {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(&kept);
    out.extend_from_slice(rest);
    out
}

// ------------------------------------------------------------------ submission checks

/// The logical header fields of the header section, as (lowercase name, raw line start) pairs. Used
/// to count and find fields without parsing values.
fn header_names(original: &[u8]) -> Vec<String> {
    let norm = normalise_crlf(original);
    let head: &[u8] = if norm.starts_with(b"\r\n") {
        &[]
    } else if let Some(p) = norm.windows(4).position(|w| w == b"\r\n\r\n") {
        norm.get(..p + 2).unwrap_or_default()
    } else {
        &norm
    };
    let mut names = Vec::new();
    for line in head.split_inclusive(|&b| b == b'\n') {
        if matches!(line.first(), Some(b' ' | b'\t')) {
            continue;
        }
        if let Some(name) = line.split(|&b| b == b':').next().filter(|_| line.contains(&b':')) {
            names.push(String::from_utf8_lossy(name.trim_ascii_end()).to_ascii_lowercase());
        }
    }
    names
}

/// Does the header section of `original` carry a field called `name` (case-insensitive)?
pub fn has_header(original: &[u8], name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    header_names(original).contains(&name)
}

/// Why the `From` header of a submitted message cannot be checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FromError {
    Missing,
    /// More than one `From` field: mail clients disagree about which one they show.
    Multiple,
    /// No usable address in it.
    Unusable,
}

/// The addresses of the (single) `From` header, lowercase, as the MIME layer reads them.
pub fn from_addresses(original: &[u8]) -> Result<Vec<String>, FromError> {
    let count = header_names(original).iter().filter(|n| *n == "from").count();
    match count {
        0 => return Err(FromError::Missing),
        1 => {}
        _ => return Err(FromError::Multiple),
    }
    let env = rusty_mime::parse_envelope(original).map_err(|_| FromError::Unusable)?;
    let addrs: Vec<String> = env.from.split(", ").filter(|a| !a.is_empty()).map(str::to_owned).collect();
    if addrs.is_empty() {
        return Err(FromError::Unusable);
    }
    Ok(addrs)
}

/// A `Message-ID` for a message that has none: derived from the content hash, so a client that
/// retries after a lost `250` produces the same id.
pub fn generated_message_id(original: &[u8], hostname: &str) -> String {
    let hash = BlobHash::of(original);
    let short: String = hash.as_str().chars().take(24).collect();
    let host: String = hostname.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-').collect();
    format!("<{short}@{host}>")
}
