//! Stable-toolchain stand-in for fuzz/fuzz_targets/parse_envelope.rs: same entry point and
//! invariants, driven by proptest. Run by scripts/ci.sh when cargo-fuzz/nightly are missing.
//! The oracle is written independently of the implementation (keep it in sync with the
//! fuzz target, not with `rusty_mime` internals).

use proptest::prelude::*;

const MAX_SUBJECT: usize = 2048;
const MAX_ADDR_FIELD: usize = 4096;
const MAX_ADDRS: usize = 100;
const MAX_ID: usize = 998;
const MAX_REFS: usize = 32;

fn no_controls(s: &str) {
    assert!(!s.chars().any(|c| c.is_control()), "control char in {s:?}");
}

fn check_id(id: &str) {
    no_controls(id);
    assert!(!id.is_empty() && id.len() <= MAX_ID, "id length {}", id.len());
    assert!(!id.chars().any(char::is_whitespace), "whitespace in id {id:?}");
}

fn check(data: &[u8]) {
    let Ok(env) = rusty_mime::parse_envelope(data) else {
        return;
    };
    no_controls(&env.subject);
    assert!(env.subject.len() <= MAX_SUBJECT);
    assert_eq!(env.subject.trim_matches(' '), env.subject, "untrimmed subject");
    assert!(!env.subject.contains("  "), "space run in subject");
    for field in [&env.from, &env.to] {
        no_controls(field);
        assert!(field.len() <= MAX_ADDR_FIELD);
        assert_eq!(field.to_lowercase(), *field);
        assert!(!field.contains([char::from(60), char::from(62)]));
        if !field.is_empty() {
            let addrs: Vec<&str> = field.split(", ").collect();
            assert!(addrs.len() <= MAX_ADDRS);
            for a in addrs {
                assert!(!a.is_empty() && !a.chars().any(char::is_whitespace), "{a:?}");
            }
        }
    }
    for id in env.message_id.iter().chain(env.in_reply_to.iter()) {
        check_id(id);
    }
    assert!(env.references.len() <= MAX_REFS);
    for id in &env.references {
        check_id(id);
    }
}

fn joined(n: usize, f: impl Fn(usize) -> String, sep: &str) -> String {
    (0..n).map(f).collect::<Vec<_>>().join(sep)
}

/// Header values with controls, tabs, encoded words and long runs.
fn hostile_value() -> impl Strategy<Value = String> {
    prop_oneof![
        "[ -~<>\"@:;,]{0,60}",
        "[\\x00-\\x1f\\x7f\\u{80}-\\u{9f} a-z@<>]{0,60}",
        Just("=?utf-8?q?a=00b=1Bc=09=09d?=".to_string()),
        (1usize..3000).prop_map(|n| format!("<{}@x>", "a".repeat(n))),
        (1usize..200).prop_map(|n| joined(n, |i| format!("<r{i}@x>"), " ")),
        (1usize..200).prop_map(|n| joined(n, |i| format!("u{i}{}@x.org", "b".repeat(40)), ", ")),
    ]
}

fn header_name() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("From"),
        Just("To"),
        Just("Subject"),
        Just("Message-ID"),
        Just("In-Reply-To"),
        Just("References"),
    ]
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

    #[test]
    fn hostile_headers_keep_output_invariants(
        headers in proptest::collection::vec((header_name(), hostile_value()), 0..8),
    ) {
        let mut raw = String::new();
        for (n, v) in &headers {
            raw.push_str(&format!("{n}: {v}\r\n"));
        }
        raw.push_str("\r\nbody");
        check(raw.as_bytes());
    }
}
