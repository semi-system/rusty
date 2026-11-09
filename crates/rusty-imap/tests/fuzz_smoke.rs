//! Stable-toolchain stand-ins for fuzz/fuzz_targets/fuzz_imap_*.rs: the same entry points and
//! invariants (`rusty_imap::fuzz_checks`), driven by the committed seeds and by proptest. Run by
//! scripts/ci.sh when cargo-fuzz/nightly are missing.

use proptest::prelude::*;
use rusty_imap::fuzz_checks as fz;

type Target = (&'static str, fn(&[u8]));

const TARGETS: [Target; 5] = [
    ("fuzz_imap_command", fz::command),
    ("fuzz_imap_structure", fz::structure),
    ("fuzz_imap_search_eval", fz::search),
    ("fuzz_imap_seqset", fz::seqset),
    ("fuzz_imap_utf7", fz::utf7_names),
];

// backend-neutral: parsers only, no database involved
#[test]
fn every_committed_seed_passes_its_target() {
    let mut seen = 0;
    for (name, f) in TARGETS {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds").join(name);
        let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        for e in entries {
            let p = e.unwrap().path();
            f(&std::fs::read(&p).unwrap());
            seen += 1;
        }
    }
    assert!(seen >= 15, "seeds went missing ({seen})");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(800))]

    // backend-neutral: parsers only, no database involved
    #[test]
    fn command_framing_never_panics_or_overgrows(data in proptest::collection::vec(any::<u8>(), 0..1500)) {
        fz::command(&data);
    }

    // backend-neutral: parsers only, no database involved
    #[test]
    fn command_framing_on_command_shaped_text(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("a LOGIN {5+}\r\nalice {6}\r\n".to_owned()),
                Just("b APPEND INBOX {20+}\r\n".to_owned()),
                Just("c NOOP\r\n".to_owned()),
                Just("{999999999}\r\n".to_owned()),
                "[ -~]{0,40}\r\n",
                "[ -~]{0,20}",
            ],
            0..30,
        ),
        chunk in 0u8..255,
    ) {
        let mut data = vec![chunk];
        data.extend(parts.concat().into_bytes());
        fz::command(&data);
    }

    // backend-neutral: parsers only, no database involved
    #[test]
    fn mime_structure_never_panics_and_decodes(data in proptest::collection::vec(any::<u8>(), 0..1200)) {
        fz::structure(&data);
    }

    // backend-neutral: parsers only, no database involved
    #[test]
    fn mime_structure_of_mime_shaped_text(
        parts in proptest::collection::vec(
            prop_oneof![
                Just("Content-Type: multipart/mixed; boundary=b\r\n\r\n".to_owned()),
                Just("--b\r\n".to_owned()),
                Just("--b--\r\n".to_owned()),
                Just("Content-Type: message/rfc822\r\n\r\n".to_owned()),
                Just("Content-Type: text/plain; charset=\"x\"; name=a\r\n\r\n".to_owned()),
                Just("Content-Disposition: attachment; filename*=utf-8''%41\r\n".to_owned()),
                Just("From: a@b, \"c\" <d@e>, g: h@i;\r\nSubject: =?utf-8?b?AAAA?=\r\n".to_owned()),
                "[ -~]{0,30}\r\n",
            ],
            0..40,
        ),
    ) {
        fz::structure(parts.concat().as_bytes());
    }

    // backend-neutral: parsers only, no database involved
    #[test]
    fn search_evaluation_is_consistent(q in "[ -~]{0,50}", msg in proptest::collection::vec(any::<u8>(), 0..300)) {
        let mut data = q.into_bytes();
        data.push(0);
        data.extend(msg);
        fz::search(&data);
    }

    // backend-neutral: parsers only, no database involved
    #[test]
    fn search_on_keyword_shaped_queries(
        keys in proptest::collection::vec(
            prop_oneof![
                Just("SEEN"), Just("UNSEEN"), Just("FROM x"), Just("SUBJECT y"), Just("BODY z"), Just("TEXT t"),
                Just("OR SEEN FROM q"), Just("NOT SEEN"), Just("SENTSINCE 1-Jan-2026"), Just("LARGER 5"), Just("1:*"), Just("UID 1:9"),
            ],
            0..8,
        ),
        msg in "[ -~\r\n]{0,200}",
    ) {
        let mut data = keys.join(" ").into_bytes();
        data.push(0);
        data.extend(msg.into_bytes());
        fz::search(&data);
    }

    // backend-neutral: parsers only, no database involved
    #[test]
    fn sequence_sets_select_sorted_unique_in_range(s in "[0-9:,*]{0,40}") {
        fz::seqset(s.as_bytes());
    }

    // backend-neutral: parsers only, no database involved
    #[test]
    fn utf7_round_trips(s in "\\PC{0,30}", wire in "[ -~]{0,30}") {
        fz::utf7_names(s.as_bytes());
        fz::utf7_names(wire.as_bytes());
    }
}
