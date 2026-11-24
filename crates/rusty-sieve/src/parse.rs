//! The Sieve parser (RFC 5228 section 8): text to a tree of commands and tests. Every limit is checked here, so what the interpreter sees is small: a script of at most 64 KiB, 1000 commands, blocks nested 16 deep,
//! lists of at most 100 strings. The extensions understood are `fileinto`, `reject`, `envelope` and `vacation`; a script that requires anything else is refused with the name of what it asked for.

use std::fmt;

pub const MAX_SCRIPT_BYTES: usize = 64 * 1024;
pub const MAX_COMMANDS: usize = 1000;
pub const MAX_DEPTH: usize = 16;
pub const MAX_LIST: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

type R<T> = Result<T, ParseError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchType {
    Is,
    Contains,
    Matches,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressPart {
    All,
    LocalPart,
    Domain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Comparator {
    /// `i;ascii-casemap`, the default: ASCII letters compare without regard to case.
    AsciiCasemap,
    /// `i;octet`: byte for byte.
    Octet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub kind: MatchType,
    pub comparator: Comparator,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Test {
    True,
    False,
    Not(Box<Test>),
    AllOf(Vec<Test>),
    AnyOf(Vec<Test>),
    Exists(Vec<String>),
    Size { over: bool, limit: u64 },
    Header { m: Match, names: Vec<String>, keys: Vec<String> },
    Address { m: Match, part: AddressPart, names: Vec<String>, keys: Vec<String> },
    Envelope { m: Match, part: AddressPart, names: Vec<String>, keys: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vacation {
    pub days: u32,
    pub subject: Option<String>,
    pub from: Option<String>,
    pub addresses: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    If { branches: Vec<(Test, Vec<Command>)>, otherwise: Option<Vec<Command>> },
    Stop,
    Keep,
    Discard,
    Redirect(String),
    FileInto(String),
    Reject(String),
    Vacation(Vacation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    pub commands: Vec<Command>,
    pub requires: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Tag(String),
    Str(String),
    Num(u64),
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Semi,
}

struct Lexer<'a> {
    s: &'a [u8],
    i: usize,
    line: usize,
}

impl<'a> Lexer<'a> {
    fn err<T>(&self, m: &str) -> R<T> {
        Err(ParseError { line: self.line, message: m.to_owned() })
    }

    fn skip(&mut self) -> R<()> {
        while let Some(&c) = self.s.get(self.i) {
            match c {
                b'\n' => {
                    self.line += 1;
                    self.i += 1;
                }
                b' ' | b'\t' | b'\r' => self.i += 1,
                b'#' => {
                    while self.s.get(self.i).is_some_and(|c| *c != b'\n') {
                        self.i += 1;
                    }
                }
                b'/' if self.s.get(self.i + 1) == Some(&b'*') => {
                    self.i += 2;
                    loop {
                        match self.s.get(self.i) {
                            None => return self.err("a comment is not closed"),
                            Some(b'*') if self.s.get(self.i + 1) == Some(&b'/') => {
                                self.i += 2;
                                break;
                            }
                            Some(b'\n') => {
                                self.line += 1;
                                self.i += 1;
                            }
                            Some(_) => self.i += 1,
                        }
                    }
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn word(&mut self) -> String {
        let start = self.i;
        while self.s.get(self.i).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.i]).to_ascii_lowercase()
    }

    fn quoted(&mut self) -> R<String> {
        self.i += 1;
        let mut out = Vec::new();
        loop {
            match self.s.get(self.i) {
                None => return self.err("a string is not closed"),
                Some(b'"') => {
                    self.i += 1;
                    break;
                }
                Some(b'\\') => match self.s.get(self.i + 1) {
                    Some(&c) => {
                        out.push(c);
                        self.i += 2;
                    }
                    None => return self.err("a string is not closed"),
                },
                Some(&c) => {
                    if c == b'\n' {
                        self.line += 1;
                    }
                    out.push(c);
                    self.i += 1;
                }
            }
        }
        String::from_utf8(out).or_else(|_| self.err("a string is not UTF-8"))
    }

    /// `text:` followed by lines up to a line of one dot (RFC 5228 section 2.4.2).
    fn multiline(&mut self) -> R<String> {
        // The rest of the `text:` line (white space and a comment) must be empty.
        while self.s.get(self.i).is_some_and(|c| *c == b' ' || *c == b'\t' || *c == b'\r') {
            self.i += 1;
        }
        if self.s.get(self.i) == Some(&b'#') {
            while self.s.get(self.i).is_some_and(|c| *c != b'\n') {
                self.i += 1;
            }
        }
        if self.s.get(self.i) != Some(&b'\n') {
            return self.err("text: must be followed by a line end");
        }
        self.i += 1;
        self.line += 1;
        let mut out = String::new();
        loop {
            let start = self.i;
            while self.s.get(self.i).is_some_and(|c| *c != b'\n') {
                self.i += 1;
            }
            let raw = String::from_utf8_lossy(&self.s[start..self.i]).into_owned();
            let line = raw.strip_suffix('\r').unwrap_or(&raw).to_owned();
            if self.i >= self.s.len() && line != "." {
                return self.err("a text: string is not closed by a line with one dot");
            }
            self.i += 1;
            self.line += 1;
            if line == "." {
                return Ok(out);
            }
            // A dot at the start of a line is doubled (RFC 5228 section 2.4.2).
            out.push_str(if line.starts_with("..") { &line[1..] } else { &line });
            out.push('\n');
        }
    }

    fn next(&mut self) -> R<Option<Tok>> {
        self.skip()?;
        let Some(&c) = self.s.get(self.i) else { return Ok(None) };
        Ok(Some(match c {
            b'{' => {
                self.i += 1;
                Tok::LBrace
            }
            b'}' => {
                self.i += 1;
                Tok::RBrace
            }
            b'(' => {
                self.i += 1;
                Tok::LParen
            }
            b')' => {
                self.i += 1;
                Tok::RParen
            }
            b'[' => {
                self.i += 1;
                Tok::LBracket
            }
            b']' => {
                self.i += 1;
                Tok::RBracket
            }
            b',' => {
                self.i += 1;
                Tok::Comma
            }
            b';' => {
                self.i += 1;
                Tok::Semi
            }
            b'"' => Tok::Str(self.quoted()?),
            b':' => {
                self.i += 1;
                let w = self.word();
                if w.is_empty() {
                    return self.err("a tag needs a name");
                }
                Tok::Tag(w)
            }
            b'0'..=b'9' => {
                let start = self.i;
                while self.s.get(self.i).is_some_and(u8::is_ascii_digit) {
                    self.i += 1;
                }
                let mut n: u64 = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("0").parse().or_else(|_| self.err("a number is too large"))?;
                let mult = match self.s.get(self.i).map(u8::to_ascii_uppercase) {
                    Some(b'K') => 1u64 << 10,
                    Some(b'M') => 1 << 20,
                    Some(b'G') => 1 << 30,
                    _ => 1,
                };
                if mult > 1 {
                    self.i += 1;
                    n = n.checked_mul(mult).map_or_else(|| self.err("a number is too large"), Ok)?;
                }
                Tok::Num(n)
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let w = self.word();
                if w == "text" && self.s.get(self.i) == Some(&b':') {
                    self.i += 1;
                    Tok::Str(self.multiline()?)
                } else {
                    Tok::Ident(w)
                }
            }
            _ => return self.err("a character that does not belong in a script"),
        }))
    }
}

struct Parser {
    toks: Vec<(Tok, usize)>,
    pos: usize,
    commands: usize,
    requires: Vec<String>,
}

fn tag_known(t: &str) -> bool {
    matches!(t, "is" | "contains" | "matches" | "comparator" | "all" | "localpart" | "domain" | "over" | "under" | "days" | "subject" | "from" | "addresses")
}

impl Parser {
    fn line(&self) -> usize {
        self.toks.get(self.pos).or_else(|| self.toks.last()).map_or(1, |t| t.1)
    }

    fn err<T>(&self, m: &str) -> R<T> {
        Err(ParseError { line: self.line(), message: m.to_owned() })
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.0)
    }

    fn take(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).map(|t| t.0.clone());
        self.pos += 1;
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn string(&mut self) -> R<String> {
        match self.take() {
            Some(Tok::Str(s)) => Ok(s),
            _ => {
                self.pos = self.pos.saturating_sub(1);
                self.err("a string is expected")
            }
        }
    }

    fn string_list(&mut self) -> R<Vec<String>> {
        if self.eat(&Tok::LBracket) {
            let mut v = Vec::new();
            loop {
                v.push(self.string()?);
                if v.len() > MAX_LIST {
                    return self.err("a list holds at most 100 strings");
                }
                if self.eat(&Tok::Comma) {
                    continue;
                }
                if self.eat(&Tok::RBracket) {
                    return Ok(v);
                }
                return self.err("a list continues with , or ends with ]");
            }
        }
        Ok(vec![self.string()?])
    }

    fn number(&mut self) -> R<u64> {
        match self.take() {
            Some(Tok::Num(n)) => Ok(n),
            _ => {
                self.pos = self.pos.saturating_sub(1);
                self.err("a number is expected")
            }
        }
    }

    /// The tags of `address`, `envelope` and `header` tests, then their positional arguments.
    fn match_tags(&mut self, addressy: bool) -> R<(Match, AddressPart)> {
        let mut kind = MatchType::Is;
        let mut comparator = Comparator::AsciiCasemap;
        let mut part = AddressPart::All;
        while let Some(Tok::Tag(t)) = self.peek().cloned() {
            if !tag_known(&t) {
                return self.err(&format!("the tag :{t} is not known"));
            }
            self.pos += 1;
            match t.as_str() {
                "is" => kind = MatchType::Is,
                "contains" => kind = MatchType::Contains,
                "matches" => kind = MatchType::Matches,
                "comparator" => {
                    comparator = match self.string()?.to_ascii_lowercase().as_str() {
                        "i;ascii-casemap" => Comparator::AsciiCasemap,
                        "i;octet" => Comparator::Octet,
                        _ => return self.err("the comparator is not known (i;ascii-casemap or i;octet)"),
                    }
                }
                "all" | "localpart" | "domain" if addressy => {
                    part = match t.as_str() {
                        "all" => AddressPart::All,
                        "localpart" => AddressPart::LocalPart,
                        _ => AddressPart::Domain,
                    }
                }
                _ => return self.err(&format!("the tag :{t} does not belong to this test")),
            }
        }
        Ok((Match { kind, comparator }, part))
    }

    fn test(&mut self, depth: usize) -> R<Test> {
        if depth > MAX_DEPTH {
            return self.err("tests are nested too deep");
        }
        let Some(Tok::Ident(name)) = self.take() else {
            self.pos = self.pos.saturating_sub(1);
            return self.err("a test is expected");
        };
        match name.as_str() {
            "true" => Ok(Test::True),
            "false" => Ok(Test::False),
            "not" => Ok(Test::Not(Box::new(self.test(depth + 1)?))),
            "allof" | "anyof" => {
                if !self.eat(&Tok::LParen) {
                    return self.err("( is expected");
                }
                let mut v = Vec::new();
                loop {
                    v.push(self.test(depth + 1)?);
                    if v.len() > MAX_LIST {
                        return self.err("allof and anyof hold at most 100 tests");
                    }
                    if self.eat(&Tok::Comma) {
                        continue;
                    }
                    if self.eat(&Tok::RParen) {
                        break;
                    }
                    return self.err(", or ) is expected");
                }
                Ok(if name == "allof" { Test::AllOf(v) } else { Test::AnyOf(v) })
            }
            "exists" => Ok(Test::Exists(self.string_list()?)),
            "size" => {
                let over = match self.take() {
                    Some(Tok::Tag(t)) if t == "over" => true,
                    Some(Tok::Tag(t)) if t == "under" => false,
                    _ => {
                        self.pos = self.pos.saturating_sub(1);
                        return self.err(":over or :under is expected");
                    }
                };
                Ok(Test::Size { over, limit: self.number()? })
            }
            "header" => {
                let (m, _) = self.match_tags(false)?;
                let (names, keys) = (self.string_list()?, self.string_list()?);
                Ok(Test::Header { m, names, keys })
            }
            "address" | "envelope" => {
                if name == "envelope" && !self.requires.iter().any(|r| r == "envelope") {
                    return self.err("envelope needs: require \"envelope\";");
                }
                let (m, part) = self.match_tags(true)?;
                let (names, keys) = (self.string_list()?, self.string_list()?);
                Ok(if name == "address" { Test::Address { m, part, names, keys } } else { Test::Envelope { m, part, names, keys } })
            }
            _ => self.err(&format!("the test {name} is not known")),
        }
    }

    fn block(&mut self, depth: usize) -> R<Vec<Command>> {
        if depth > MAX_DEPTH {
            return self.err("blocks are nested too deep");
        }
        if !self.eat(&Tok::LBrace) {
            return self.err("{ is expected");
        }
        let mut v = Vec::new();
        while !self.eat(&Tok::RBrace) {
            if self.peek().is_none() {
                return self.err("a block is not closed");
            }
            v.push(self.command(depth + 1, false)?);
        }
        Ok(v)
    }

    fn need(&self, ext: &str) -> R<()> {
        if self.requires.iter().any(|r| r == ext) {
            Ok(())
        } else {
            self.err(&format!("this needs: require \"{ext}\";"))
        }
    }

    fn command(&mut self, depth: usize, _top: bool) -> R<Command> {
        self.commands += 1;
        if self.commands > MAX_COMMANDS {
            return self.err("a script holds at most 1000 commands");
        }
        let Some(Tok::Ident(name)) = self.take() else {
            self.pos = self.pos.saturating_sub(1);
            return self.err("a command is expected");
        };
        let end = |p: &mut Parser| -> R<()> {
            if p.eat(&Tok::Semi) {
                Ok(())
            } else {
                p.err("; is expected")
            }
        };
        match name.as_str() {
            "if" => {
                let mut branches = vec![];
                let t = self.test(depth)?;
                branches.push((t, self.block(depth)?));
                let mut otherwise = None;
                loop {
                    match self.peek() {
                        Some(Tok::Ident(w)) if w == "elsif" => {
                            self.pos += 1;
                            let t = self.test(depth)?;
                            branches.push((t, self.block(depth)?));
                        }
                        Some(Tok::Ident(w)) if w == "else" => {
                            self.pos += 1;
                            otherwise = Some(self.block(depth)?);
                            break;
                        }
                        _ => break,
                    }
                }
                Ok(Command::If { branches, otherwise })
            }
            "require" => self.err("require comes first in the script"),
            "stop" => {
                end(self)?;
                Ok(Command::Stop)
            }
            "keep" => {
                end(self)?;
                Ok(Command::Keep)
            }
            "discard" => {
                end(self)?;
                Ok(Command::Discard)
            }
            "redirect" => {
                let a = self.string()?;
                end(self)?;
                Ok(Command::Redirect(a))
            }
            "fileinto" => {
                self.need("fileinto")?;
                let f = self.string()?;
                end(self)?;
                Ok(Command::FileInto(f))
            }
            "reject" => {
                self.need("reject")?;
                let r = self.string()?;
                end(self)?;
                Ok(Command::Reject(r))
            }
            "vacation" => {
                self.need("vacation")?;
                let (mut days, mut subject, mut from, mut addresses) = (7u32, None, None, Vec::new());
                while let Some(Tok::Tag(t)) = self.peek().cloned() {
                    self.pos += 1;
                    match t.as_str() {
                        "days" => days = u32::try_from(self.number()?).unwrap_or(u32::MAX).clamp(1, 365),
                        "subject" => subject = Some(self.string()?),
                        "from" => from = Some(self.string()?),
                        "addresses" => addresses = self.string_list()?,
                        _ => return self.err(&format!("the tag :{t} is not known for vacation")),
                    }
                }
                let reason = self.string()?;
                end(self)?;
                Ok(Command::Vacation(Vacation { days, subject, from, addresses, reason }))
            }
            _ => self.err(&format!("the command {name} is not known")),
        }
    }

}

/// Parse a script.
pub fn parse(src: &str) -> R<Script> {
    if src.len() > MAX_SCRIPT_BYTES {
        return Err(ParseError { line: 1, message: "a script is at most 64 KiB".into() });
    }
    let mut lx = Lexer { s: src.as_bytes(), i: 0, line: 1 };
    let mut toks = Vec::new();
    while let Some(t) = lx.next()? {
        toks.push((t, lx.line));
        if toks.len() > 200_000 {
            return lx.err("the script is too long");
        }
    }
    let mut p = Parser { toks, pos: 0, commands: 0, requires: Vec::new() };
    // The requires come first, and only there.
    while matches!(p.peek(), Some(Tok::Ident(w)) if w == "require") {
        p.pos += 1;
        for r in p.string_list()? {
            if !matches!(r.as_str(), "fileinto" | "reject" | "envelope" | "vacation") {
                return p.err(&format!("the extension {r} is not supported"));
            }
            p.requires.push(r);
        }
        if !p.eat(&Tok::Semi) {
            return p.err("; is expected");
        }
    }
    let mut commands = Vec::new();
    while p.peek().is_some() {
        commands.push(p.command(0, true)?);
    }
    Ok(Script { commands, requires: p.requires })
}
