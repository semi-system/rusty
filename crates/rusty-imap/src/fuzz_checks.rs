//! Entry points and invariants shared by the cargo-fuzz targets (fuzz/fuzz_targets/fuzz_imap_*.rs)
//! and their proptest stand-ins (tests/fuzz_smoke.rs): one body, two drivers. Each function must
//! never panic on any input and asserts the documented bounds of the layer it exercises.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use crate::adapter::{parse_command, response_decodes};
use crate::command::{Body, FetchItem};
use rusty_core::Flags;

use crate::frame::{Frame, FrameLimits, Framer};
use crate::search::{eval, needs_content, Bounds, Content, Facts, Tri};
use crate::seqset;
use crate::structure::{self, MAX_DEPTH, MAX_PARTS};
use crate::utf7;

/// Framer + command decoder over arbitrary bytes fed in arbitrary chunks (`data[0]` picks the
/// chunk size). The buffer never grows beyond the caps, and every `Command` frame is a complete
/// command for the decoder (it never reports `Incomplete` or a literal for it).
pub fn command(data: &[u8]) {
    let limits = FrameLimits { max_line: 256, max_literal: 128, max_append: 512, max_command: 600, max_line_mode: 64 };
    let mut f = Framer::new(limits);
    let (chunk, rest) = match data.split_first() {
        Some((c, r)) => (usize::from(*c % 31) + 1, r),
        None => return,
    };
    let bound = limits.max_command + limits.max_line + limits.max_append + chunk + 16;
    for piece in rest.chunks(chunk) {
        f.push(piece);
        loop {
            match f.next() {
                Frame::NeedMore => break,
                Frame::Command(c) => {
                    // Any outcome but a panic is fine: a framed command is complete for the decoder.
                    let _ = parse_command(&c);
                }
                Frame::Line(_) | Frame::SendContinuation | Frame::Reject { .. } => {}
                Frame::Fatal(_) => return,
            }
        }
        assert!(f.buffered() <= bound, "buffer grew to {}", f.buffered());
    }
}

/// MIME structure over arbitrary bytes: bounded tree, every output decodes as a FETCH response.
pub fn structure(data: &[u8]) {
    let m = structure::parse(data);
    assert!(m.part_count() >= 1 && m.part_count() <= MAX_PARTS, "{} parts", m.part_count());
    assert!(m.depth() <= MAX_DEPTH, "depth {}", m.depth());
    for ext in [true, false] {
        let bs = m.body_structure(data, ext);
        assert_decodes("BODYSTRUCTURE", &bs);
    }
    let env = m.envelope(data);
    assert_decodes("ENVELOPE", &env);
    // Every section of the first few part numbers is addressable without panicking.
    for spec in ["HEADER", "TEXT", "1", "1.1", "1.2", "2", "1.MIME", "2.MIME", "1.HEADER", "1.TEXT", "HEADER.FIELDS (subject from)", "HEADER.FIELDS.NOT (subject)", "1.1.1"] {
        let line = format!("a FETCH 1 (BODY.PEEK[{spec}])\r\n");
        let cmd = parse_command(line.as_bytes()).expect("section grammar");
        let Body::Fetch { items, .. } = cmd.body else { panic!("not a fetch") };
        for it in items {
            if let FetchItem::BodyExt { section: Some(s), .. } = it {
                if let Some(bytes) = m.section(data, &s) {
                    assert!(bytes.len() <= data.len() + 8, "section longer than the message");
                }
            }
        }
    }
}

/// `* 1 FETCH (<item> <value>)` must decode with the grammar library's response parser.
fn assert_decodes(item: &str, value: &[u8]) {
    let mut line = format!("* 1 FETCH ({item} ").into_bytes();
    line.extend_from_slice(value);
    line.extend_from_slice(b")\r\n");
    if !response_decodes(&line) {
        panic!("our output does not decode: {}", String::from_utf8_lossy(&line).chars().take(300).collect::<String>());
    }
}

/// SEARCH evaluation: `data` is `<query>\0<message>`. A decision made without the content must
/// agree with the decision made with it.
pub fn search(data: &[u8]) {
    let (q, raw) = match data.iter().position(|b| *b == 0) {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &[][..]),
    };
    let mut line = b"a SEARCH ".to_vec();
    line.extend_from_slice(q);
    line.extend_from_slice(b"\r\n");
    let Ok(cmd) = parse_command(&line) else { return };
    let Body::Search { key: criteria, .. } = cmd.body else { return };
    let kw = vec!["k".to_owned()];
    let facts = Facts { seq: 2, uid: 20, flags: Flags::SEEN, keywords: &kw, size: 100, internal_date: 1_700_000_000 };
    let b = Bounds { max_seq: 5, max_uid: 50 };
    let early = eval(&criteria, &facts, b, None);
    let full = eval(&criteria, &facts, b, Some(&Content::new(raw)));
    assert_ne!(full, Tri::Needs, "a decision with content is final");
    if early != Tri::Needs {
        assert_eq!(early, full, "the cheap decision changed once content was loaded");
    } else {
        assert!(needs_content(&criteria), "Needs without needs_content");
    }
}

/// Sequence sets: selections are ascending, unique, in range and bounded by the folder.
pub fn seqset(data: &[u8]) {
    let mut line = b"a FETCH ".to_vec();
    line.extend_from_slice(data);
    line.extend_from_slice(b" FLAGS\r\n");
    let Ok(cmd) = parse_command(&line) else { return };
    let Body::Fetch { set: sequence_set, .. } = cmd.body else { return };
    let uids: Vec<u32> = (1..=40).map(|n| n * 3).collect();
    for count in [0usize, 1, 7, 40] {
        let sel = seqset::select_by_seq(&sequence_set, count);
        assert!(sel.len() <= count && sel.windows(2).all(|w| w[0] < w[1]) && sel.iter().all(|i| *i < count));
    }
    let sel = seqset::select_by_uid(&sequence_set, &uids);
    assert!(sel.len() <= uids.len() && sel.windows(2).all(|w| w[0] < w[1]) && sel.iter().all(|i| *i < uids.len()));
    let sel = seqset::select_by_uid(&sequence_set, &[]);
    assert!(sel.is_empty());
}

/// Modified UTF-7: whatever decodes is canonical (re-encodes to the same text); whatever encodes
/// decodes back.
pub fn utf7_names(data: &[u8]) {
    if let Ok(s) = std::str::from_utf8(data) {
        if let Some(d) = utf7::decode(s) {
            assert_eq!(utf7::encode(&d), s);
        }
        let e = utf7::encode(s);
        assert!(e.bytes().all(|b| (0x20..=0x7e).contains(&b)));
        // Names longer than the decoder's cap (far above any folder name) are refused on the way back.
        if e.len() <= utf7::MAX_WIRE_NAME {
            assert_eq!(utf7::decode(&e).as_deref(), Some(s));
        }
    }
}
