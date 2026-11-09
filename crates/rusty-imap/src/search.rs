//! SEARCH key evaluation. Pure: the session builds [`Facts`] from the folder snapshot (flags,
//! size, internal date, keywords, positions) and only loads a message's bytes when a key cannot be
//! decided without them; [`eval`] answers `Yes`, `No` or `Needs` (content required) with
//! three-valued logic, so `UNSEEN FROM x` is decided by the flag alone for read messages.
//!
//! Text matching is a case-insensitive substring match on decoded text: header values have their
//! RFC 2047 encoded words decoded, bodies their base64 / quoted-printable transfer encoding. Only
//! UTF-8, US-ASCII and Latin-1 style charsets are decoded; other charsets match as raw bytes.

use std::cell::OnceCell;

use chrono::Datelike;
use imap_codec::imap_types::core::AString;
use imap_codec::imap_types::datetime::NaiveDate;
use imap_codec::imap_types::search::SearchKey;
use rusty_core::Flags;

use crate::headers::{self, Field};
use crate::seqset;
use crate::structure::{self, Mime};

/// What the folder snapshot knows about one message.
#[derive(Debug, Clone, Copy)]
pub struct Facts<'a> {
    pub seq: u32,
    pub uid: u32,
    pub flags: Flags,
    /// Lower-case keywords.
    pub keywords: &'a [String],
    pub size: u64,
    pub internal_date: i64,
}

/// `*` of sequence sets and uid sets.
#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    pub max_seq: u32,
    pub max_uid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tri {
    Yes,
    No,
    /// Cannot be decided without the message's bytes.
    Needs,
}

impl Tri {
    fn of(b: bool) -> Tri {
        if b {
            Tri::Yes
        } else {
            Tri::No
        }
    }

    fn not(self) -> Tri {
        match self {
            Tri::Yes => Tri::No,
            Tri::No => Tri::Yes,
            Tri::Needs => Tri::Needs,
        }
    }

    fn and(self, o: Tri) -> Tri {
        match (self, o) {
            (Tri::No, _) | (_, Tri::No) => Tri::No,
            (Tri::Needs, _) | (_, Tri::Needs) => Tri::Needs,
            _ => Tri::Yes,
        }
    }

    fn or(self, o: Tri) -> Tri {
        match (self, o) {
            (Tri::Yes, _) | (_, Tri::Yes) => Tri::Yes,
            (Tri::Needs, _) | (_, Tri::Needs) => Tri::Needs,
            _ => Tri::No,
        }
    }
}

/// The bytes of one message, prepared lazily for header and body matching.
pub struct Content<'a> {
    raw: &'a [u8],
    mime: OnceCell<Mime>,
    fields: OnceCell<Vec<Field>>,
    body: OnceCell<String>,
}

impl<'a> Content<'a> {
    pub fn new(raw: &'a [u8]) -> Content<'a> {
        Content { raw, mime: OnceCell::new(), fields: OnceCell::new(), body: OnceCell::new() }
    }

    fn mime(&self) -> &Mime {
        self.mime.get_or_init(|| structure::parse(self.raw))
    }

    fn block(&self) -> &[u8] {
        self.raw.get(self.mime().root_header()).unwrap_or(&[])
    }

    fn fields(&self) -> &[Field] {
        self.fields.get_or_init(|| headers::fields(self.block()))
    }

    /// Lower-case decoded values of the headers named `name`.
    fn values(&self, name: &str) -> Vec<String> {
        self.fields().iter().filter(|f| f.is(name)).map(|f| decode_words(&f.value).to_lowercase()).collect()
    }

    /// Decoded, lower-cased text of all text parts.
    fn body_text(&self) -> &str {
        self.body.get_or_init(|| {
            let mut out = String::new();
            for leaf in self.mime().text_leaves() {
                let bytes = self.raw.get(leaf.body.clone()).unwrap_or(&[]);
                let decoded = match leaf.encoding.as_str() {
                    "BASE64" => base64_decode(bytes),
                    "QUOTED-PRINTABLE" => qp_decode(bytes),
                    _ => bytes.to_vec(),
                };
                out.push_str(&to_text(&decoded, leaf.charset.as_deref()).to_lowercase());
                out.push('\n');
            }
            out
        })
    }

    /// Day number (days since 1970-01-01) of the Date header.
    fn sent_day(&self) -> Option<i64> {
        let f = headers::find(self.fields(), "date")?;
        parse_date_header(&f.value)
    }
}

// ---------------------------------------------------------------------------- decoding helpers

fn to_text(bytes: &[u8], charset: Option<&str>) -> String {
    match charset {
        Some("iso-8859-1" | "latin1" | "windows-1252" | "iso-8859-15") => bytes.iter().map(|b| char::from(*b)).collect(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Quoted-printable (RFC 2045); broken escapes pass through.
fn qp_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while let Some(&b) = input.get(i) {
        if b == b'=' {
            let a = input.get(i + 1).copied();
            let c = input.get(i + 2).copied();
            match (a.and_then(hex), c.and_then(hex)) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                _ => {
                    // Soft line break: `=` at the end of a line.
                    if a == Some(b'\r') && c == Some(b'\n') {
                        i += 3;
                        continue;
                    }
                    if a == Some(b'\n') {
                        i += 2;
                        continue;
                    }
                }
            }
        }
        out.push(b);
        i += 1;
    }
    out
}

fn b64_val(b: u8) -> Option<u32> {
    match b {
        b'A'..=b'Z' => Some(u32::from(b - b'A')),
        b'a'..=b'z' => Some(u32::from(b - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(b - b'0') + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Lenient base64: characters outside the alphabet are skipped.
fn base64_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in input {
        if b == b'=' {
            break;
        }
        let Some(v) = b64_val(b) else { continue };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// Decode RFC 2047 encoded words in a header value; everything else is kept (lossily as UTF-8).
fn decode_words(value: &[u8]) -> String {
    let mut out = String::new();
    let mut rest = value;
    let mut last_was_word = false;
    while !rest.is_empty() {
        if let Some(start) = rest.windows(2).position(|w| w == b"=?") {
            let before = rest.get(..start).unwrap_or(&[]);
            if let Some((text, used)) = encoded_word(rest.get(start..).unwrap_or(&[])) {
                // Whitespace between two encoded words is not part of the text.
                if !(last_was_word && before.iter().all(u8::is_ascii_whitespace)) {
                    out.push_str(&String::from_utf8_lossy(before));
                }
                out.push_str(&text);
                rest = rest.get(start + used..).unwrap_or(&[]);
                last_was_word = true;
                continue;
            }
            out.push_str(&String::from_utf8_lossy(rest.get(..start + 2).unwrap_or(&[])));
            rest = rest.get(start + 2..).unwrap_or(&[]);
            last_was_word = false;
        } else {
            out.push_str(&String::from_utf8_lossy(rest));
            break;
        }
    }
    out
}

/// `=?charset?B|Q?text?=` at the start of `s`: the decoded text and the bytes used.
fn encoded_word(s: &[u8]) -> Option<(String, usize)> {
    let body = s.strip_prefix(b"=?")?;
    let mut it = body.splitn(3, |b| *b == b'?');
    let charset = it.next()?;
    let enc = it.next()?;
    let rest = it.next()?;
    let end = rest.windows(2).position(|w| w == b"?=")?;
    let text = rest.get(..end)?;
    let used = 2 + charset.len() + 1 + enc.len() + 1 + end + 2;
    let bytes = match enc {
        b"B" | b"b" => base64_decode(text),
        b"Q" | b"q" => qp_decode(&text.iter().map(|b| if *b == b'_' { b' ' } else { *b }).collect::<Vec<_>>()),
        _ => return None,
    };
    let cs = String::from_utf8_lossy(charset).to_ascii_lowercase();
    Some((to_text(&bytes, Some(cs.as_str())), used))
}

// ---------------------------------------------------------------------------- dates

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn naive_day(d: &NaiveDate) -> i64 {
    let d: &chrono::NaiveDate = d.as_ref();
    days_from_civil(i64::from(d.year()), i64::from(d.month()), i64::from(d.day()))
}

/// The date of an RFC 5322 `Date:` value (`Fri, 06 Nov 2026 10:00:00 +0000`), time and zone ignored.
fn parse_date_header(v: &[u8]) -> Option<i64> {
    let text = String::from_utf8_lossy(v);
    let mut toks = text.split_whitespace().peekable();
    if toks.peek().is_some_and(|t| t.ends_with(',')) {
        toks.next();
    }
    let day: i64 = toks.next()?.parse().ok()?;
    let mon = toks.next()?.to_ascii_lowercase();
    let year: i64 = toks.next()?.parse().ok()?;
    let year = match year {
        0..=49 => year + 2000,
        50..=99 => year + 1900,
        y => y,
    };
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let m = MONTHS.iter().position(|m| mon.starts_with(m))? as i64 + 1;
    (1..=31).contains(&day).then(|| days_from_civil(year, m, day))
}

// ---------------------------------------------------------------------------- evaluation

fn needle(a: &AString<'_>) -> String {
    String::from_utf8_lossy(a.as_ref()).to_lowercase()
}

/// Does deciding `key` ever need the message's bytes?
pub fn needs_content(key: &SearchKey<'_>) -> bool {
    use SearchKey::*;
    match key {
        And(keys) => keys.as_ref().iter().any(needs_content),
        Not(k) => needs_content(k),
        Or(a, b) => needs_content(a) || needs_content(b),
        Bcc(_) | Body(_) | Cc(_) | From(_) | Header(..) | SentBefore(_) | SentOn(_) | SentSince(_) | Subject(_) | Text(_) | To(_) => true,
        _ => false,
    }
}

fn header_has(c: &Content<'_>, name: &str, text: &str) -> bool {
    c.values(name).iter().any(|v| v.contains(text))
}

/// Evaluate `key` for one message. `content` is `None` until the caller loaded the bytes.
pub fn eval(key: &SearchKey<'_>, f: &Facts<'_>, b: Bounds, content: Option<&Content<'_>>) -> Tri {
    use SearchKey::*;
    let flag = |x: Flags| Tri::of(f.flags.contains(x));
    let with = |g: &dyn Fn(&Content<'_>) -> bool| content.map_or(Tri::Needs, |c| Tri::of(g(c)));
    match key {
        And(keys) => keys.as_ref().iter().fold(Tri::Yes, |acc, k| acc.and(eval(k, f, b, content))),
        Not(k) => eval(k, f, b, content).not(),
        Or(x, y) => eval(x, f, b, content).or(eval(y, f, b, content)),
        SequenceSet(set) => Tri::of(!seqset::ranges(set, b.max_seq).iter().all(|(lo, hi)| f.seq < *lo || f.seq > *hi)),
        Uid(set) => Tri::of(!seqset::ranges(set, b.max_uid).iter().all(|(lo, hi)| f.uid < *lo || f.uid > *hi)),
        All => Tri::Yes,
        Answered => flag(Flags::ANSWERED),
        Unanswered => flag(Flags::ANSWERED).not(),
        Deleted => flag(Flags::DELETED),
        Undeleted => flag(Flags::DELETED).not(),
        Draft => flag(Flags::DRAFT),
        Undraft => flag(Flags::DRAFT).not(),
        Flagged => flag(Flags::FLAGGED),
        Unflagged => flag(Flags::FLAGGED).not(),
        Seen => flag(Flags::SEEN),
        Unseen => flag(Flags::SEEN).not(),
        // Nothing is \Recent in this server: every message is old.
        Recent | New => Tri::No,
        Old => Tri::Yes,
        Keyword(k) => Tri::of(has_keyword(f, k.as_ref())),
        Unkeyword(k) => Tri::of(!has_keyword(f, k.as_ref())),
        Larger(n) => Tri::of(f.size > u64::from(*n)),
        Smaller(n) => Tri::of(f.size < u64::from(*n)),
        Before(d) => Tri::of(f.internal_date.div_euclid(86_400) < naive_day(d)),
        On(d) => Tri::of(f.internal_date.div_euclid(86_400) == naive_day(d)),
        Since(d) => Tri::of(f.internal_date.div_euclid(86_400) >= naive_day(d)),
        SentBefore(d) => with(&|c| c.sent_day().is_some_and(|s| s < naive_day(d))),
        SentOn(d) => with(&|c| c.sent_day().is_some_and(|s| s == naive_day(d))),
        SentSince(d) => with(&|c| c.sent_day().is_some_and(|s| s >= naive_day(d))),
        From(s) => with(&|c| header_has(c, "from", &needle(s))),
        To(s) => with(&|c| header_has(c, "to", &needle(s))),
        Cc(s) => with(&|c| header_has(c, "cc", &needle(s))),
        Bcc(s) => with(&|c| header_has(c, "bcc", &needle(s))),
        Subject(s) => with(&|c| header_has(c, "subject", &needle(s))),
        Header(name, value) => with(&|c| {
            let name = String::from_utf8_lossy(name.as_ref()).into_owned();
            let want = needle(value);
            let vals = c.values(&name);
            !vals.is_empty() && (want.is_empty() || vals.iter().any(|v| v.contains(&want)))
        }),
        Body(s) => with(&|c| c.body_text().contains(&needle(s))),
        Text(s) => with(&|c| {
            let n = needle(s);
            c.body_text().contains(&n)
                || c.fields().iter().any(|f| {
                    String::from_utf8_lossy(&f.name).to_lowercase().contains(&n) || decode_words(&f.value).to_lowercase().contains(&n)
                })
        }),
    }
}

fn has_keyword(f: &Facts<'_>, k: &str) -> bool {
    f.keywords.iter().any(|x| x.eq_ignore_ascii_case(k))
}

/// Does `key` look at keywords (so the caller must read them for the folder)?
pub fn needs_keywords(key: &SearchKey<'_>) -> bool {
    use SearchKey::*;
    match key {
        And(keys) => keys.as_ref().iter().any(needs_keywords),
        Not(k) => needs_keywords(k),
        Or(a, b) => needs_keywords(a) || needs_keywords(b),
        Keyword(_) | Unkeyword(_) => true,
        _ => false,
    }
}
