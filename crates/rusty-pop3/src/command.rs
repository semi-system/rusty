//! The commands of POP3 (RFC 1939, 2449, 1957, 5034, 6856): one line in, one [`Command`] out. No I/O, no state, nothing about a line is echoed in an error.
//!
//! The rules are the strict ones: a line holds no NUL and no control character (bytes below 0x20 and 0x7f; the CR before the LF was taken off by the caller); the verb is 3 or 4 ASCII letters in
//! any case; arguments are separated by exactly one space; a message number is decimal digits with no sign and no leading zero, at least 1 and at most [`MAX_NUMBER`]; the arguments of `USER` and
//! `PASS` are the rest of the line (a password may hold spaces) and must be UTF-8 (RFC 6856).

/// The longest command line (without its CRLF): the RFC says 255 octets for most commands; the room above that is for `AUTH PLAIN` with its initial response, which gets
/// [`MAX_AUTH_LINE`] when the server asked for it.
pub const MAX_LINE: usize = 512;
/// The longest line of an `AUTH PLAIN` initial response or continuation (a 1 KiB password plus two names, base64).
pub const MAX_AUTH_LINE: usize = 4 * 1024;
/// The largest message number or line count a command may carry (a mailbox is far smaller; the bound keeps the digits from overflowing).
pub const MAX_NUMBER: u32 = 100_000_000;
/// The longest user name or password.
pub const MAX_CREDENTIAL: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Capa,
    Stls,
    User(String),
    Pass(String),
    /// `AUTH` alone lists the mechanisms; `AUTH mech [initial]` starts one. The mechanism is upper-cased.
    Auth { mechanism: Option<String>, initial: Option<String> },
    Stat,
    List(Option<u32>),
    Uidl(Option<u32>),
    Retr(u32),
    Top(u32, u32),
    Dele(u32),
    Noop,
    Rset,
    Quit,
    /// RFC 6856: switch the session to UTF-8 (this server always speaks it, so it only says yes).
    Utf8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Not a command this server knows.
    Unknown,
    /// A known command with arguments that do not fit.
    Syntax,
}

fn is_clean(line: &[u8]) -> bool {
    line.iter().all(|b| *b >= 0x20 && *b != 0x7f)
}

/// A message number or a count: decimal digits, no sign, no leading zero, `1..=MAX_NUMBER` (`min` 0 for a line count of `TOP`).
fn number(arg: &str, min: u32) -> Option<u32> {
    if arg.is_empty() || arg.len() > 9 || !arg.bytes().all(|b| b.is_ascii_digit()) || (arg.len() > 1 && arg.starts_with('0')) {
        return None;
    }
    let n: u32 = arg.parse().ok()?;
    (n >= min && n <= MAX_NUMBER).then_some(n)
}

fn credential(rest: &str) -> Option<String> {
    (!rest.is_empty() && rest.len() <= MAX_CREDENTIAL).then(|| rest.to_owned())
}

/// Parse one line (no CRLF). Nothing about `line` is kept in the error.
pub fn parse(line: &[u8]) -> Result<Command, ParseError> {
    if !is_clean(line) {
        return Err(ParseError::Syntax);
    }
    let text = std::str::from_utf8(line).map_err(|_| ParseError::Syntax)?;
    let (verb, rest) = match text.split_once(' ') {
        Some((v, r)) => (v, Some(r)),
        None => (text, None),
    };
    if !(3..=4).contains(&verb.len()) || !verb.bytes().all(|b| b.is_ascii_alphanumeric()) || !verb.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) {
        return Err(ParseError::Unknown);
    }
    let verb = verb.to_ascii_uppercase();
    let args: Vec<&str> = rest.map_or_else(Vec::new, |r| r.split(' ').collect());
    let none = |c: Command| if rest.is_none() { Ok(c) } else { Err(ParseError::Syntax) };
    match verb.as_str() {
        "CAPA" => none(Command::Capa),
        "STLS" => none(Command::Stls),
        "STAT" => none(Command::Stat),
        "NOOP" => none(Command::Noop),
        "RSET" => none(Command::Rset),
        "QUIT" => none(Command::Quit),
        "UTF8" => none(Command::Utf8),
        "USER" => rest.and_then(credential).map(Command::User).ok_or(ParseError::Syntax),
        "PASS" => rest.and_then(credential).map(Command::Pass).ok_or(ParseError::Syntax),
        "AUTH" => match args.as_slice() {
            [] => Ok(Command::Auth { mechanism: None, initial: None }),
            [m] if !m.is_empty() => Ok(Command::Auth { mechanism: Some(m.to_ascii_uppercase()), initial: None }),
            [m, i] if !m.is_empty() && !i.is_empty() => Ok(Command::Auth { mechanism: Some(m.to_ascii_uppercase()), initial: Some((*i).to_owned()) }),
            _ => Err(ParseError::Syntax),
        },
        "LIST" | "UIDL" => {
            let n = match args.as_slice() {
                [] if rest.is_none() => None,
                [a] => Some(number(a, 1).ok_or(ParseError::Syntax)?),
                _ => return Err(ParseError::Syntax),
            };
            Ok(if verb == "LIST" { Command::List(n) } else { Command::Uidl(n) })
        }
        "RETR" | "DELE" => match args.as_slice() {
            [a] => {
                let n = number(a, 1).ok_or(ParseError::Syntax)?;
                Ok(if verb == "RETR" { Command::Retr(n) } else { Command::Dele(n) })
            }
            _ => Err(ParseError::Syntax),
        },
        "TOP" => match args.as_slice() {
            [a, b] => Ok(Command::Top(number(a, 1).ok_or(ParseError::Syntax)?, number(b, 0).ok_or(ParseError::Syntax)?)),
            _ => Err(ParseError::Syntax),
        },
        _ => Err(ParseError::Unknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Command, ParseError> {
        parse(s.as_bytes())
    }

    #[test]
    fn every_command_parses_in_any_case() {
        assert_eq!(p("capa"), Ok(Command::Capa));
        assert_eq!(p("StLs"), Ok(Command::Stls));
        assert_eq!(p("STAT"), Ok(Command::Stat));
        assert_eq!(p("list"), Ok(Command::List(None)));
        assert_eq!(p("LIST 12"), Ok(Command::List(Some(12))));
        assert_eq!(p("uidl"), Ok(Command::Uidl(None)));
        assert_eq!(p("UIDL 3"), Ok(Command::Uidl(Some(3))));
        assert_eq!(p("RETR 1"), Ok(Command::Retr(1)));
        assert_eq!(p("top 2 0"), Ok(Command::Top(2, 0)));
        assert_eq!(p("TOP 2 10"), Ok(Command::Top(2, 10)));
        assert_eq!(p("dele 4"), Ok(Command::Dele(4)));
        assert_eq!(p("NOOP"), Ok(Command::Noop));
        assert_eq!(p("RSET"), Ok(Command::Rset));
        assert_eq!(p("quit"), Ok(Command::Quit));
        assert_eq!(p("UTF8"), Ok(Command::Utf8));
    }

    #[test]
    fn user_and_pass_take_the_rest_of_the_line_and_auth_takes_a_mechanism() {
        assert_eq!(p("USER alice@example.org"), Ok(Command::User("alice@example.org".into())));
        assert_eq!(p("PASS my secret  with spaces"), Ok(Command::Pass("my secret  with spaces".into())));
        assert_eq!(p("PASS pässwörd"), Ok(Command::Pass("pässwörd".into())));
        assert_eq!(p("AUTH"), Ok(Command::Auth { mechanism: None, initial: None }));
        assert_eq!(p("auth plain"), Ok(Command::Auth { mechanism: Some("PLAIN".into()), initial: None }));
        assert_eq!(p("AUTH PLAIN AGFsaWNlAHB3"), Ok(Command::Auth { mechanism: Some("PLAIN".into()), initial: Some("AGFsaWNlAHB3".into()) }));
        for bad in ["USER", "USER ", "PASS", "AUTH PLAIN a b", "AUTH  PLAIN"] {
            assert_eq!(p(bad), Err(ParseError::Syntax), "{bad:?}");
        }
        assert_eq!(p(&format!("PASS {}", "x".repeat(MAX_CREDENTIAL + 1))), Err(ParseError::Syntax));
    }

    #[test]
    fn numbers_are_plain_decimals_in_range() {
        for bad in ["RETR", "RETR 0", "RETR 01", "RETR +1", "RETR -1", "RETR 1 2", "RETR x", "RETR 1x", "RETR 100000001", "RETR 9999999999", "DELE 0", "LIST 0", "UIDL 00", "TOP 1", "TOP 1 -1", "TOP 0 1", "TOP 1 2 3", "RETR \u{661}"] {
            assert_eq!(p(bad), Err(ParseError::Syntax), "{bad:?}");
        }
        assert_eq!(p("RETR 100000000"), Ok(Command::Retr(100_000_000)));
    }

    #[test]
    fn commands_that_take_nothing_refuse_an_argument() {
        for bad in ["CAPA x", "STAT 1", "NOOP ", "QUIT now", "RSET 1", "STLS now", "UTF8 on"] {
            assert_eq!(p(bad), Err(ParseError::Syntax), "{bad:?}");
        }
    }

    #[test]
    fn what_is_not_a_command_or_not_clean_is_refused_without_echo() {
        assert_eq!(p(""), Err(ParseError::Unknown));
        assert_eq!(p("HELO example.org"), Err(ParseError::Unknown));
        assert_eq!(p("RETRIEVE 1"), Err(ParseError::Unknown));
        assert_eq!(p("RE 1"), Err(ParseError::Unknown));
        assert_eq!(p("1234 x"), Err(ParseError::Unknown));
        for bad in ["USER a\0b", "STAT\r", "USER a\tb", "PASS a\x7fb", "RETR 1\n"] {
            assert_eq!(p(bad), Err(ParseError::Syntax), "{bad:?}");
        }
        assert_eq!(parse(b"PASS \xff\xfe"), Err(ParseError::Syntax), "a password is UTF-8");
    }
}
