//! The message a person composes (docs/design-webmail.md, sending), built from the plain fields of the form and nothing else: the sender is the signed-in address, every address is checked, every header value is a
//! single line without control characters (so no field can add a header), a subject that is not plain ASCII is encoded as RFC 2047 words, the text is UTF-8 in base64. There is no way to say "Bcc" in the
//! message: the blind copies are envelope recipients only.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use rand_core::{OsRng, RngCore};
use rusty_directory::parse_address;

pub const MAX_RECIPIENTS: usize = 100;
pub const MAX_SUBJECT: usize = 300;
pub const MAX_REFERENCE: usize = 255;

/// What the form carries.
#[derive(Debug, Default, Clone)]
pub struct Draft {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub text: String,
    /// The `Message-ID` of the message this answers, with its angle brackets.
    pub in_reply_to: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Invalid {
    NoRecipient,
    TooManyRecipients,
    Address,
    Subject,
    Reference,
}

/// An address as `local@domain` in its normal form, or `None`.
fn address(a: &str) -> Option<String> {
    let a = a.trim();
    if a.is_empty() || a.len() > 254 || a.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>' | ',' | ';' | '"' | '(' | ')' | '\\')) {
        return None;
    }
    let (l, d) = parse_address(a).ok()?;
    Some(format!("{l}@{d}"))
}

fn addresses(list: &[String]) -> Result<Vec<String>, Invalid> {
    list.iter().map(|a| address(a).ok_or(Invalid::Address)).collect()
}

/// A header value that is plain: one line, printable ASCII or tab.
fn plain(s: &str) -> bool {
    s.bytes().all(|b| b == b'\t' || (0x20..0x7f).contains(&b))
}

fn word_encoded(s: &str) -> String {
    let mut out = Vec::new();
    let mut chunk = String::new();
    for c in s.chars() {
        if chunk.len() + c.len_utf8() > 45 {
            out.push(std::mem::take(&mut chunk));
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        out.push(chunk);
    }
    out.iter().map(|p| format!("=?UTF-8?B?{}?=", STANDARD.encode(p))).collect::<Vec<_>>().join("\r\n ")
}

fn subject_header(s: &str) -> Result<String, Invalid> {
    if s.chars().count() > MAX_SUBJECT || s.chars().any(|c| c.is_control()) {
        return Err(Invalid::Subject);
    }
    Ok(if plain(s) { s.to_owned() } else { word_encoded(s) })
}

fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `Date:` value for a unix time (UTC).
pub fn rfc5322_date(unix: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    let wd = DAYS[usize::try_from((days + 4).rem_euclid(7)).unwrap_or(0)];
    let mo = MONTHS[usize::try_from(m - 1).unwrap_or(0)];
    format!("{wd}, {d} {mo} {y} {:02}:{:02}:{:02} +0000", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// A message ready to be queued.
pub struct Composed {
    pub bytes: Vec<u8>,
    /// Every envelope recipient (to, cc and bcc), once each.
    pub recipients: Vec<String>,
    pub subject: String,
}

/// Build the message of `draft` sent from `from` (the signed-in address) at `now` (unix seconds).
pub fn build(from: &str, draft: &Draft, now: i64) -> Result<Composed, Invalid> {
    compose(from, draft, now, false)
}

/// Build the message to keep in the Drafts folder: the same, but it may have no recipient yet, and the blind copies are written down so that the draft can be opened again as it was. A draft is never sent as it
/// is: sending builds the message again from the form.
pub fn build_draft(from: &str, draft: &Draft, now: i64) -> Result<Composed, Invalid> {
    compose(from, draft, now, true)
}

fn compose(from: &str, draft: &Draft, now: i64, saving: bool) -> Result<Composed, Invalid> {
    let from = address(from).ok_or(Invalid::Address)?;
    let (to, cc, bcc) = (addresses(&draft.to)?, addresses(&draft.cc)?, addresses(&draft.bcc)?);
    let mut all: Vec<String> = Vec::new();
    for a in to.iter().chain(&cc).chain(&bcc) {
        if !all.contains(a) {
            all.push(a.clone());
        }
    }
    if all.is_empty() && !saving {
        return Err(Invalid::NoRecipient);
    }
    if all.len() > MAX_RECIPIENTS {
        return Err(Invalid::TooManyRecipients);
    }
    let subject = subject_header(&draft.subject)?;
    let reference = match &draft.in_reply_to {
        Some(r) if r.len() <= MAX_REFERENCE && r.starts_with('<') && r.ends_with('>') && plain(r) && !r.contains(char::is_whitespace) => Some(r.as_str()),
        Some(_) => return Err(Invalid::Reference),
        None => None,
    };
    let domain = from.rsplit_once('@').map_or("localhost", |(_, d)| d);
    let mut id = [0u8; 16];
    OsRng.fill_bytes(&mut id);
    let mut h = String::new();
    h.push_str(&format!("From: {from}\r\n"));
    if !to.is_empty() {
        h.push_str(&format!("To: {}\r\n", to.join(", ")));
    }
    if !cc.is_empty() {
        h.push_str(&format!("Cc: {}\r\n", cc.join(", ")));
    }
    if saving && !bcc.is_empty() {
        h.push_str(&format!("Bcc: {}\r\n", bcc.join(", ")));
    }
    h.push_str(&format!("Subject: {subject}\r\n"));
    h.push_str(&format!("Date: {}\r\n", rfc5322_date(now)));
    h.push_str(&format!("Message-ID: <{}@{domain}>\r\n", hex::encode(id)));
    if let Some(r) = reference {
        h.push_str(&format!("In-Reply-To: {r}\r\nReferences: {r}\r\n"));
    }
    h.push_str("MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n");
    let body = STANDARD.encode(draft.text.as_bytes());
    let mut bytes = h.into_bytes();
    for line in body.as_bytes().chunks(76) {
        bytes.extend_from_slice(line);
        bytes.extend_from_slice(b"\r\n");
    }
    Ok(Composed { bytes, recipients: all, subject: draft.subject.chars().take(200).collect() })
}

/// Now, in unix seconds.
pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Draft {
        Draft { to: vec!["Bob@One.Test".into()], subject: "Hello".into(), text: "Hi Bob\r\n".into(), ..Draft::default() }
    }

    fn text(c: &Composed) -> String {
        String::from_utf8(c.bytes.clone()).unwrap()
    }

    #[test]
    fn a_plain_message_has_the_headers_of_the_form_and_a_base64_body() {
        let c = build("alice@one.test", &draft(), 1_700_000_000).unwrap();
        let t = text(&c);
        assert!(t.starts_with("From: alice@one.test\r\nTo: bob@one.test\r\nSubject: Hello\r\nDate: Tue, 14 Nov 2023 22:13:20 +0000\r\nMessage-ID: <"), "{t}");
        assert!(t.contains("Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\nSGkgQm9iDQo=\r\n"));
        assert_eq!(c.recipients, ["bob@one.test"]);
    }

    #[test]
    fn blind_copies_are_recipients_and_never_a_header_and_duplicates_count_once() {
        let mut d = draft();
        d.cc = vec!["bob@one.test".into()];
        d.bcc = vec!["secret@two.test".into()];
        let c = build("alice@one.test", &d, 0).unwrap();
        assert_eq!(c.recipients.len(), 2);
        assert!(!text(&c).to_ascii_lowercase().contains("bcc") && !text(&c).contains("secret@two.test"));
        assert!(text(&c).contains("Cc: bob@one.test\r\n"));
    }

    #[test]
    fn no_field_can_add_a_header_or_break_the_envelope() {
        for bad in ["a@b.test\r\nBcc: x@y.test", "a@b.test, c@d.test", "a b@c.test", "<a@b.test>", "no-at-sign", "", "a@b.test;c@d.test", "\"a\"@b.test"] {
            let mut d = draft();
            d.to = vec![bad.into()];
            assert_eq!(build("alice@one.test", &d, 0).err(), Some(Invalid::Address), "{bad:?}");
        }
        let mut d = draft();
        d.subject = "x\r\nBcc: y@z.test".into();
        assert_eq!(build("alice@one.test", &d, 0).err(), Some(Invalid::Subject));
        let mut d = draft();
        d.in_reply_to = Some("<a@b>\r\nBcc: x".into());
        assert_eq!(build("alice@one.test", &d, 0).err(), Some(Invalid::Reference));
        d.in_reply_to = Some("a@b".into());
        assert_eq!(build("alice@one.test", &d, 0).err(), Some(Invalid::Reference));
    }

    #[test]
    fn the_recipients_are_between_one_and_a_hundred() {
        let mut d = draft();
        d.to.clear();
        assert_eq!(build("alice@one.test", &d, 0).err(), Some(Invalid::NoRecipient));
        d.to = (0..101).map(|i| format!("u{i}@x.test")).collect();
        assert_eq!(build("alice@one.test", &d, 0).err(), Some(Invalid::TooManyRecipients));
    }

    #[test]
    fn a_subject_that_is_not_ascii_is_encoded_in_words_that_stay_short() {
        let mut d = draft();
        d.subject = "Xin chào các bạn, đây là một tiêu đề khá dài để thử việc chia từ mã hóa".into();
        let t = text(&build("alice@one.test", &d, 0).unwrap());
        let subject = t.split("\r\nDate:").next().unwrap().split("Subject: ").nth(1).unwrap();
        assert!(subject.starts_with("=?UTF-8?B?") && subject.split("\r\n ").count() > 1);
        assert!(subject.split("\r\n ").all(|w| w.len() <= 76));
    }

    #[test]
    fn a_draft_may_have_no_recipient_and_keeps_its_blind_copies_but_a_message_does_not() {
        let mut d = draft();
        d.to.clear();
        d.bcc = vec!["quiet@one.test".into()];
        let t = text(&build_draft("alice@one.test", &d, 0).unwrap());
        assert!(!t.contains("\r\nTo:") && t.contains("Bcc: quiet@one.test\r\n"));
        d.bcc.clear();
        assert!(build_draft("alice@one.test", &d, 0).unwrap().recipients.is_empty());
        assert_eq!(build("alice@one.test", &d, 0).err(), Some(Invalid::NoRecipient));
    }

    #[test]
    fn a_reply_names_the_message_it_answers() {
        let mut d = draft();
        d.in_reply_to = Some("<abc@one.test>".into());
        let t = text(&build("alice@one.test", &d, 0).unwrap());
        assert!(t.contains("In-Reply-To: <abc@one.test>\r\nReferences: <abc@one.test>\r\n"));
    }
}
