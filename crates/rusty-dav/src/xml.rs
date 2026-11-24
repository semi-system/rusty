//! The XML of WebDAV requests (RFC 4918, 4791, 6352, 6578): a strict reader of the small subset those bodies use, and the escapes for the answers. There is no DTD, no entity beyond the five predefined ones and
//! the numeric references, no processing instruction but the XML declaration, no external anything; a document is bounded in size, depth and element count, so a hostile body costs a bounded amount of work.
//! Names are resolved against the namespaces in scope (`xmlns` and `xmlns:p`), so the answer to `<D:prop>` and to `<d:prop>` is the same.

pub const MAX_BYTES: usize = 64 * 1024;
pub const MAX_DEPTH: usize = 16;
pub const MAX_ELEMENTS: usize = 10_000;
pub const MAX_ATTRS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Element(Element),
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    /// The namespace URI (empty when the name has none).
    pub ns: String,
    /// The local name.
    pub name: String,
    /// The attributes that are not namespace declarations, by local name (a prefixed attribute is kept under its local name too: the subset has no clash).
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Element {
    pub fn is(&self, ns: &str, name: &str) -> bool {
        self.ns == ns && self.name == name
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|n| match n {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        })
    }

    /// The first child element with this name.
    pub fn child(&self, ns: &str, name: &str) -> Option<&Element> {
        self.elements().find(|e| e.is(ns, name))
    }

    /// The text directly inside, trimmed of nothing: as written.
    pub fn text(&self) -> String {
        self.children
            .iter()
            .filter_map(|n| match n {
                Node::Text(t) => Some(t.as_str()),
                Node::Element(_) => None,
            })
            .collect()
    }
}

/// Text for the inside of an element or a double-quoted attribute: `& < > "` escaped, control characters (other than tab, line feed and carriage return) dropped.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if c.is_control() && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

struct Scope {
    /// `(prefix, uri)`; the empty prefix is the default namespace.
    bindings: Vec<(String, String)>,
}

struct Reader<'a> {
    s: &'a str,
    at: usize,
    elements: usize,
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':')
}

impl<'a> Reader<'a> {
    fn rest(&self) -> &'a str {
        self.s.get(self.at..).unwrap_or("")
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.rest().starts_with(lit) {
            self.at += lit.len();
            true
        } else {
            false
        }
    }

    fn skip_space(&mut self) {
        let n = self.rest().len() - self.rest().trim_start_matches([' ', '\t', '\r', '\n']).len();
        self.at += n;
    }

    fn name(&mut self) -> Result<&'a str, &'static str> {
        let rest = self.rest();
        let mut end = 0;
        for (i, c) in rest.char_indices() {
            if (i == 0 && !is_name_start(c)) || !is_name_char(c) {
                break;
            }
            end = i + c.len_utf8();
        }
        if end == 0 {
            return Err("a name is expected");
        }
        self.at += end;
        Ok(rest.get(..end).unwrap_or(""))
    }

    /// Text up to `<`, with its references resolved.
    fn text(&mut self) -> Result<String, &'static str> {
        let rest = self.rest();
        let end = rest.find('<').unwrap_or(rest.len());
        let raw = rest.get(..end).unwrap_or("");
        self.at += end;
        unescape(raw)
    }

    fn attr_value(&mut self) -> Result<String, &'static str> {
        let quote = match self.rest().chars().next() {
            Some(q @ ('"' | '\'')) => q,
            _ => return Err("an attribute value is quoted"),
        };
        self.at += 1;
        let rest = self.rest();
        let Some(end) = rest.find(quote) else { return Err("an attribute value is not closed") };
        let raw = rest.get(..end).unwrap_or("");
        if raw.contains('<') {
            return Err("an attribute value holds no <");
        }
        self.at += end + 1;
        unescape(raw)
    }

    fn element(&mut self, scopes: &mut Vec<Scope>, depth: usize) -> Result<Element, &'static str> {
        if depth >= MAX_DEPTH {
            return Err("elements are nested too deep");
        }
        self.elements += 1;
        if self.elements > MAX_ELEMENTS {
            return Err("too many elements");
        }
        if !self.eat("<") {
            return Err("an element is expected");
        }
        let qname = self.name()?;
        let mut attrs: Vec<(String, String)> = Vec::new();
        let mut scope = Scope { bindings: Vec::new() };
        let mut empty = false;
        loop {
            let before = self.at;
            self.skip_space();
            if self.eat("/>") {
                empty = true;
                break;
            }
            if self.eat(">") {
                break;
            }
            if self.at == before {
                return Err("a space is expected between attributes");
            }
            let aname = self.name()?;
            self.skip_space();
            if !self.eat("=") {
                return Err("an attribute has a value");
            }
            self.skip_space();
            let value = self.attr_value()?;
            if aname == "xmlns" {
                scope.bindings.push((String::new(), value));
            } else if let Some(p) = aname.strip_prefix("xmlns:") {
                if p.is_empty() || p.contains(':') {
                    return Err("a namespace prefix is not valid");
                }
                scope.bindings.push((p.to_owned(), value));
            } else {
                if attrs.len() >= MAX_ATTRS {
                    return Err("too many attributes");
                }
                let local = aname.rsplit(':').next().unwrap_or(aname);
                if attrs.iter().any(|(n, _)| n == local) {
                    return Err("an attribute is repeated");
                }
                attrs.push((local.to_owned(), value));
            }
        }
        scopes.push(scope);
        let (prefix, local) = match qname.split_once(':') {
            Some((p, l)) if !l.contains(':') && !p.is_empty() && !l.is_empty() => (p, l),
            Some(_) => return Err("a name is not valid"),
            None => ("", qname),
        };
        let ns = lookup(scopes, prefix).ok_or("a namespace prefix is not declared").or_else(|e| if prefix.is_empty() { Ok(String::new()) } else { Err(e) })?;
        let mut el = Element { ns, name: local.to_owned(), attrs, children: Vec::new() };
        if !empty {
            loop {
                if self.eat("</") {
                    let close = self.name()?;
                    self.skip_space();
                    if !self.eat(">") {
                        return Err("an end tag is not closed");
                    }
                    if close != qname {
                        return Err("an end tag does not match its start tag");
                    }
                    break;
                }
                if self.eat("<!--") {
                    let Some(end) = self.rest().find("-->") else { return Err("a comment is not closed") };
                    self.at += end + 3;
                } else if self.eat("<![CDATA[") {
                    let Some(end) = self.rest().find("]]>") else { return Err("a CDATA section is not closed") };
                    el.children.push(Node::Text(self.rest().get(..end).unwrap_or("").to_owned()));
                    self.at += end + 3;
                } else if self.rest().starts_with("<!") || self.rest().starts_with("<?") {
                    return Err("a declaration or an instruction is not accepted here");
                } else if self.rest().starts_with('<') {
                    let child = self.element(scopes, depth + 1)?;
                    el.children.push(Node::Element(child));
                } else if self.rest().is_empty() {
                    return Err("an element is not closed");
                } else {
                    let t = self.text()?;
                    el.children.push(Node::Text(t));
                }
            }
        }
        scopes.pop();
        Ok(el)
    }
}

fn lookup(scopes: &[Scope], prefix: &str) -> Option<String> {
    scopes.iter().rev().flat_map(|s| s.bindings.iter().rev()).find(|(p, _)| p == prefix).map(|(_, u)| u.clone())
}

/// `&lt; &gt; &amp; &quot; &apos; &#10; &#x0A;`; anything else after an ampersand is an error (no entity is ever defined).
fn unescape(raw: &str) -> Result<String, &'static str> {
    if !raw.contains('&') {
        return Ok(raw.to_owned());
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        out.push_str(rest.get(..i).unwrap_or(""));
        let after = rest.get(i + 1..).unwrap_or("");
        let Some(end) = after.find(';') else { return Err("a reference is not closed") };
        let ent = after.get(..end).unwrap_or("");
        match ent {
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "amp" => out.push('&'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let code = if let Some(h) = ent.strip_prefix("#x") { u32::from_str_radix(h, 16).ok() } else { ent.strip_prefix('#').and_then(|d| d.parse().ok()) };
                match code.and_then(char::from_u32) {
                    Some(c) if !c.is_control() || matches!(c, '\t' | '\n' | '\r') => out.push(c),
                    _ => return Err("a reference is not known"),
                }
            }
        }
        rest = after.get(end + 1..).unwrap_or("");
    }
    out.push_str(rest);
    Ok(out)
}

/// Read a document: an optional XML declaration, then one element, then nothing but space and comments.
pub fn parse(bytes: &[u8]) -> Result<Element, &'static str> {
    if bytes.len() > MAX_BYTES {
        return Err("the document is too large");
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "the document is not UTF-8")?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut r = Reader { s: text, at: 0, elements: 0 };
    r.skip_space();
    if r.rest().starts_with("<?xml") {
        let Some(end) = r.rest().find("?>") else { return Err("the declaration is not closed") };
        r.at += end + 2;
    }
    loop {
        r.skip_space();
        if r.eat("<!--") {
            let Some(end) = r.rest().find("-->") else { return Err("a comment is not closed") };
            r.at += end + 3;
        } else {
            break;
        }
    }
    if r.rest().starts_with("<!") {
        return Err("a document type is not accepted");
    }
    let root = r.element(&mut Vec::new(), 0)?;
    loop {
        r.skip_space();
        if r.eat("<!--") {
            let Some(end) = r.rest().find("-->") else { return Err("a comment is not closed") };
            r.at += end + 3;
        } else {
            break;
        }
    }
    if !r.rest().is_empty() {
        return Err("text after the document");
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_resolved_against_the_namespaces_in_scope() {
        let e = parse(b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:propfind xmlns:D=\"DAV:\"><D:prop><D:displayname/><c:calendar-home-set xmlns:c=\"urn:ietf:params:xml:ns:caldav\"/><x xmlns=\"urn:x\"/></D:prop></D:propfind>").unwrap();
        assert!(e.is("DAV:", "propfind"));
        let prop = e.child("DAV:", "prop").unwrap();
        let names: Vec<(String, String)> = prop.elements().map(|c| (c.ns.clone(), c.name.clone())).collect();
        assert_eq!(names, [("DAV:".into(), "displayname".into()), ("urn:ietf:params:xml:ns:caldav".into(), "calendar-home-set".into()), ("urn:x".into(), "x".into())]);
        // Another prefix for the same namespace is the same name.
        let e2 = parse(b"<d:propfind xmlns:d='DAV:'><d:prop/></d:propfind>").unwrap();
        assert!(e2.child("DAV:", "prop").is_some());
    }

    #[test]
    fn text_attributes_references_comments_and_cdata() {
        let e = parse(b"<a xmlns=\"urn:a\" k=\"v &amp; w\" j='&#65;'><!-- c --><b>x &lt; y &#x42; &quot;</b><![CDATA[<raw>]]></a>").unwrap();
        assert_eq!((e.attr("k"), e.attr("j")), (Some("v & w"), Some("A")));
        assert_eq!(e.child("urn:a", "b").unwrap().text(), "x < y B \"");
        assert_eq!(e.text(), "<raw>");
    }

    #[test]
    fn what_could_hurt_is_refused() {
        for bad in [
            &b""[..],
            b"not xml",
            b"<!DOCTYPE a [<!ENTITY x \"y\">]><a/>",
            b"<a>&x;</a>",
            b"<a>&amp</a>",
            b"<a><b></a></b>",
            b"<a><b></b>",
            b"<a/><b/>",
            b"<a/>junk",
            b"<p:a/>",
            b"<a k=v/>",
            b"<a k=\"1\" k=\"2\"/>",
            b"<a k=\"<\"/>",
            b"<a><?pi x?></a>",
            b"<a>&#0;</a>",
            b"<a>&#1114112;</a>",
            b"<a xmlns:=\"u\"/>",
            b"<a:b:c xmlns:a=\"u\"/>",
            b"\xff\xfe<a/>",
            b"<a\x01/>",
        ] {
            assert!(parse(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn the_limits_hold() {
        let deep = format!("{}{}", "<a>".repeat(MAX_DEPTH + 1), "</a>".repeat(MAX_DEPTH + 1));
        assert!(parse(deep.as_bytes()).is_err());
        let ok = format!("{}{}", "<a>".repeat(MAX_DEPTH), "</a>".repeat(MAX_DEPTH));
        assert!(parse(ok.as_bytes()).is_ok());
        let wide = format!("<a>{}</a>", "<b/>".repeat(MAX_ELEMENTS));
        assert!(parse(wide.as_bytes()).is_err());
        let big = format!("<a>{}</a>", "x".repeat(MAX_BYTES));
        assert!(parse(big.as_bytes()).is_err());
        let attrs = format!("<a {}/>", (0..=MAX_ATTRS).map(|i| format!("k{i}=\"1\"")).collect::<Vec<_>>().join(" "));
        assert!(parse(attrs.as_bytes()).is_err());
    }

    #[test]
    fn the_escape_leaves_nothing_that_could_open_a_tag() {
        assert_eq!(escape("a<b>&\"c\u{1}\td"), "a&lt;b&gt;&amp;&quot;c\td");
        assert_eq!(parse(format!("<a>{}</a>", escape("<x>&y\"")).as_bytes()).unwrap().text(), "<x>&y\"");
    }
}
