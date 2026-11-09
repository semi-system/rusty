//! Response writers: the bytes the server sends. Everything here appends to a `Vec<u8>` and never
//! reads the clock or the store. Strings are written as atoms, quoted strings or literals
//! depending on their content, so arbitrary bytes (8-bit headers, NUL in a body) are always
//! representable; the property tests in `tests/wire_roundtrip.rs` decode this output with
//! the grammar library's response parser.

use rusty_core::Flags;

/// Longest string still written as a quoted string.
const MAX_QUOTED: usize = 1024;
const BACKSLASH: u8 = 0x5c;

/// RFC 3501 `atom-char`s only (non-empty).
pub fn is_atom(s: &[u8]) -> bool {
    !s.is_empty() && s.iter().all(|b| (0x21..=0x7e).contains(b) && *b != BACKSLASH && !b"(){%*\"]".contains(b))
}

/// Append `s` as an IMAP string (quoted when it is plain printable ASCII, else a literal).
pub fn string(out: &mut Vec<u8>, s: &[u8]) {
    if s.len() <= MAX_QUOTED && s.iter().all(|b| (0x20..=0x7e).contains(b)) {
        out.push(b'"');
        for b in s {
            if *b == b'"' || *b == BACKSLASH {
                out.push(BACKSLASH);
            }
            out.push(*b);
        }
        out.push(b'"');
    } else {
        out.extend_from_slice(format!("{{{}}}\r\n", s.len()).as_bytes());
        out.extend_from_slice(s);
    }
}

/// Append `NIL` or the string.
pub fn nstring(out: &mut Vec<u8>, s: Option<&[u8]>) {
    match s {
        Some(s) => string(out, s),
        None => out.extend_from_slice(b"NIL"),
    }
}

/// Append an astring: an atom when it is one, otherwise like [`string`].
pub fn astring(out: &mut Vec<u8>, s: &[u8]) {
    if is_atom(s) {
        out.extend_from_slice(s);
    } else {
        string(out, s);
    }
}

/// A mailbox name (UTF-8 in, modified UTF-7 on the wire). `INBOX` is written upper case.
pub fn mailbox(out: &mut Vec<u8>, utf8_name: &str) {
    let wire = crate::utf7::encode(utf8_name);
    let (head, tail) = match wire.split_once('/') {
        Some((h, t)) => (h, Some(t)),
        None => (wire.as_str(), None),
    };
    if head.eq_ignore_ascii_case("INBOX") {
        let canon = match tail {
            Some(t) => format!("INBOX/{t}"),
            None => "INBOX".to_owned(),
        };
        astring(out, canon.as_bytes());
    } else {
        astring(out, wire.as_bytes());
    }
}

/// `(\Seen \Flagged kw1 kw2)`.
pub fn flag_list(out: &mut Vec<u8>, flags: Flags, keywords: &[String]) {
    out.push(b'(');
    let mut first = true;
    let mut put = |out: &mut Vec<u8>, s: &[u8]| {
        if !first {
            out.push(b' ');
        }
        first = false;
        out.extend_from_slice(s);
    };
    for (flag, name) in [
        (Flags::SEEN, "\\Seen"),
        (Flags::FLAGGED, "\\Flagged"),
        (Flags::ANSWERED, "\\Answered"),
        (Flags::DELETED, "\\Deleted"),
        (Flags::DRAFT, "\\Draft"),
    ] {
        if flags.contains(flag) {
            put(out, name.as_bytes());
        }
    }
    for k in keywords {
        put(out, k.as_bytes());
    }
    out.push(b')');
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Civil date of a day count since 1970-01-01 (proleptic Gregorian, Howard Hinnant's algorithm).
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `"dd-Mon-yyyy hh:mm:ss +0000"` for a unix time (UTC); times outside 1970..=9999 clamp.
pub fn date_time(unix: i64) -> String {
    let t = unix.clamp(0, 253_402_300_799);
    let (y, m, d) = civil_from_days(t.div_euclid(86_400));
    let secs = t.rem_euclid(86_400);
    let month = MONTHS.get((m as usize).saturating_sub(1)).copied().unwrap_or("Jan");
    format!("{d:02}-{month}-{y:04} {:02}:{:02}:{:02} +0000", secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// Like [`string`] but for text taken from message headers: a NUL byte (which no IMAP parser
/// accepts inside a string) becomes a space, everything else is kept.
pub fn text(out: &mut Vec<u8>, s: &[u8]) {
    if s.contains(&0) {
        let clean: Vec<u8> = s.iter().map(|b| if *b == 0 { b' ' } else { *b }).collect();
        string(out, &clean);
    } else {
        string(out, s);
    }
}

/// [`text`] or `NIL`.
pub fn ntext(out: &mut Vec<u8>, s: Option<&[u8]>) {
    match s {
        Some(s) => text(out, s),
        None => out.extend_from_slice(b"NIL"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(f: impl FnOnce(&mut Vec<u8>)) -> String {
        let mut v = Vec::new();
        f(&mut v);
        String::from_utf8_lossy(&v).into_owned()
    }

    #[test]
    fn strings_choose_quoted_or_literal() {
        assert_eq!(run(|o| string(o, b"hello world")), "\"hello world\"");
        assert_eq!(run(|o| string(o, b"say \"hi\" \\")), "\"say \\\"hi\\\" \\\\\"");
        assert_eq!(run(|o| string(o, b"")), "\"\"");
        assert_eq!(run(|o| string(o, b"line\r\nbreak")), "{11}\r\nline\r\nbreak");
        assert_eq!(run(|o| string(o, "d\u{e9}j\u{e0}".as_bytes())), "{6}\r\nd\u{e9}j\u{e0}");
        let mut v = Vec::new();
        string(&mut v, b"nul\0byte");
        assert_eq!(v, b"{8}\r\nnul\0byte");
    }

    #[test]
    fn nil_and_astring() {
        assert_eq!(run(|o| nstring(o, None)), "NIL");
        assert_eq!(run(|o| nstring(o, Some(b"x y"))), "\"x y\"");
        assert_eq!(run(|o| astring(o, b"INBOX")), "INBOX");
        assert_eq!(run(|o| astring(o, b"a b")), "\"a b\"");
        assert_eq!(run(|o| astring(o, b"")), "\"\"");
        assert_eq!(run(|o| astring(o, b"a]b")), "\"a]b\"", "] is a resp-special, not an atom char for astring here");
    }

    #[test]
    fn mailbox_names_are_utf7_and_inbox_is_canonical() {
        assert_eq!(run(|o| mailbox(o, "Inbox")), "INBOX");
        assert_eq!(run(|o| mailbox(o, "INBOX")), "INBOX");
        assert_eq!(run(|o| mailbox(o, "Sent")), "Sent");
        assert_eq!(run(|o| mailbox(o, "A B")), "\"A B\"");
        assert_eq!(run(|o| mailbox(o, "Tom & Jerry")), "\"Tom &- Jerry\"");
        assert!(run(|o| mailbox(o, "\u{110}\u{e3}")).starts_with("&"));
    }

    #[test]
    fn flags_lists() {
        assert_eq!(run(|o| flag_list(o, Flags::empty(), &[])), "()");
        assert_eq!(
            run(|o| flag_list(o, Flags::SEEN | Flags::FLAGGED | Flags::DELETED, &["$label1".into()])),
            "(\\Seen \\Flagged \\Deleted $label1)"
        );
        assert_eq!(
            run(|o| flag_list(o, Flags::ANSWERED | Flags::DRAFT, &[])),
            "(\\Answered \\Draft)"
        );
    }

    #[test]
    fn internal_dates() {
        assert_eq!(date_time(0), "01-Jan-1970 00:00:00 +0000");
        assert_eq!(date_time(1_700_000_000), "14-Nov-2023 22:13:20 +0000");
        assert_eq!(date_time(951_782_400), "29-Feb-2000 00:00:00 +0000");
        assert_eq!(date_time(-5), "01-Jan-1970 00:00:00 +0000");
    }

    #[test]
    fn atoms() {
        assert!(is_atom(b"INBOX") && is_atom(b"a.b-c") && is_atom(b"$label1"));
        assert!(!is_atom(b"") && !is_atom(b"a b") && !is_atom(b"a(b") && !is_atom(b"a\"b") && !is_atom(b"a]b") && !is_atom(b"a\\b") && !is_atom(b"a*"));
    }
}

