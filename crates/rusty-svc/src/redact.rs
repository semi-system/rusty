//! Log hygiene: client lines and secrets reach the log only through these helpers. A password,
//! an AUTH payload or a message body is never logged (security floor, item 2).

use std::fmt;

/// Longest client line shown in a log (characters).
const MAX_LOGGED_CHARS: usize = 1000;

const REDACTED: &str = "<redacted>";

/// A value that prints as `<redacted>` in `Debug` and `Display`.
pub struct Redacted<T>(T);

impl<T> Redacted<T> {
    pub fn new(value: T) -> Redacted<T> {
        Redacted(value)
    }

    /// The wrapped value, for the one place that must use it.
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl<T> fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// A client protocol line made safe to log.
///
/// * `in_auth_exchange`: the line is part of an authentication exchange (the continuation after
///   a bare `AUTH LOGIN`, an SASL response); it is opaque and replaced entirely.
/// * `LOGIN` (IMAP, with or without a tag) keeps its verb and drops every argument; `AUTH` and
///   `AUTHENTICATE` keep the mechanism and drop the initial response; `PASS` drops its argument.
/// * Control characters become `?` (a client cannot forge log lines) and the line is cut at 1000
///   characters.
pub fn redact_command(line: &str, in_auth_exchange: bool) -> String {
    if in_auth_exchange {
        return REDACTED.to_owned();
    }
    let clean: String = line
        .chars()
        .take(MAX_LOGGED_CHARS)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    let toks: Vec<&str> = clean.split_ascii_whitespace().collect();
    let is_sensitive = |t: &str| {
        ["LOGIN", "AUTH", "AUTHENTICATE", "PASS"]
            .iter()
            .any(|v| t.eq_ignore_ascii_case(v))
    };
    // An IMAP tag may precede the verb.
    let at = match toks.as_slice() {
        [first, ..] if is_sensitive(first) => Some(0),
        [_, second, ..] if is_sensitive(second) => Some(1),
        _ => None,
    };
    let Some(at) = at else {
        return clean;
    };
    let verb = toks.get(at).copied().unwrap_or_default();
    let keep = if verb.eq_ignore_ascii_case("AUTH") || verb.eq_ignore_ascii_case("AUTHENTICATE") {
        2 // verb and mechanism
    } else {
        1
    };
    let head = at + keep;
    if toks.len() <= head {
        return clean; // nothing secret has been sent yet ("AUTH LOGIN", bare "LOGIN")
    }
    let mut out = toks.get(..head).unwrap_or_default().join(" ");
    out.push(' ');
    out.push_str(REDACTED);
    out
}
