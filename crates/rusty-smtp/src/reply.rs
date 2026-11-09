//! SMTP replies and the ONE table that maps store errors to statuses (plan 1b rule 4):
//!
//! | store error | recipient (RCPT)  | message (end of DATA) |
//! |-------------|-------------------|-----------------------|
//! | `Busy`      | 451 4.3.0         | 451 4.3.0             |
//! | `Storage`   | 451 4.3.0         | 451 4.3.0             |
//! | `Conflict`  | 451 4.3.0         | 451 4.3.0             |
//! | `NotFound`  | 550 5.1.1         | 550 5.1.1             |
//! | `Invalid`   | 501 5.1.3         | 554 5.6.0             |
//! | `Quota`     | 452 4.5.3         | 552 5.2.2             |
//!
//! Reply texts are fixed; storage text and caller input never reach the wire.

use rusty_core::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub code: u16,
    /// Enhanced status code ("2.0.0"), or empty for replies that carry none (greeting, EHLO).
    pub enhanced: &'static str,
    pub lines: Vec<String>,
}

/// One text line made safe for the wire: control characters (CR and LF included) become spaces.
fn clean(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

impl Reply {
    pub fn new(code: u16, enhanced: &'static str, text: impl AsRef<str>) -> Reply {
        Reply { code, enhanced, lines: vec![clean(text.as_ref())] }
    }

    pub fn multi(code: u16, enhanced: &'static str, lines: Vec<String>) -> Reply {
        let mut lines: Vec<String> = lines.iter().map(|l| clean(l)).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Reply { code, enhanced, lines }
    }

    /// Wire form: `code-text` for every line but the last, `code text` for the last.
    pub fn render(&self) -> Vec<u8> {
        let mut out = String::new();
        let last = self.lines.len().saturating_sub(1);
        for (i, line) in self.lines.iter().enumerate() {
            let sep = if i == last { ' ' } else { '-' };
            out.push_str(&self.code.to_string());
            out.push(sep);
            if !self.enhanced.is_empty() {
                out.push_str(self.enhanced);
                out.push(' ');
            }
            out.push_str(line);
            out.push_str("\r\n");
        }
        out.into_bytes()
    }

    pub fn is_positive(&self) -> bool {
        (200..300).contains(&self.code)
    }
}

/// Where the error happened (the table above has one column for each).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorContext {
    Rcpt,
    Data,
}

const TEMP: (u16, &str, &str) = (451, "4.3.0", "Temporary local problem, please try again later");

/// The shared mapping (see the module table).
pub fn map_error(e: &Error, ctx: ErrorContext) -> Reply {
    let (code, enhanced, text) = match (e, ctx) {
        (Error::Busy(_) | Error::Storage { .. } | Error::Conflict(_), _) => TEMP,
        (Error::NotFound(_), _) => (550, "5.1.1", "No such user here"),
        (Error::Invalid(_), ErrorContext::Rcpt) => (501, "5.1.3", "Bad recipient address syntax"),
        (Error::Invalid(_), ErrorContext::Data) => (554, "5.6.0", "Message content rejected"),
        (Error::Quota(_), ErrorContext::Rcpt) => (452, "4.5.3", "Too many recipients"),
        (Error::Quota(_), ErrorContext::Data) => (552, "5.2.2", "Mailbox full or message too large"),
    };
    Reply::new(code, enhanced, text)
}

fn is_temporary(e: &Error) -> bool {
    matches!(e, Error::Busy(_) | Error::Storage { .. } | Error::Conflict(_))
}

/// The reply to the end of DATA from the per-recipient delivery results (all-or-451):
///
/// * every delivery stored: 250;
/// * any failure next to a success, or any temporary failure: 451, because SMTP cannot answer per
///   recipient after DATA and the retry is idempotent (`DeliveryId`), so nothing is dropped and
///   nothing is stored twice;
/// * only permanent failures: the permanent status of the first.
pub fn delivery_reply(results: Vec<Result<(), Error>>) -> Reply {
    let mut errors = results.iter().filter_map(|r| r.as_ref().err()).peekable();
    let first = errors.peek().copied();
    let all_failed = !results.is_empty() && results.iter().all(Result::is_err);
    match first {
        None if results.is_empty() => Reply::new(TEMP.0, TEMP.1, TEMP.2),
        None => Reply::new(250, "2.0.0", "Ok: queued"),
        Some(e) => {
            let any_temporary = results.iter().filter_map(|r| r.as_ref().err()).any(is_temporary);
            if any_temporary || !all_failed {
                Reply::new(TEMP.0, TEMP.1, TEMP.2)
            } else {
                map_error(e, ErrorContext::Data)
            }
        }
    }
}
