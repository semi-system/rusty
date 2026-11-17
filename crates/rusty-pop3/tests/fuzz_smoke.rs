//! Stable-toolchain stand-ins for fuzz/fuzz_targets/fuzz_pop3_*.rs: the same entry points and invariants (`rusty_pop3::fuzz_checks`), driven by the committed seeds and by proptest.

use proptest::prelude::*;
use rusty_pop3::fuzz_checks as fz;

type Target = (&'static str, fn(&[u8]));

const TARGETS: [Target; 2] = [("fuzz_pop3_command", fz::command), ("fuzz_pop3_session", fz::session)];

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
    assert!(seen >= 12, "seeds went missing ({seen})");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1500))]

    #[test]
    fn the_command_parser_is_total(data in proptest::collection::vec(any::<u8>(), 0..700)) {
        fz::command(&data);
    }

    #[test]
    fn a_session_driven_by_any_bytes_keeps_its_invariants(data in proptest::collection::vec(any::<u8>(), 0..1500)) {
        fz::session(&data);
    }

    /// Lines that look like commands reach deeper than random bytes do.
    #[test]
    fn a_session_driven_by_command_like_lines_keeps_its_invariants(lines in proptest::collection::vec(
        prop_oneof![
            Just("USER alice".to_owned()), Just("PASS correct-horse".to_owned()), Just("PASS wrong-wrong".to_owned()), Just("STLS".to_owned()), Just("CAPA".to_owned()),
            Just("STAT".to_owned()), Just("LIST".to_owned()), Just("UIDL".to_owned()), Just("QUIT".to_owned()), Just("RSET".to_owned()), Just("NOOP".to_owned()),
            (0u32..6).prop_map(|n| format!("RETR {n}")), (0u32..6).prop_map(|n| format!("DELE {n}")), (0u32..6, 0u32..4).prop_map(|(a, b)| format!("TOP {a} {b}")),
            (0u32..6).prop_map(|n| format!("LIST {n}")), (0u32..6).prop_map(|n| format!("UIDL {n}")), "[ -~]{0,40}",
        ], 0..30)) {
        let text = lines.join("\r\n");
        fz::session(text.as_bytes());
    }
}
