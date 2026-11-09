//! MIME structure of a stored message: the part tree, BODYSTRUCTURE, ENVELOPE and BODY[section]
//! extraction. Pure functions over the raw bytes; parsing never fails and never allocates more
//! than the limits allow (a hostile message degrades to fewer, flatter parts, ultimately one
//! `text/plain` part, instead of hanging or panicking).

use std::ops::Range;

use crate::command::Section;

use crate::headers::{self, find, line_at, Field};
use crate::wire;

/// Deepest nesting of multipart / message/rfc822 parts that is parsed; deeper content is one
/// opaque `text/plain` part.
pub const MAX_DEPTH: usize = 16;
/// Most parts in one message; further parts are ignored.
pub const MAX_PARTS: usize = 512;
/// Largest header block that is parsed as headers.
pub const MAX_HEADER_BYTES: usize = 256 * 1024;
/// Most parameters read from one Content-Type / Content-Disposition.
const MAX_PARAMS: usize = 32;
/// Longest parameter value kept.
const MAX_PARAM_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// Header lines of the part (final line break included, the blank separator excluded).
    pub header: Range<usize>,
    /// The body (after the blank separator line).
    pub body: Range<usize>,
}

type Params = Vec<(String, Vec<u8>)>;

#[derive(Debug, Clone)]
struct Info {
    mtype: String,
    subtype: String,
    params: Params,
    id: Option<Vec<u8>>,
    desc: Option<Vec<u8>>,
    enc: String,
    md5: Option<Vec<u8>>,
    disposition: Option<(String, Params)>,
    language: Option<Vec<u8>>,
    location: Option<Vec<u8>>,
}

impl Info {
    fn text_plain() -> Info {
        Info {
            mtype: "text".into(),
            subtype: "plain".into(),
            params: vec![("charset".into(), b"us-ascii".to_vec())],
            id: None,
            desc: None,
            enc: "7BIT".into(),
            md5: None,
            disposition: None,
            language: None,
            location: None,
        }
    }
}

#[derive(Debug, Clone)]
enum Kind {
    Leaf,
    Multi(Vec<Node>),
    /// `message/rfc822`: the embedded message.
    Msg(Box<Node>),
}

#[derive(Debug, Clone)]
struct Node {
    part: Part,
    info: Info,
    kind: Kind,
    /// Text lines of the body (text parts and embedded messages).
    lines: u32,
}

#[derive(Debug, Clone)]
pub struct Mime {
    root: Node,
    parts: usize,
    depth: usize,
}

struct Counter {
    parts: usize,
    depth: usize,
}

/// Parse the structure of `raw`.
pub fn parse(raw: &[u8]) -> Mime {
    let mut c = Counter { parts: 0, depth: 0 };
    let root = build(raw, 0..raw.len(), 0, false, &mut c);
    Mime { root, parts: c.parts, depth: c.depth }
}

/// Header lines and body start of the part occupying `range`.
fn split(raw: &[u8], range: &Range<usize>) -> (Range<usize>, usize) {
    let mut pos = range.start;
    let slice = raw.get(..range.end).unwrap_or(raw);
    while let Some((start, end, next)) = line_at(slice, pos) {
        if start == end {
            // The blank separator line.
            return (range.start..start, next.min(range.end));
        }
        if next - range.start > MAX_HEADER_BYTES {
            break;
        }
        pos = next;
    }
    if pos >= range.end && pos - range.start <= MAX_HEADER_BYTES {
        // Only header lines, no blank line: there is no body.
        return (range.start..range.end, range.end);
    }
    // Header block over the cap: not headers at all.
    (range.start..range.start, range.start)
}

fn count_lines(body: &[u8]) -> u32 {
    let lf = body.iter().filter(|b| **b == b'\n').count();
    let extra = usize::from(!body.is_empty() && body.last() != Some(&b'\n'));
    u32::try_from(lf + extra).unwrap_or(u32::MAX)
}

fn param_value(v: &[u8]) -> Vec<u8> {
    let mut v = v.to_vec();
    v.truncate(MAX_PARAM_BYTES);
    v
}

/// `;name=value` parameters of a structured header value (the part after the media type or the
/// disposition type).
fn parse_params(mut rest: &[u8]) -> Params {
    let mut out = Params::new();
    while out.len() < MAX_PARAMS {
        let skip = rest.iter().position(|b| *b != b';' && *b != b' ' && *b != b'\t').unwrap_or(rest.len());
        rest = rest.get(skip..).unwrap_or(&[]);
        if rest.is_empty() {
            break;
        }
        let semi = rest.iter().position(|b| *b == b';');
        let Some(eq) = rest.iter().position(|b| *b == b'=') else {
            // A bare token without a value: skip it.
            match semi {
                Some(s) => {
                    rest = rest.get(s..).unwrap_or(&[]);
                    continue;
                }
                None => break,
            }
        };
        if semi.is_some_and(|s| s < eq) {
            rest = rest.get(semi.unwrap_or(0)..).unwrap_or(&[]);
            continue;
        }
        let name = String::from_utf8_lossy(headers::trim(rest.get(..eq).unwrap_or(&[]))).to_ascii_lowercase();
        let tail = rest.get(eq + 1..).unwrap_or(&[]);
        let lead = tail.iter().position(|b| *b != b' ' && *b != 0x09).unwrap_or(tail.len());
        let after = tail.get(lead..).unwrap_or(&[]);
        let (value, consumed) = if after.first() == Some(&b'"') {
            let mut v = Vec::new();
            let (mut esc, mut i) = (false, 1);
            while let Some(&b) = after.get(i) {
                i += 1;
                if esc {
                    v.push(b);
                    esc = false;
                } else if b == b'\\' {
                    esc = true;
                } else if b == b'"' {
                    break;
                } else {
                    v.push(b);
                }
            }
            (v, i)
        } else {
            let end = after.iter().position(|b| *b == b';').unwrap_or(after.len());
            (headers::trim(after.get(..end).unwrap_or(&[])).to_vec(), end)
        };
        rest = tail.get(lead + consumed..).unwrap_or(&[]);
        if !name.is_empty() && name.bytes().all(|b| b.is_ascii_graphic()) {
            out.push((name, param_value(&value)));
        }
    }
    out
}

fn info_from(fields: &[Field], digest_child: bool) -> Info {
    let mut info = Info::text_plain();
    if digest_child {
        info.mtype = "message".into();
        info.subtype = "rfc822".into();
        info.params.clear();
    }
    if let Some(f) = find(fields, "content-type") {
        let (head, rest) = match f.value.iter().position(|b| *b == b';') {
            Some(i) => (f.value.get(..i).unwrap_or(&[]), f.value.get(i..).unwrap_or(&[])),
            None => (f.value.as_slice(), &[][0..0]),
        };
        let head = String::from_utf8_lossy(headers::trim(head)).to_ascii_lowercase();
        if let Some((t, s)) = head.split_once('/') {
            let ok = |x: &str| !x.is_empty() && x.len() <= 64 && x.bytes().all(|b| b.is_ascii_graphic() && !b"()<>@,;:\\\"/[]?=".contains(&b));
            if ok(t) && ok(s) {
                info.mtype = t.to_owned();
                info.subtype = s.to_owned();
                info.params = parse_params(rest);
            }
        }
    }
    if let Some(f) = find(fields, "content-transfer-encoding") {
        let e = String::from_utf8_lossy(headers::trim(&f.value)).to_ascii_uppercase();
        if !e.is_empty() && e.len() <= 32 && e.bytes().all(|b| b.is_ascii_graphic()) {
            info.enc = e;
        }
    }
    let val = |name: &str| find(fields, name).map(|f| f.value.clone());
    info.id = val("content-id");
    info.desc = val("content-description");
    info.md5 = val("content-md5");
    info.language = val("content-language");
    info.location = val("content-location");
    if let Some(f) = find(fields, "content-disposition") {
        let (head, rest) = match f.value.iter().position(|b| *b == b';') {
            Some(i) => (f.value.get(..i).unwrap_or(&[]), f.value.get(i..).unwrap_or(&[])),
            None => (f.value.as_slice(), &[][0..0]),
        };
        let kind = String::from_utf8_lossy(headers::trim(head)).to_ascii_lowercase();
        if !kind.is_empty() && kind.len() <= 32 && kind.bytes().all(|b| b.is_ascii_graphic()) {
            info.disposition = Some((kind, parse_params(rest)));
        }
    }
    info
}

/// Part ranges between the delimiter lines of `boundary` inside `body`.
fn split_multipart(raw: &[u8], body: &Range<usize>, boundary: &[u8], limit: usize) -> Vec<Range<usize>> {
    let slice = raw.get(..body.end).unwrap_or(raw);
    let mut parts = Vec::new();
    let mut open: Option<usize> = None;
    let mut pos = body.start;
    while let Some((start, end, next)) = line_at(slice, pos) {
        pos = next;
        let line = slice.get(start..end).unwrap_or(&[]);
        let Some(rest) = line.strip_prefix(b"--").and_then(|l| l.strip_prefix(boundary)) else {
            continue;
        };
        let closing = rest.starts_with(b"--");
        if !closing && !rest.iter().all(|b| *b == b' ' || *b == b'\t') {
            continue; // `--boundaryX`: another boundary that shares the prefix
        }
        if let Some(ps) = open.take() {
            // The line break before the delimiter belongs to the delimiter.
            let mut pe = start;
            if pe > ps && slice.get(pe - 1) == Some(&b'\n') {
                pe -= 1;
                if pe > ps && slice.get(pe - 1) == Some(&b'\r') {
                    pe -= 1;
                }
            }
            if parts.len() < limit {
                parts.push(ps..pe.max(ps));
            }
        }
        if closing {
            return parts;
        }
        open = Some(next.min(body.end));
    }
    if let Some(ps) = open {
        if parts.len() < limit {
            parts.push(ps..body.end.max(ps));
        }
    }
    parts
}

fn build(raw: &[u8], range: Range<usize>, depth: usize, digest_child: bool, c: &mut Counter) -> Node {
    c.parts += 1;
    c.depth = c.depth.max(depth);
    let (header, body_start) = split(raw, &range);
    let body = body_start..range.end.max(body_start);
    let fields = headers::fields(raw.get(header.clone()).unwrap_or(&[]));
    let mut info = info_from(&fields, digest_child);
    let part = Part { header, body: body.clone() };
    let body_bytes = raw.get(body.clone()).unwrap_or(&[]);
    let leaf = |info: Info, part: Part| {
        let lines = if info.mtype == "text" { count_lines(body_bytes) } else { 0 };
        Node { part, info, kind: Kind::Leaf, lines }
    };
    let too_deep = depth >= MAX_DEPTH || c.parts >= MAX_PARTS;
    if info.mtype == "multipart" {
        let boundary = info
            .params
            .iter()
            .find(|(n, _)| n == "boundary")
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty());
        if too_deep || boundary.is_none() {
            return leaf(Info::text_plain(), part);
        }
        let boundary = boundary.unwrap_or_default();
        let ranges = split_multipart(raw, &body, &boundary, MAX_PARTS);
        if ranges.is_empty() {
            return leaf(Info::text_plain(), part);
        }
        let digest = info.subtype == "digest";
        let mut kids = Vec::with_capacity(ranges.len().min(64));
        for r in ranges {
            if c.parts >= MAX_PARTS {
                break;
            }
            kids.push(build(raw, r, depth + 1, digest, c));
        }
        return Node { part, info, kind: Kind::Multi(kids), lines: 0 };
    }
    if info.mtype == "message" && (info.subtype == "rfc822" || info.subtype == "global") {
        if too_deep {
            info = Info::text_plain();
            return leaf(info, part);
        }
        let inner = build(raw, body.clone(), depth + 1, false, c);
        let lines = count_lines(body_bytes);
        return Node { part, info, kind: Kind::Msg(Box::new(inner)), lines };
    }
    leaf(info, part)
}

// ---------------------------------------------------------------------------- output

fn write_params(out: &mut Vec<u8>, params: &Params) {
    if params.is_empty() {
        out.extend_from_slice(b"NIL");
        return;
    }
    out.push(b'(');
    for (i, (n, v)) in params.iter().enumerate() {
        if i > 0 {
            out.push(b' ');
        }
        wire::text(out, n.to_ascii_uppercase().as_bytes());
        out.push(b' ');
        wire::text(out, v);
    }
    out.push(b')');
}

fn write_ext(out: &mut Vec<u8>, info: &Info, with_md5: bool) {
    if with_md5 {
        out.push(b' ');
        wire::ntext(out, info.md5.as_deref());
    }
    out.push(b' ');
    match &info.disposition {
        Some((kind, params)) => {
            out.push(b'(');
            wire::text(out, kind.to_ascii_uppercase().as_bytes());
            out.push(b' ');
            write_params(out, params);
            out.push(b')');
        }
        None => out.extend_from_slice(b"NIL"),
    }
    out.push(b' ');
    match &info.language {
        Some(l) => {
            out.push(b'(');
            let tags: Vec<&[u8]> = l.split(|b| *b == b',').map(headers::trim).filter(|t| !t.is_empty()).collect();
            for (i, t) in tags.iter().enumerate() {
                if i > 0 {
                    out.push(b' ');
                }
                wire::text(out, t);
            }
            out.push(b')');
        }
        None => out.extend_from_slice(b"NIL"),
    }
    out.push(b' ');
    wire::ntext(out, info.location.as_deref());
}

fn write_node(out: &mut Vec<u8>, raw: &[u8], n: &Node, ext: bool) {
    out.push(b'(');
    match &n.kind {
        Kind::Multi(kids) => {
            for k in kids {
                write_node(out, raw, k, ext);
            }
            out.push(b' ');
            wire::text(out, n.info.subtype.to_ascii_uppercase().as_bytes());
            if ext {
                out.push(b' ');
                write_params(out, &n.info.params);
                write_ext(out, &n.info, false);
            }
        }
        Kind::Leaf | Kind::Msg(_) => {
            wire::text(out, n.info.mtype.to_ascii_uppercase().as_bytes());
            out.push(b' ');
            wire::text(out, n.info.subtype.to_ascii_uppercase().as_bytes());
            out.push(b' ');
            write_params(out, &n.info.params);
            out.push(b' ');
            wire::ntext(out, n.info.id.as_deref());
            out.push(b' ');
            wire::ntext(out, n.info.desc.as_deref());
            out.push(b' ');
            wire::text(out, n.info.enc.as_bytes());
            out.extend_from_slice(format!(" {}", n.part.body.len()).as_bytes());
            match &n.kind {
                Kind::Msg(inner) => {
                    out.push(b' ');
                    write_envelope_of(out, raw, inner);
                    out.push(b' ');
                    write_node(out, raw, inner, ext);
                    out.extend_from_slice(format!(" {}", n.lines).as_bytes());
                }
                _ if n.info.mtype == "text" => out.extend_from_slice(format!(" {}", n.lines).as_bytes()),
                _ => {}
            }
            if ext {
                write_ext(out, &n.info, true);
            }
        }
    }
    out.push(b')');
}

fn write_envelope_of(out: &mut Vec<u8>, raw: &[u8], n: &Node) {
    crate::envelope::write(out, raw.get(n.part.header.clone()).unwrap_or(&[]));
}

// ---------------------------------------------------------------------------- sections

fn kids(n: &Node) -> Vec<&Node> {
    match &n.kind {
        Kind::Multi(v) => v.iter().collect(),
        Kind::Msg(inner) => match &inner.kind {
            Kind::Multi(v) => v.iter().collect(),
            _ => vec![&**inner],
        },
        Kind::Leaf => Vec::new(),
    }
}

impl Mime {
    pub fn part_count(&self) -> usize {
        self.parts
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    /// `BODYSTRUCTURE` (`extensible`) or `BODY` (`!extensible`) value, parenthesised.
    pub fn body_structure(&self, raw: &[u8], extensible: bool) -> Vec<u8> {
        let mut out = Vec::new();
        write_node(&mut out, raw, &self.root, extensible);
        out
    }

    /// The ENVELOPE value of the message, parenthesised.
    pub fn envelope(&self, raw: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        write_envelope_of(&mut out, raw, &self.root);
        out
    }

    /// Where the whole message's header block is (HEADER, RFC822.HEADER).
    fn top_level(&self) -> Vec<&Node> {
        match &self.root.kind {
            Kind::Multi(v) => v.iter().collect(),
            _ => vec![&self.root],
        }
    }

    fn resolve(&self, path: &[u32]) -> Option<&Node> {
        let mut level = self.top_level();
        let mut found: Option<&Node> = None;
        for num in path {
            let idx = usize::try_from(*num).ok()?.checked_sub(1)?;
            let n = *level.get(idx)?;
            found = Some(n);
            level = kids(n);
        }
        found
    }

    /// The message a header/text section refers to: the whole message, or the embedded message of
    /// a message/rfc822 part.
    fn message_node(&self, part: Option<&[u32]>) -> Option<&Node> {
        match part {
            None => Some(&self.root),
            Some(p) => match &self.resolve(p)?.kind {
                Kind::Msg(inner) => Some(inner),
                _ => None,
            },
        }
    }

    /// The bytes of `BODY[section]`; `None` when the section names a part that does not exist.
    pub fn section(&self, raw: &[u8], section: &Section) -> Option<Vec<u8>> {
        let slice = |r: Range<usize>| raw.get(r).map(<[u8]>::to_vec);
        match section {
            Section::Part(p) => slice(self.resolve(p)?.part.body.clone()),
            Section::Mime(p) => {
                let n = self.resolve(p)?;
                slice(n.part.header.start..n.part.body.start)
            }
            Section::Header(p) => {
                let n = self.message_node(p.as_deref())?;
                slice(n.part.header.start..n.part.body.start)
            }
            Section::Text(p) => slice(self.message_node(p.as_deref())?.part.body.clone()),
            Section::HeaderFields(p, names) | Section::HeaderFieldsNot(p, names) => {
                let not = matches!(section, Section::HeaderFieldsNot(..));
                let n = self.message_node(p.as_deref())?;
                let block = raw.get(n.part.header.clone())?;
                let wanted: Vec<Vec<u8>> = names.iter().map(|a| a.to_ascii_lowercase()).collect();
                let mut out = Vec::new();
                for f in headers::fields(block) {
                    let hit = wanted.iter().any(|w| f.name.eq_ignore_ascii_case(w));
                    if hit != not {
                        let chunk = block.get(f.line.clone()).unwrap_or(&[]);
                        out.extend_from_slice(chunk);
                        if !chunk.ends_with(b"\n") {
                            out.extend_from_slice(b"\r\n");
                        }
                    }
                }
                out.extend_from_slice(b"\r\n");
                Some(out)
            }
        }
    }
}

/// A text leaf of a message, for body search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextLeaf {
    pub body: Range<usize>,
    /// Upper-case Content-Transfer-Encoding (`7BIT`, `BASE64`, `QUOTED-PRINTABLE`, ...).
    pub encoding: String,
    /// Lower-case charset parameter.
    pub charset: Option<String>,
}

fn collect_text(n: &Node, out: &mut Vec<TextLeaf>) {
    match &n.kind {
        Kind::Multi(v) => v.iter().for_each(|k| collect_text(k, out)),
        Kind::Msg(inner) => collect_text(inner, out),
        Kind::Leaf => {
            if n.info.mtype == "text" && out.len() < MAX_PARTS {
                let charset = n
                    .info
                    .params
                    .iter()
                    .find(|(k, _)| k == "charset")
                    .map(|(_, v)| String::from_utf8_lossy(v).to_ascii_lowercase());
                out.push(TextLeaf { body: n.part.body.clone(), encoding: n.info.enc.clone(), charset });
            }
        }
    }
}

impl Mime {
    /// The header block of the message (without the blank line).
    pub fn root_header(&self) -> Range<usize> {
        self.root.part.header.clone()
    }

    /// The text leaves of the message in order, embedded messages included.
    pub fn text_leaves(&self) -> Vec<TextLeaf> {
        let mut out = Vec::new();
        collect_text(&self.root, &mut out);
        out
    }
}
