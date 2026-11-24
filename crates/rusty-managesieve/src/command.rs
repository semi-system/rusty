//! One ManageSieve command (RFC 5804 section 4) out of a buffer of bytes, strictly, with no state and no I/O. A command is an atom and arguments: a quoted string, a literal `{n+}` CRLF and `n` bytes
//! (the only literal a client may send), or a bare number or atom. The parser never reads past the first command and never allocates more than the limits it is given.

/// One argument of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    /// A quoted string or a literal: bytes (a name or a script is checked as text by the caller).
    Str(Vec<u8>),
    /// A number or an atom, as written.
    Atom(String),
}

impl Arg {
    pub fn bytes(&self) -> &[u8] {
        match self {
            Arg::Str(b) => b,
            Arg::Atom(a) => a.as_bytes(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Upper case.
    pub name: String,
    pub args: Vec<Arg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The longest literal (a script).
    pub max_literal: usize,
    /// The longest command apart from its literals (a line of atoms and quoted strings).
    pub max_line: usize,
    /// Arguments of one command.
    pub max_args: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { max_literal: 64 * 1024, max_line: 4096, max_args: 8 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// More bytes are needed.
    Need,
    /// A command and how many bytes it took.
    Done(Command, usize),
    /// Not a command, or over a limit: the connection cannot be resynchronised, so the caller answers and closes.
    Bad(&'static str),
}

/// Parse one command from the start of `buf`. With `bare_string` the buffer holds one string only (the response of the client in a SASL exchange) and the command is named `""`.
pub fn parse(buf: &[u8], limits: &Limits, bare_string: bool) -> Parsed {
    let mut p = Cursor { buf, at: 0, text: 0, limits };
    let name = if bare_string {
        String::new()
    } else {
        match p.atom() {
            Ok(Some(a)) if !a.is_empty() => a.to_ascii_uppercase(),
            Ok(Some(_)) => return Parsed::Bad("a command is expected"),
            Ok(None) => return need_or_bad(&p),
            Err(e) => return Parsed::Bad(e),
        }
    };
    let mut args: Vec<Arg> = Vec::new();
    loop {
        // After the name or an argument: the end of the line, or a space and the next argument.
        let first = bare_string && args.is_empty();
        if !first {
            match p.peek() {
                None => return need_or_bad(&p),
                Some(b'\r') => {
                    return match p.buf.get(p.at + 1) {
                        None => need_or_bad(&p),
                        Some(b'\n') => Parsed::Done(Command { name, args }, p.at + 2),
                        Some(_) => Parsed::Bad("a bare CR"),
                    };
                }
                Some(b'\n') => return Parsed::Bad("a line ends with CRLF"),
                Some(b' ') => {
                    p.at += 1;
                    p.text += 1;
                }
                Some(_) => return Parsed::Bad("a space is expected"),
            }
        }
        if args.len() >= limits.max_args {
            return Parsed::Bad("too many arguments");
        }
        match p.argument() {
            Ok(Some(a)) => args.push(a),
            Ok(None) => return need_or_bad(&p),
            Err(e) => return Parsed::Bad(e),
        }
    }
}

fn need_or_bad(p: &Cursor<'_>) -> Parsed {
    if p.text > p.limits.max_line {
        Parsed::Bad("the line is too long")
    } else {
        Parsed::Need
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    at: usize,
    /// Bytes read that are not literal data: bounded by `max_line`.
    text: usize,
    limits: &'a Limits,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.buf.get(self.at).copied()
    }

    /// `Ok(None)`: the buffer ends inside it (an atom is complete only when a byte after it has been seen).
    fn atom(&mut self) -> Result<Option<String>, &'static str> {
        let start = self.at;
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' {
                self.at += 1;
                self.text += 1;
                if self.text > self.limits.max_line {
                    return Err("the line is too long");
                }
            } else {
                break;
            }
        }
        if self.at == self.buf.len() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(self.buf.get(start..self.at).unwrap_or(&[])).into_owned()))
    }

    fn argument(&mut self) -> Result<Option<Arg>, &'static str> {
        match self.peek() {
            None => Ok(None),
            Some(b'"') => self.quoted(),
            Some(b'{') => self.literal(),
            Some(_) => match self.atom()? {
                Some(a) if a.is_empty() => Err("an argument is expected"),
                other => Ok(other.map(Arg::Atom)),
            },
        }
    }

    fn quoted(&mut self) -> Result<Option<Arg>, &'static str> {
        self.at += 1;
        self.text += 1;
        let mut out = Vec::new();
        loop {
            let Some(b) = self.peek() else { return Ok(None) };
            self.at += 1;
            self.text += 1;
            if self.text > self.limits.max_line {
                return Err("the line is too long");
            }
            match b {
                b'"' => return Ok(Some(Arg::Str(out))),
                b'\r' | b'\n' | 0 => return Err("a quoted string holds no line break"),
                b'\\' => match self.peek() {
                    None => return Ok(None),
                    Some(c) if c == b'"' || c == b'\\' => {
                        self.at += 1;
                        self.text += 1;
                        out.push(c);
                    }
                    Some(_) => return Err("a backslash escapes a quote or a backslash only"),
                },
                other => out.push(other),
            }
        }
    }

    fn literal(&mut self) -> Result<Option<Arg>, &'static str> {
        self.at += 1;
        self.text += 1;
        let mut n: usize = 0;
        let mut digits = 0;
        loop {
            let Some(b) = self.peek() else { return Ok(None) };
            self.at += 1;
            self.text += 1;
            match b {
                b'0'..=b'9' => {
                    digits += 1;
                    n = n.saturating_mul(10).saturating_add(usize::from(b - b'0'));
                    if digits > 9 || n > self.limits.max_literal {
                        return Err("the literal is too large");
                    }
                }
                b'+' if digits > 0 => break,
                _ => return Err("a literal is written {n+}"),
            }
        }
        // "}" CR LF, then the data.
        for want in *b"}\r\n" {
            match self.peek() {
                None => return Ok(None),
                Some(b) if b == want => {
                    self.at += 1;
                    self.text += 1;
                }
                Some(_) => return Err("a literal is written {n+}"),
            }
        }
        let Some(end) = self.at.checked_add(n) else { return Err("the literal is too large") };
        match self.buf.get(self.at..end) {
            Some(data) => {
                self.at = end;
                Ok(Some(Arg::Str(data.to_vec())))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(s: &[u8]) -> (Command, usize) {
        match parse(s, &Limits::default(), false) {
            Parsed::Done(c, n) => (c, n),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_command_with_quoted_strings_literals_and_numbers() {
        let (c, n) = done(b"putscript \"a b\" {9+}\r\nkeep;\r\n;;\r\n");
        assert_eq!(c.name, "PUTSCRIPT");
        assert_eq!(c.args, vec![Arg::Str(b"a b".to_vec()), Arg::Str(b"keep;\r\n;;".to_vec())]);
        assert_eq!(n, 33);
        let (c, _) = done(b"HAVESPACE \"x\" 120\r\nNOOP\r\n");
        assert_eq!(c.args, vec![Arg::Str(b"x".to_vec()), Arg::Atom("120".into())]);
        let (c, n) = done(b"LISTSCRIPTS\r\nrest");
        assert_eq!((c.name.as_str(), c.args.len(), n), ("LISTSCRIPTS", 0, 13));
        let (c, _) = done(b"NOOP \"a\\\"b\\\\\"\r\n");
        assert_eq!(c.args, vec![Arg::Str(b"a\"b\\".to_vec())]);
    }

    #[test]
    fn a_partial_command_waits_and_a_malformed_one_is_refused() {
        let l = Limits::default();
        for part in [&b""[..], b"LISTSCR", b"LISTSCRIPTS", b"LISTSCRIPTS\r", b"GETSCRIPT \"ab", b"GETSCRIPT {5+}\r\nab", b"GETSCRIPT {5", b"GETSCRIPT {5+}"] {
            assert_eq!(parse(part, &l, false), Parsed::Need, "{:?}", String::from_utf8_lossy(part));
        }
        for bad in [&b"LISTSCRIPTS\n"[..], b"A\rB", b"GETSCRIPT {5}\r\nabcde\r\n", b"GETSCRIPT {+}\r\n", b"GETSCRIPT \"a\nb\"\r\n", b"\r\n", b"X \"a\\n\"\r\n", b"X\"a\"\r\n"] {
            assert!(matches!(parse(bad, &l, false), Parsed::Bad(_)), "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn the_limits_hold_before_the_data_is_read() {
        let l = Limits { max_literal: 10, max_line: 32, max_args: 2 };
        assert!(matches!(parse(b"PUTSCRIPT \"a\" {11+}\r\n", &l, false), Parsed::Bad(_)), "a literal is refused by its size, before its bytes arrive");
        assert!(matches!(parse(b"PUTSCRIPT \"a\" {99999999999+}\r\n", &l, false), Parsed::Bad(_)));
        assert!(matches!(parse(&[b'A'; 40], &l, false), Parsed::Bad(_)), "an endless atom is cut");
        assert!(matches!(parse(b"X \"1\" \"2\" \"3\"\r\n", &l, false), Parsed::Bad(_)));
        let long = format!("X \"{}", "a".repeat(40));
        assert!(matches!(parse(long.as_bytes(), &l, false), Parsed::Bad(_)));
    }

    #[test]
    fn the_response_of_a_sasl_exchange_is_one_string() {
        let l = Limits::default();
        let Parsed::Done(c, n) = parse(b"\"AGEAYg==\"\r\n", &l, true) else { panic!() };
        assert_eq!((c.name.as_str(), c.args, n), ("", vec![Arg::Str(b"AGEAYg==".to_vec())], 12));
        assert!(matches!(parse(b"\r\n", &l, true), Parsed::Bad(_)));
    }
}
