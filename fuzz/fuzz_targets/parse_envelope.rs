#![no_main]
use libfuzzer_sys::fuzz_target;

// Output invariants, written independently of rusty-mime internals (mirrored by the
// proptest stand-in crates/rusty-mime/tests/fuzz_smoke.rs).
const MAX_SUBJECT: usize = 2048;
const MAX_ADDR_FIELD: usize = 4096;
const MAX_ADDRS: usize = 100;
const MAX_ID: usize = 998;
const MAX_REFS: usize = 32;

fn no_controls(s: &str) {
    assert!(!s.chars().any(|c| c.is_control()), "control char in output");
}

fn check_id(id: &str) {
    no_controls(id);
    assert!(!id.is_empty() && id.len() <= MAX_ID, "id length out of range");
    assert!(!id.chars().any(char::is_whitespace), "whitespace in id");
}

fuzz_target!(|data: &[u8]| {
    let Ok(env) = rusty_mime::parse_envelope(data) else {
        return;
    };
    no_controls(&env.subject);
    assert!(env.subject.len() <= MAX_SUBJECT, "subject too long");
    assert_eq!(env.subject.trim_matches(' '), env.subject, "untrimmed subject");
    assert!(!env.subject.contains("  "), "space run in subject");
    for field in [&env.from, &env.to] {
        no_controls(field);
        assert!(field.len() <= MAX_ADDR_FIELD, "address field too long");
        assert_eq!(field.to_lowercase(), *field, "address field not lowercase");
        assert!(!field.contains([char::from(60), char::from(62)]), "angle bracket in address");
        if !field.is_empty() {
            let addrs: Vec<&str> = field.split(", ").collect();
            assert!(addrs.len() <= MAX_ADDRS, "too many addresses");
            for a in addrs {
                assert!(!a.is_empty() && !a.chars().any(char::is_whitespace), "bad address");
            }
        }
    }
    for id in env.message_id.iter().chain(env.in_reply_to.iter()) {
        check_id(id);
    }
    assert!(env.references.len() <= MAX_REFS, "too many references");
    for id in &env.references {
        check_id(id);
    }
});
