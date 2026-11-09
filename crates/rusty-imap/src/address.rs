//! Address lists of ENVELOPE (RFC 3501 section 7.4.2): `name <local@host>`, bare addresses,
//! `addr (comment)`, quoted display names and groups. Lenient and bounded: whatever cannot be
//! made sense of is skipped, never an error.

/// Most addresses parsed from one header.
pub const MAX_ADDRESSES: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addr {
    /// `(name NIL mailbox host)`
    Mailbox { name: Option<Vec<u8>>, mailbox: Vec<u8>, host: Option<Vec<u8>> },
    /// `(NIL NIL group NIL)`
    GroupStart(Vec<u8>),
    /// `(NIL NIL NIL NIL)`
    GroupEnd,
}

/// Parse a header value into address entries.
pub fn parse(value: &[u8]) -> Vec<Addr> {
    let mut out = Vec::new();
    let mut cur: Vec<u8> = Vec::new(); // element text, comments removed
    let mut comment: Vec<u8> = Vec::new(); // first comment of the element
    let mut in_group = false;
    let (mut quoted, mut angle, mut depth) = (false, false, 0usize);
    let mut escaped = false;

    let flush = |cur: &mut Vec<u8>, comment: &mut Vec<u8>, out: &mut Vec<Addr>| {
        if out.len() < MAX_ADDRESSES {
            if let Some(a) = element(cur, comment) {
                out.push(a);
            }
        }
        cur.clear();
        comment.clear();
    };

    for &b in value {
        if depth > 0 {
            // Inside a comment.
            if escaped {
                escaped = false;
                comment.push(b);
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'(' {
                depth += 1;
                comment.push(b);
            } else if b == b')' {
                depth -= 1;
                if depth > 0 {
                    comment.push(b);
                }
            } else {
                comment.push(b);
            }
            continue;
        }
        if quoted {
            cur.push(b);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                quoted = false;
            }
            continue;
        }
        match b {
            b'"' => {
                quoted = true;
                cur.push(b);
            }
            b'(' => depth = 1,
            b'<' => {
                angle = true;
                cur.push(b);
            }
            b'>' => {
                angle = false;
                cur.push(b);
            }
            b':' if !angle && !in_group => {
                // Group start: the text so far is the group name.
                let name = unquote(crate::headers::trim(&cur));
                if out.len() < MAX_ADDRESSES {
                    out.push(Addr::GroupStart(name));
                }
                in_group = true;
                cur.clear();
                comment.clear();
            }
            b';' if in_group && !angle => {
                flush(&mut cur, &mut comment, &mut out);
                if out.len() < MAX_ADDRESSES {
                    out.push(Addr::GroupEnd);
                }
                in_group = false;
            }
            b',' if !angle => flush(&mut cur, &mut comment, &mut out),
            _ => cur.push(b),
        }
    }
    flush(&mut cur, &mut comment, &mut out);
    out
}

/// Remove one level of quoting (`"a \"b\""` to `a "b"`); anything else is returned as it is.
fn unquote(v: &[u8]) -> Vec<u8> {
    let v = crate::headers::trim(v);
    let Some(inner) = v.strip_prefix(b"\"").and_then(|r| r.strip_suffix(b"\"")) else {
        return v.to_vec();
    };
    let mut out = Vec::with_capacity(inner.len());
    let mut esc = false;
    for &b in inner {
        if esc {
            out.push(b);
            esc = false;
        } else if b == b'\\' {
            esc = true;
        } else {
            out.push(b);
        }
    }
    out
}

/// Position of the last `@` outside a quoted string.
fn last_at(v: &[u8]) -> Option<usize> {
    let (mut q, mut esc, mut at) = (false, false, None);
    for (i, &b) in v.iter().enumerate() {
        if esc {
            esc = false;
        } else if q && b == b'\\' {
            esc = true;
        } else if b == b'"' {
            q = !q;
        } else if b == b'@' && !q {
            at = Some(i);
        }
    }
    at
}

fn element(text: &[u8], comment: &[u8]) -> Option<Addr> {
    let text = crate::headers::trim(text);
    // `name <addr>`: find the angle bracket outside quotes.
    let mut q = false;
    let mut esc = false;
    let mut lt = None;
    for (i, &b) in text.iter().enumerate() {
        if esc {
            esc = false;
        } else if q && b == b'\\' {
            esc = true;
        } else if b == b'"' {
            q = !q;
        } else if b == b'<' && !q {
            lt = Some(i);
            break;
        }
    }
    let (name_part, addr_part): (&[u8], &[u8]) = match lt {
        Some(i) => {
            let name = text.get(..i).unwrap_or(&[]);
            let rest = text.get(i + 1..).unwrap_or(&[]);
            let end = rest.iter().position(|b| *b == b'>').unwrap_or(rest.len());
            (name, rest.get(..end).unwrap_or(&[]))
        }
        None => (&[], text),
    };
    let mut name = unquote(name_part);
    if name.is_empty() {
        name = crate::headers::trim(comment).to_vec();
    }
    let mut addr = crate::headers::trim(addr_part);
    // Obsolete source route: `@a,@b:user@host`.
    if addr.first() == Some(&b'@') {
        if let Some(i) = addr.iter().position(|b| *b == b':') {
            addr = crate::headers::trim(addr.get(i + 1..).unwrap_or(&[]));
        }
    }
    if addr.is_empty() && name.is_empty() {
        return None;
    }
    let name = (!name.is_empty()).then_some(name);
    match last_at(addr) {
        Some(i) => {
            let local = unquote(addr.get(..i).unwrap_or(&[]));
            let host = crate::headers::trim(addr.get(i + 1..).unwrap_or(&[])).to_vec();
            Some(Addr::Mailbox { name, mailbox: local, host: Some(host) })
        }
        None if addr.is_empty() => None,
        None => Some(Addr::Mailbox { name, mailbox: unquote(addr), host: None }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mb(name: Option<&str>, m: &str, h: Option<&str>) -> Addr {
        Addr::Mailbox { name: name.map(|n| n.as_bytes().to_vec()), mailbox: m.as_bytes().to_vec(), host: h.map(|h| h.as_bytes().to_vec()) }
    }

    #[test]
    fn plain_named_quoted_and_commented_forms() {
        assert_eq!(parse(b"a@x.com"), vec![mb(None, "a", Some("x.com"))]);
        assert_eq!(parse(b"Alice <a@x.com>, \"Doe, John\" <j@y.org>"), vec![mb(Some("Alice"), "a", Some("x.com")), mb(Some("Doe, John"), "j", Some("y.org"))]);
        assert_eq!(parse(b"old@x.com (Old Name)"), vec![mb(Some("Old Name"), "old", Some("x.com"))]);
        assert_eq!(parse(b"\"quoted local\"@x.com"), vec![mb(None, "quoted local", Some("x.com"))]);
        assert_eq!(parse(b"<@relay:user@x.com>"), vec![mb(None, "user", Some("x.com"))]);
        assert_eq!(parse(b"nohost"), vec![mb(None, "nohost", None)]);
    }

    #[test]
    fn groups() {
        assert_eq!(
            parse(b"Team: a@x.com, b@y.com;, c@z.com"),
            vec![
                Addr::GroupStart(b"Team".to_vec()),
                mb(None, "a", Some("x.com")),
                mb(None, "b", Some("y.com")),
                Addr::GroupEnd,
                mb(None, "c", Some("z.com")),
            ]
        );
        assert_eq!(parse(b"undisclosed-recipients:;"), vec![Addr::GroupStart(b"undisclosed-recipients".to_vec()), Addr::GroupEnd]);
    }

    #[test]
    fn junk_and_hostile_input_never_panics_and_is_bounded() {
        for junk in [&b""[..], b",,,", b"<", b">", b"((((", b"\"", b"a@@b", b":;:;", b"\\", b"<<<>>>"] {
            let _ = parse(junk);
        }
        let big = "a@x.com,".repeat(10_000);
        assert!(parse(big.as_bytes()).len() <= MAX_ADDRESSES + 1);
        let deep = "(".repeat(100_000);
        let _ = parse(deep.as_bytes());
    }
}
