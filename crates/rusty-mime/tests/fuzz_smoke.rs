//! Stable-toolchain stand-in for fuzz/fuzz_targets/parse_envelope.rs: same entry point and
//! invariants, driven by proptest. Run by scripts/ci.sh when cargo-fuzz/nightly are missing.

use proptest::prelude::*;

fn check(data: &[u8]) {
    if let Ok(env) = rusty_mime::parse_envelope(data) {
        for field in [&env.from, &env.to] {
            assert_eq!(field.to_lowercase(), *field);
            assert!(!field.contains([char::from(60), char::from(62)]));
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn arbitrary_bytes_do_not_panic(data in proptest::collection::vec(any::<u8>(), 0..2048)) {
        check(&data);
    }

    #[test]
    fn header_shaped_input_keeps_invariants(
        from in "[ -~<>\"@:;,]{0,60}",
        to in "[ -~<>\"@:;,]{0,60}",
        tail in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut raw = format!("From: {from}\r\nTo: {to}\r\n\r\n").into_bytes();
        raw.extend(tail);
        check(&raw);
    }
}
