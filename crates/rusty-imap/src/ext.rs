//! Our own parser for the few commands the grammar library (1.0.0) does not know: `ID`,
//! `NAMESPACE`, `UID EXPUNGE` (UIDPLUS) and the extended `LIST` (selection and return options,
//! RFC 5258 with RFC 6154 SPECIAL-USE). Library-independent (it produces our own [`Command`]s) and
//! tried by the adapter before the library. Bounded: it reads only the frame it is given.

use crate::command::{Body, Command, ParseError, SeqNum, SeqRange, SeqSet};
use crate::utf7;

/// Most client ID parameters kept.
const MAX_ID_PAIRS: usize = 30;
/// Most patterns in an extended LIST.
const MAX_PATTERNS: usize = 32;

struct Lex<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Lex<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn sp(&mut self) -> bool {
        if self.peek() == Some(b' ') {
            while self.peek() == Some(b' ') {
                self.pos += 1;
            }
            true
        } else {
            false
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// An atom-ish run: everything up to a space, parenthesis or CR/LF.
    fn atom(&mut self) -> Option<&'a [u8]> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if matches!(c, b' ' | b'(' | b')' | b'\r' | b'\n' | b'"' | b'{') {
                break;
            }
            self.pos += 1;
        }
        (self.pos > start).then(|| self.b.get(start..self.pos).unwrap_or(&[]))
    }

    /// A quoted string or a literal (`{n}` / `{n+}`, whose data follows the CRLF inline).
    fn string(&mut self) -> Option<Vec<u8>> {
        if self.eat(b'"') {
            let mut out = Vec::new();
            loop {
                let c = self.peek()?;
                self.pos += 1;
                match c {
                    b'"' => return Some(out),
                    b'\\' => {
                        out.push(self.peek()?);
                        self.pos += 1;
                    }
                    b'\r' | b'\n' => return None,
                    c => out.push(c),
                }
            }
        }
        if self.eat(b'{') {
            let start = self.pos;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
            let n: usize = std::str::from_utf8(self.b.get(start..self.pos)?).ok()?.parse().ok()?;
            self.eat(b'+');
            if !self.eat(b'}') || !self.eat(b'\r') || !self.eat(b'\n') {
                return None;
            }
            let data = self.b.get(self.pos..self.pos.checked_add(n)?)?;
            self.pos += n;
            return Some(data.to_vec());
        }
        self.atom().map(<[u8]>::to_vec)
    }

    fn end(&mut self) -> bool {
        self.eat(b'\r') && self.eat(b'\n') && self.pos == self.b.len()
    }
}

fn seqset(text: &[u8]) -> Option<SeqSet> {
    let num = |t: &[u8]| -> Option<SeqNum> {
        if t == b"*" {
            return Some(SeqNum::Star);
        }
        let n: u32 = std::str::from_utf8(t).ok()?.parse().ok()?;
        (n > 0).then_some(SeqNum::Num(n))
    };
    let mut ranges = Vec::new();
    for part in text.split(|b| *b == b',') {
        let mut it = part.splitn(2, |b| *b == b':');
        let lo = num(it.next()?)?;
        let hi = match it.next() {
            Some(h) => num(h)?,
            None => lo,
        };
        ranges.push(SeqRange { lo, hi });
        if ranges.len() > 10_000 {
            return None;
        }
    }
    (!ranges.is_empty()).then_some(SeqSet(ranges))
}

fn name(raw: &[u8]) -> Result<String, ParseError> {
    utf7::decode(std::str::from_utf8(raw).map_err(|_| ParseError::BadName)?).ok_or(ParseError::BadName)
}

fn text(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).into_owned()
}

/// `Some` when the frame is one of ours (the result may still be a syntax error).
pub fn parse(frame: &[u8]) -> Option<Result<Command, ParseError>> {
    let mut lx = Lex { b: frame, pos: 0 };
    let tag = lx.atom()?;
    if tag.iter().any(|c| matches!(c, b'+' | b'*' | b'}' | b']')) || !lx.sp() {
        return None;
    }
    let verb = lx.atom()?.to_ascii_uppercase();
    let tag = text(tag);
    let done = |r: Result<Body, ParseError>| Some(r.map(|body| Command { tag: tag.clone(), body }));
    match verb.as_slice() {
        b"NAMESPACE" => done(if lx.end() { Ok(Body::Namespace) } else { Err(ParseError::Syntax) }),
        b"ID" => done(id(&mut lx)),
        b"UID" => {
            let save = lx.pos;
            lx.sp();
            let sub = lx.atom().map(<[u8]>::to_ascii_uppercase);
            if sub.as_deref() != Some(b"EXPUNGE") {
                lx.pos = save;
                return None; // UID FETCH/STORE/...: the library's
            }
            done(uid_expunge(&mut lx))
        }
        b"LIST" => {
            // Extended only with selection options `(...)` first or a RETURN clause; plain LIST
            // stays with the library.
            let save = lx.pos;
            lx.sp();
            let extended = lx.peek() == Some(b'(') || contains_return(frame);
            lx.pos = save;
            if extended {
                done(list_ext(&mut lx))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn contains_return(frame: &[u8]) -> bool {
    frame.windows(8).any(|w| w.eq_ignore_ascii_case(b" RETURN ") || w.eq_ignore_ascii_case(b" RETURN("))
}

fn id(lx: &mut Lex<'_>) -> Result<Body, ParseError> {
    if !lx.sp() {
        return Err(ParseError::Syntax);
    }
    let mut pairs = Vec::new();
    if lx.atom().is_some_and(|a| a.eq_ignore_ascii_case(b"NIL")) {
        return if lx.end() { Ok(Body::Id { client: pairs }) } else { Err(ParseError::Syntax) };
    }
    if !lx.eat(b'(') {
        return Err(ParseError::Syntax);
    }
    loop {
        if lx.eat(b')') {
            break;
        }
        if !pairs.is_empty() && !lx.sp() {
            return Err(ParseError::Syntax);
        }
        let k = lx.string().ok_or(ParseError::Syntax)?;
        if !lx.sp() {
            return Err(ParseError::Syntax);
        }
        let v = match lx.string() {
            Some(v) if v.eq_ignore_ascii_case(b"NIL") && lx.b.get(lx.pos.saturating_sub(1)) != Some(&b'"') => Vec::new(),
            Some(v) => v,
            None => return Err(ParseError::Syntax),
        };
        if pairs.len() < MAX_ID_PAIRS {
            pairs.push((text(&k), text(&v)));
        }
    }
    if lx.end() {
        Ok(Body::Id { client: pairs })
    } else {
        Err(ParseError::Syntax)
    }
}

fn uid_expunge(lx: &mut Lex<'_>) -> Result<Body, ParseError> {
    if !lx.sp() {
        return Err(ParseError::Syntax);
    }
    let set = lx.atom().and_then(seqset).ok_or(ParseError::Syntax)?;
    if lx.end() {
        Ok(Body::UidExpunge { set })
    } else {
        Err(ParseError::Syntax)
    }
}

fn list_ext(lx: &mut Lex<'_>) -> Result<Body, ParseError> {
    let (mut subscribed_only, mut ret_sub, mut ret_children, mut ret_special) = (false, false, false, false);
    if !lx.sp() {
        return Err(ParseError::Syntax);
    }
    if lx.eat(b'(') {
        // Selection options: only SUBSCRIBED is understood; others are refused.
        while !lx.eat(b')') {
            lx.sp();
            let o = lx.atom().ok_or(ParseError::Syntax)?.to_ascii_uppercase();
            match o.as_slice() {
                b"SUBSCRIBED" => subscribed_only = true,
                b"RECURSIVEMATCH" | b"REMOTE" => {}
                _ => return Err(ParseError::Syntax),
            }
        }
        if !lx.sp() {
            return Err(ParseError::Syntax);
        }
    }
    let reference = name(&lx.string().ok_or(ParseError::Syntax)?)?;
    if !lx.sp() {
        return Err(ParseError::Syntax);
    }
    let mut patterns = Vec::new();
    if lx.eat(b'(') {
        while !lx.eat(b')') {
            if !patterns.is_empty() && !lx.sp() {
                return Err(ParseError::Syntax);
            }
            if patterns.len() >= MAX_PATTERNS {
                return Err(ParseError::Syntax);
            }
            patterns.push(name(&lx.string().ok_or(ParseError::Syntax)?)?);
        }
    } else {
        patterns.push(name(&lx.string().ok_or(ParseError::Syntax)?)?);
    }
    if lx.sp() {
        let kw = lx.atom().ok_or(ParseError::Syntax)?;
        if !kw.eq_ignore_ascii_case(b"RETURN") {
            return Err(ParseError::Syntax);
        }
        lx.sp();
        if !lx.eat(b'(') {
            return Err(ParseError::Syntax);
        }
        while !lx.eat(b')') {
            lx.sp();
            let o = lx.atom().ok_or(ParseError::Syntax)?.to_ascii_uppercase();
            match o.as_slice() {
                b"SUBSCRIBED" => ret_sub = true,
                b"CHILDREN" => ret_children = true,
                b"SPECIAL-USE" => ret_special = true,
                _ => return Err(ParseError::Syntax),
            }
        }
    }
    if !lx.end() {
        return Err(ParseError::Syntax);
    }
    let _ = (ret_children, ret_special); // children and special-use attributes are always returned
    Ok(Body::ListExt { reference, patterns, subscribed_only, return_subscribed: ret_sub })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> Body {
        parse(s.as_bytes()).unwrap().unwrap().body
    }

    #[test]
    fn namespace_id_and_uid_expunge() {
        assert_eq!(ok("a NAMESPACE\r\n"), Body::Namespace);
        assert_eq!(ok("a namespace\r\n"), Body::Namespace);
        assert_eq!(ok("a ID NIL\r\n"), Body::Id { client: vec![] });
        assert_eq!(
            ok("a ID (\"name\" \"Thunderbird\" \"version\" \"115.0\")\r\n"),
            Body::Id { client: vec![("name".into(), "Thunderbird".into()), ("version".into(), "115.0".into())] }
        );
        assert_eq!(ok("a ID (\"x\" NIL)\r\n"), Body::Id { client: vec![("x".into(), String::new())] });
        assert!(matches!(ok("a UID EXPUNGE 5:7,9\r\n"), Body::UidExpunge { set } if set.0.len() == 2));
        assert!(parse(b"a UID FETCH 1 FLAGS\r\n").is_none(), "other UID commands belong to the library");
        assert!(parse(b"a NOOP\r\n").is_none());
        for bad in ["a NAMESPACE x\r\n", "a ID\r\n", "a ID (\"k\")\r\n", "a UID EXPUNGE\r\n", "a UID EXPUNGE 0\r\n", "a UID EXPUNGE 1:\r\n"] {
            assert_eq!(parse(bad.as_bytes()).unwrap().err(), Some(ParseError::Syntax), "{bad:?}");
        }
    }

    #[test]
    fn extended_list_forms() {
        assert_eq!(
            ok("a LIST \"\" \"*\" RETURN (SPECIAL-USE CHILDREN)\r\n"),
            Body::ListExt { reference: String::new(), patterns: vec!["*".into()], subscribed_only: false, return_subscribed: false }
        );
        assert_eq!(
            ok("a LIST (SUBSCRIBED) \"\" (\"INBOX\" \"Sent/%\") RETURN (SUBSCRIBED)\r\n"),
            Body::ListExt { reference: String::new(), patterns: vec!["INBOX".into(), "Sent/%".into()], subscribed_only: true, return_subscribed: true }
        );
        assert!(parse(b"a LIST \"\" \"*\"\r\n").is_none(), "plain LIST stays with the library");
        assert_eq!(parse(b"a LIST (BOGUS) \"\" \"*\"\r\n").unwrap().err(), Some(ParseError::Syntax));
        assert_eq!(parse(b"a LIST \"\" \"*\" RETURN (BOGUS)\r\n").unwrap().err(), Some(ParseError::Syntax));
        assert_eq!(parse(b"a LIST \"\" \"&AGE-\" RETURN (SUBSCRIBED)\r\n").unwrap().err(), Some(ParseError::BadName));
    }

    #[test]
    fn literals_and_garbage() {
        assert_eq!(ok("a ID ({4+}\r\nname {2+}\r\nTB)\r\n"), Body::Id { client: vec![("name".into(), "TB".into())] });
        for junk in ["", "a", "a ID (", "a ID ({999}\r\n", "a LIST ((((((", "a UID", "* ID NIL\r\n"] {
            let _ = parse(junk.as_bytes());
        }
    }
}
