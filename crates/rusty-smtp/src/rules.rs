//! The mail rules of an account (docs/design-mail-rules.md): forwarding to other addresses and an automatic reply while the person is away. They are one JSON object, the key `mail_rules` of
//! the account's own configuration, so the admin web and (later) the webmail write the same thing and delivery reads it:
//!
//! ```json
//! { "forward": { "to": ["me@other.example"], "keep": true },
//!   "vacation": { "enabled": true, "subject": "Away", "body": "Back on Monday.", "from": 1700000000, "until": 1700500000 } }
//! ```
//!
//! This module is the pure part: reading and checking the object, deciding whether a message may be answered or forwarded, and building the reply.

use serde_json::Value;

use crate::message::{header_count, header_values, rfc5322_date};

/// The key of the account configuration.
pub const KEY: &str = "mail_rules";
/// Most addresses one account forwards to.
pub const MAX_FORWARD: usize = 10;
pub const MAX_SUBJECT: usize = 200;
pub const MAX_BODY: usize = 5000;
/// A forwarded message carries this header with the address that forwarded it, once per hop.
pub const FORWARDED_HEADER: &str = "X-Rusty-Forwarded";
/// Hops a message may take through forwarding accounts before it is a loop.
pub const MAX_FORWARD_HOPS: usize = 3;
/// One automatic reply per sender and this long (seconds).
pub const REPLY_WINDOW_SECS: i64 = 7 * 86_400;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Forward {
    pub to: Vec<String>,
    /// Keep a copy in the mailbox (the delivery already made it); `false` is only a statement of what the person wants, the copy is kept in version 1.
    pub keep: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vacation {
    pub enabled: bool,
    pub subject: String,
    pub body: String,
    /// Unix seconds; `None`: no start or no end.
    pub from: Option<i64>,
    pub until: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailRules {
    pub forward: Option<Forward>,
    pub vacation: Option<Vacation>,
}

fn address_ok(a: &str) -> bool {
    let Some((local, domain)) = a.rsplit_once('@') else { return false };
    a.len() <= 254
        && !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && a.bytes().all(|b| b.is_ascii_graphic() && b != b'<' && b != b'>' && b != b'"' && b != b',' && b != b';' && b != b'\\')
}

impl MailRules {
    /// Read the object as stored; anything that is not shaped right is left out (delivery never fails because a rule is odd).
    pub fn from_value(v: &Value) -> MailRules {
        let forward = v.get("forward").and_then(|f| {
            let mut to: Vec<String> = Vec::new();
            for a in f.get("to")?.as_array()?.iter().filter_map(|x| x.as_str()).map(|s| s.trim().to_ascii_lowercase()).filter(|s| address_ok(s)) {
                if to.len() < MAX_FORWARD && !to.contains(&a) {
                    to.push(a);
                }
            }
            (!to.is_empty()).then(|| Forward { to, keep: f.get("keep").and_then(Value::as_bool).unwrap_or(true) })
        });
        let vacation = v.get("vacation").map(|x| Vacation {
            enabled: x.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            subject: x.get("subject").and_then(Value::as_str).unwrap_or_default().chars().take(MAX_SUBJECT).collect(),
            body: x.get("body").and_then(Value::as_str).unwrap_or_default().chars().take(MAX_BODY).collect(),
            from: x.get("from").and_then(Value::as_i64),
            until: x.get("until").and_then(Value::as_i64),
        });
        MailRules { forward, vacation }
    }

    /// Check an object a person or an administrator sends: `Err` with a fixed word for the first thing wrong. The stored form is [`MailRules::to_value`] of what was read.
    pub fn validate(v: &Value) -> Result<MailRules, &'static str> {
        let obj = v.as_object().ok_or("not an object")?;
        if obj.keys().any(|k| k != "forward" && k != "vacation") {
            return Err("unknown field");
        }
        if let Some(f) = obj.get("forward").filter(|f| !f.is_null()) {
            let to = f.get("to").and_then(Value::as_array).ok_or("forward needs addresses")?;
            if to.is_empty() || to.len() > MAX_FORWARD {
                return Err("forward needs one to ten addresses");
            }
            if !to.iter().all(|x| x.as_str().is_some_and(|s| address_ok(s.trim()))) {
                return Err("forward has an address that is not one");
            }
        }
        if let Some(x) = obj.get("vacation").filter(|x| !x.is_null()) {
            let subject = x.get("subject").and_then(Value::as_str).unwrap_or_default();
            let body = x.get("body").and_then(Value::as_str).unwrap_or_default();
            if subject.chars().count() > MAX_SUBJECT || subject.contains(['\r', '\n']) {
                return Err("the subject is too long or has a line break");
            }
            if body.chars().count() > MAX_BODY {
                return Err("the text is too long");
            }
            if x.get("enabled").and_then(Value::as_bool).unwrap_or(false) && body.trim().is_empty() {
                return Err("an automatic reply needs a text");
            }
            if let (Some(a), Some(b)) = (x.get("from").and_then(Value::as_i64), x.get("until").and_then(Value::as_i64)) {
                if b <= a {
                    return Err("the end is before the start");
                }
            }
        }
        Ok(MailRules::from_value(v))
    }

    pub fn to_value(&self) -> Value {
        let mut o = serde_json::Map::new();
        if let Some(f) = &self.forward {
            o.insert("forward".into(), serde_json::json!({ "to": f.to, "keep": f.keep }));
        }
        if let Some(v) = &self.vacation {
            o.insert("vacation".into(), serde_json::json!({ "enabled": v.enabled, "subject": v.subject, "body": v.body, "from": v.from, "until": v.until }));
        }
        Value::Object(o)
    }
}

impl Vacation {
    /// Is the automatic reply on at `now`?
    pub fn active(&self, now: i64) -> bool {
        self.enabled && self.from.is_none_or(|f| now >= f) && self.until.is_none_or(|u| now < u)
    }
}

/// Is this a message a machine wrote or a list or a bulk sender sent? Nothing is answered automatically to those (RFC 3834), whatever the rules say.
pub fn is_automatic(original: &[u8], mail_from: &str) -> bool {
    if mail_from.is_empty() || mail_from.starts_with("<>") {
        return true;
    }
    let local = mail_from.split('@').next().unwrap_or_default().to_ascii_lowercase();
    if ["mailer-daemon", "postmaster", "no-reply", "noreply", "do-not-reply"].iter().any(|p| local == *p) || local.ends_with("-bounces") || local.contains("-request") {
        return true;
    }
    if header_values(original, "auto-submitted").iter().any(|v| !v.trim().eq_ignore_ascii_case("no")) {
        return true;
    }
    if header_values(original, "precedence").iter().any(|v| ["bulk", "junk", "list"].contains(&v.trim().to_ascii_lowercase().as_str())) {
        return true;
    }
    ["list-id", "list-unsubscribe", "x-autoreply", "x-autorespond", "x-auto-response-suppress"].iter().any(|h| header_count(original, h) > 0)
}

/// May a message be forwarded by an account of this server, and to whom: the hop count is within bounds and the addresses that already forwarded it are left out.
pub fn forward_targets(rules: &Forward, original: &[u8], own_address: &str) -> Vec<String> {
    if header_count(original, FORWARDED_HEADER) >= MAX_FORWARD_HOPS {
        return Vec::new();
    }
    let seen: Vec<String> = header_values(original, FORWARDED_HEADER).into_iter().map(|v| v.to_ascii_lowercase()).collect();
    rules.to.iter().filter(|t| !t.eq_ignore_ascii_case(own_address) && !seen.iter().any(|s| s == *t || s == &own_address.to_ascii_lowercase())).cloned().collect()
}

/// The message to forward: the original with a marker header in front (the loop guard).
pub fn forwarded_copy(original: &[u8], by: &str) -> Vec<u8> {
    let mut out = format!("{FORWARDED_HEADER}: {by}\r\n").into_bytes();
    out.extend_from_slice(original);
    out
}

/// The automatic reply from `from` (the address the mail was sent to) to `to`, in answer to a message with `original`'s Message-ID.
pub fn vacation_reply(v: &Vacation, from: &str, to: &str, original: &[u8], host: &str, now: i64, token: &str) -> Vec<u8> {
    let subject = if v.subject.trim().is_empty() {
        let theirs = header_values(original, "subject").into_iter().next().unwrap_or_default();
        format!("Automatic reply: {}", theirs.chars().take(100).filter(|c| !c.is_control()).collect::<String>())
    } else {
        v.subject.chars().filter(|c| !c.is_control()).collect()
    };
    let mut head = format!("From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: {}\r\nMessage-ID: <vacation-{token}@{host}>\r\n", rfc5322_date(now));
    if let Some(id) = header_values(original, "message-id").into_iter().next().filter(|i| i.starts_with('<') && i.ends_with('>') && !i.contains(['\r', '\n'])) {
        head.push_str(&format!("In-Reply-To: {id}\r\nReferences: {id}\r\n"));
    }
    head.push_str("Auto-Submitted: auto-replied\r\nPrecedence: bulk\r\nX-Auto-Response-Suppress: All\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(v.body.replace("\r\n", "\n").replace('\n', "\r\n").as_bytes());
    out.extend_from_slice(b"\r\n");
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn rules_are_read_cleaned_and_written_back_in_one_form() {
        let r = MailRules::validate(&json!({ "forward": { "to": [" Me@Other.example ", "me@other.example"], "keep": false }, "vacation": { "enabled": true, "subject": "Away", "body": "Back soon", "from": 10, "until": 20 } })).unwrap();
        assert_eq!(r.forward.as_ref().unwrap().to, vec!["me@other.example"]);
        assert!(!r.forward.as_ref().unwrap().keep);
        let back = MailRules::from_value(&r.to_value());
        assert_eq!(back, r);
    }

    #[test]
    fn what_is_wrong_is_named_and_nothing_odd_is_stored() {
        let bad = |v: Value| MailRules::validate(&v).unwrap_err();
        assert_eq!(bad(json!({ "x": 1 })), "unknown field");
        assert_eq!(bad(json!({ "forward": { "to": [] } })), "forward needs one to ten addresses");
        assert_eq!(bad(json!({ "forward": { "to": ["nope"] } })), "forward has an address that is not one");
        assert_eq!(bad(json!({ "forward": { "to": ["a<b>@x.example"] } })), "forward has an address that is not one");
        assert_eq!(bad(json!({ "vacation": { "enabled": true, "body": "  " } })), "an automatic reply needs a text");
        assert_eq!(bad(json!({ "vacation": { "enabled": false, "subject": "a\r\nBcc: x", "body": "b" } })), "the subject is too long or has a line break");
        assert_eq!(bad(json!({ "vacation": { "enabled": true, "body": "b", "from": 20, "until": 10 } })), "the end is before the start");
        // An odd stored object is read as far as it makes sense, never an error.
        assert_eq!(MailRules::from_value(&json!({ "forward": { "to": ["bad"] }, "vacation": 5 })), MailRules { forward: None, vacation: Some(Vacation::default()) });
    }

    #[test]
    fn the_reply_is_on_only_inside_its_window() {
        let v = Vacation { enabled: true, from: Some(100), until: Some(200), ..Vacation::default() };
        assert!(!v.active(99) && v.active(100) && v.active(199) && !v.active(200));
        assert!(!Vacation { enabled: false, ..v.clone() }.active(150));
        assert!(Vacation { enabled: true, ..Vacation::default() }.active(5), "no window: always");
    }

    #[test]
    fn machines_lists_and_bulk_mail_are_never_answered() {
        let msg = |h: &str| format!("From: a@far.example\r\n{h}\r\nSubject: x\r\n\r\nHi\r\n").into_bytes();
        assert!(!is_automatic(&msg("To: me@one.test"), "a@far.example"));
        assert!(is_automatic(&msg("To: me@one.test"), ""), "the null sender");
        assert!(is_automatic(&msg("To: me@one.test"), "MAILER-DAEMON@far.example"));
        assert!(is_automatic(&msg("Auto-Submitted: auto-generated"), "a@far.example"));
        assert!(!is_automatic(&msg("Auto-Submitted: no"), "a@far.example"));
        assert!(is_automatic(&msg("Precedence: bulk"), "a@far.example"));
        assert!(is_automatic(&msg("List-Id: <x.far.example>"), "a@far.example"));
        assert!(is_automatic(&msg("To: me@one.test"), "team-bounces@far.example"));
    }

    #[test]
    fn a_forward_stops_at_a_loop_and_skips_the_addresses_that_already_had_it() {
        let rules = Forward { to: vec!["me@other.example".into(), "x@one.test".into()], keep: true };
        let plain = b"Subject: x\r\n\r\nHi\r\n";
        assert_eq!(forward_targets(&rules, plain, "x@one.test"), vec!["me@other.example"], "never to itself");
        let once = forwarded_copy(plain, "me@other.example");
        assert_eq!(forward_targets(&rules, &once, "x@one.test"), Vec::<String>::new(), "it was at me@other.example, and itself is skipped");
        let mut deep = plain.to_vec();
        for i in 0..MAX_FORWARD_HOPS {
            deep = forwarded_copy(&deep, &format!("h{i}@z.example"));
        }
        assert!(forward_targets(&rules, &deep, "y@one.test").is_empty(), "too many hops");
    }

    #[test]
    fn the_reply_answers_the_message_and_says_what_it_is() {
        let v = Vacation { enabled: true, subject: String::new(), body: "I am away.\nBack Monday.".into(), from: None, until: None };
        let orig = b"From: a@far.example\r\nMessage-ID: <abc@far.example>\r\nSubject: Lunch?\r\n\r\nHi\r\n";
        let text = String::from_utf8(vacation_reply(&v, "me@one.test", "a@far.example", orig, "mx.one.test", 1_700_000_000, "t0k")).unwrap();
        assert!(text.contains("Subject: Automatic reply: Lunch?\r\n"), "{text}");
        assert!(text.contains("In-Reply-To: <abc@far.example>\r\n") && text.contains("Auto-Submitted: auto-replied\r\n"), "{text}");
        assert!(text.ends_with("I am away.\r\nBack Monday.\r\n"), "{text}");
    }
}
