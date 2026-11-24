//! Stable-toolchain stand-in for fuzz/fuzz_targets/fuzz_managesieve_command.rs: the same entry point and invariants (`rusty_managesieve::fuzz_checks`), driven by the committed seeds and by proptest.

use proptest::prelude::*;
use rusty_managesieve::fuzz_checks as fz;

#[test]
fn every_committed_seed_passes_its_target() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds/fuzz_managesieve_command");
    let mut seen = 0;
    for e in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        fz::command(&std::fs::read(e.unwrap().path()).unwrap());
        seen += 1;
    }
    assert!(seen >= 8, "seeds went missing ({seen})");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn the_command_parser_is_total(data in proptest::collection::vec(any::<u8>(), 0..700)) {
        fz::command(&data);
    }

    #[test]
    fn text_shaped_like_commands_keeps_the_invariants(s in "[A-Za-z]{0,12}( ([\"][ -~]{0,10}[\"]|[{][0-9]{1,3}[+][}]\r\n[ -~]{0,20}|[0-9]{1,4})){0,4}\r\n") {
        fz::command(s.as_bytes());
    }
}
