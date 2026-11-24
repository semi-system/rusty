//! Entry points shared by the fuzz targets (`fuzz/fuzz_targets/fuzz_dav_*.rs`) and their stable-toolchain stand-ins (`tests/fuzz_smoke.rs`): the same invariants whoever drives them.

use crate::content::{self, Component, MAX_DEPTH};
use crate::{ical, time, vcard};

fn depth(c: &Component) -> usize {
    1 + c.children.iter().map(depth).max().unwrap_or(0)
}

/// The reader of objects is total and bounded: it never panics, a tree it accepts is within its limits, and what the two readers accept is consistent with the tree.
pub fn object(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else { return };
    if let Ok(tree) = content::parse(text) {
        assert!(depth(&tree) <= MAX_DEPTH);
        assert!(!tree.name.is_empty() && tree.name == tree.name.to_ascii_uppercase());
    }
    if let Ok(o) = ical::read(text) {
        assert!(!o.uid.is_empty() && o.uid.chars().count() <= 255);
        if let (Some(s), Some(e)) = (o.starts_at, o.ends_at) {
            assert!(e >= s, "an end before its start: {s} {e}");
        }
        assert!(o.ends_at.is_none() || o.starts_at.is_some(), "an end without a start");
    }
    if let Ok(c) = vcard::read(text) {
        assert!(!c.full_name.is_empty());
    }
}

/// Dates and durations: total, and a value that is accepted is one that the grammar allows.
pub fn moment(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else { return };
    if let Some(m) = time::parse_moment(text) {
        assert!(text.len() == 8 || text.len() == 15 || text.len() == 16, "{text:?}");
        assert!(m.secs.abs() < 400_000_000_000, "{}", m.secs);
    }
    if let Some(d) = time::parse_duration(text) {
        assert!(d.abs() <= 1000 * 366 * 86_400);
        assert!(text.contains('P'));
    }
}

fn xml_depth(e: &crate::xml::Element) -> usize {
    1 + e.elements().map(xml_depth).max().unwrap_or(0)
}

/// The reader of request bodies is total and bounded, and what it accepts is within its limits; the escape of any text reads back as the same text.
pub fn xml(data: &[u8]) {
    if let Ok(root) = crate::xml::parse(data) {
        assert!(xml_depth(&root) <= crate::xml::MAX_DEPTH);
        assert!(!root.name.is_empty());
    }
    if let Ok(text) = std::str::from_utf8(data) {
        let doc = format!("<a>{}</a>", crate::xml::escape(text));
        if let Ok(root) = crate::xml::parse(doc.as_bytes()) {
            let wanted: String = text.chars().filter(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r')).collect();
            assert_eq!(root.text(), wanted);
        }
    }
}

/// A path of the service names one resource or none; the path made from the resource names it again.
pub fn path(data: &[u8]) {
    use crate::paths::{self, Resource};
    let Ok(text) = std::str::from_utf8(data) else { return };
    let Some(res) = paths::parse(text) else { return };
    let again = match &res {
        Resource::Root => "/dav/".to_owned(),
        Resource::Principal(a) => paths::principal_href(a),
        Resource::Home(k, a) => paths::home_href(*k, a),
        Resource::Collection(k, a, c) => paths::collection_href(*k, a, c),
        Resource::Object(k, a, c, o) => paths::object_href(*k, a, c, o),
    };
    assert_eq!(paths::parse(&again), Some(res), "{again}");
}
