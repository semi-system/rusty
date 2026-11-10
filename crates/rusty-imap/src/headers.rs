//! Header block parsing shared by BODYSTRUCTURE, ENVELOPE, SEARCH and BODY[HEADER.FIELDS]:
//! split a header block into fields (names, the raw bytes of each field with its folded
//! continuation lines, and the unfolded value). Bounded and total: garbage lines are skipped.

use std::ops::Range;

/// Most fields looked at in one header block.
pub const MAX_FIELDS: usize = 4096;
/// Longest unfolded value kept (longer values are cut).
pub const MAX_VALUE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// The name as written (no colon).
    pub name: Vec<u8>,
    /// All bytes of the field in the block: the first line, its continuation lines, final line
    /// break included.
    pub line: Range<usize>,
    /// The value, unfolded (line breaks removed) and trimmed of surrounding whitespace.
    pub value: Vec<u8>,
}

impl Field {
    pub fn is(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name.as_bytes())
    }
}

/// `(start, end_exclusive_of_line_break, next_line_start)` of the line at `from`.
pub(crate) fn line_at(raw: &[u8], from: usize) -> Option<(usize, usize, usize)> {
    let rest = raw.get(from..)?;
    if rest.is_empty() {
        return None;
    }
    match rest.iter().position(|b| *b == b'\n') {
        Some(i) => {
            let next = from + i + 1;
            let mut end = from + i;
            if end > from && raw.get(end - 1) == Some(&b'\r') {
                end -= 1;
            }
            Some((from, end, next))
        }
        None => Some((from, raw.len(), raw.len())),
    }
}

/// Fields of the header block `block` (the bytes of the block only, indexes are into it).
pub fn fields(block: &[u8]) -> Vec<Field> {
    let mut out: Vec<Field> = Vec::new();
    let mut pos = 0;
    while let Some((start, end, next)) = line_at(block, pos) {
        pos = next;
        let line = block.get(start..end).unwrap_or(&[]);
        if line.is_empty() {
            break; // blank line: end of headers
        }
        if line.first().is_some_and(|b| *b == b' ' || *b == b'\t') {
            // Continuation of the previous field.
            if let Some(f) = out.last_mut() {
                f.line.end = next;
                if f.value.len() < MAX_VALUE_BYTES {
                    f.value.extend_from_slice(line);
                }
            }
            continue;
        }
        let Some(colon) = line.iter().position(|b| *b == b':') else {
            continue;
        };
        let name = line.get(..colon).unwrap_or(&[]);
        if name.is_empty() || name.iter().any(|b| !(0x21..=0x7e).contains(b)) {
            continue;
        }
        if out.len() >= MAX_FIELDS {
            break;
        }
        let mut value = line.get(colon + 1..).unwrap_or(&[]).to_vec();
        value.truncate(MAX_VALUE_BYTES);
        out.push(Field { name: name.to_vec(), line: start..next, value });
    }
    for f in &mut out {
        f.value = trim(&f.value).to_vec();
    }
    out
}

pub(crate) fn trim(v: &[u8]) -> &[u8] {
    let is_ws = |b: &u8| *b == b' ' || *b == b'\t' || *b == b'\r' || *b == b'\n';
    let start = v.iter().position(|b| !is_ws(b)).unwrap_or(v.len());
    let end = v.iter().rposition(|b| !is_ws(b)).map_or(start, |i| i + 1);
    v.get(start..end).unwrap_or(&[])
}

/// The first field named `name`.
pub fn find<'a>(fields: &'a [Field], name: &str) -> Option<&'a Field> {
    fields.iter().find(|f| f.is(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_unfold_and_keep_their_raw_extent() {
        let b = b"Subject: a\r\n b\r\n\tc\r\nFrom: x@y\r\n\r\nbody";
        let f = fields(b);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].name, b"Subject");
        assert_eq!(f[0].value, b"a b\tc");
        assert_eq!(&b[f[0].line.clone()], b"Subject: a\r\n b\r\n\tc\r\n");
        assert_eq!(&b[f[1].line.clone()], b"From: x@y\r\n");
    }

    #[test]
    fn garbage_lines_and_bare_lf_are_tolerated() {
        let b = b"not a header\nX-One: 1\nbad name: 2\n: empty\nX-Two:   spaced  \n\n";
        let f = fields(b);
        let names: Vec<_> = f.iter().map(|x| String::from_utf8_lossy(&x.name).into_owned()).collect();
        assert_eq!(names, ["X-One", "X-Two"]);
        assert_eq!(f[1].value, b"spaced");
    }

    #[test]
    fn the_field_count_is_bounded() {
        let mut b = Vec::new();
        for i in 0..10_000 {
            b.extend_from_slice(format!("X-{i}: v\r\n").as_bytes());
        }
        assert_eq!(fields(&b).len(), MAX_FIELDS);
    }
}
