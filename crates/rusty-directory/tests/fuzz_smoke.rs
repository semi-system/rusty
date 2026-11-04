//! Stable-toolchain stand-in for fuzz/fuzz_targets/parse_address.rs: same entry points and
//! invariants, driven by proptest. Run by scripts/ci.sh when cargo-fuzz/nightly are missing.

use proptest::prelude::*;

/// Independent oracle (does not call allowed_local_char): general categories via the crate.
fn oracle_local_ok(local: &str) -> bool {
    use unicode_general_category::{get_general_category, GeneralCategory as G};
    local.chars().all(|c| {
        if c.is_whitespace() {
            return false;
        }
        if c.is_ascii() {
            return true;
        }
        let in_ranges = matches!(c as u32, 0xDF..=0xFF | 0x100..=0x17F | 0x180..=0x24F | 0x1E00..=0x1EFF);
        let bad_cat = matches!(
            get_general_category(c),
            G::NonspacingMark | G::SpacingMark | G::EnclosingMark | G::Format | G::Control
                | G::PrivateUse | G::Unassigned | G::SpaceSeparator | G::LineSeparator
                | G::ParagraphSeparator
        );
        in_ranges && !bad_cat
    })
}

fn check(s: &str) {
    if let Ok((local, domain)) = rusty_directory::parse_address(s) {
        assert!(local.chars().all(rusty_directory::allowed_local_char), "{local:?}");
        assert!(oracle_local_ok(&local), "oracle: {local:?}");
        assert!(!domain.chars().any(|c| c.is_whitespace() || c.is_control()));
        let again = rusty_directory::parse_address(&format!("{local}@{domain}")).unwrap();
        assert_eq!(again, (local.clone(), domain.clone()));
        assert_eq!(rusty_directory::normalize_address(&local, &domain).unwrap(), (local, domain));
    }
}

fn spicy_char() -> impl Strategy<Value = char> {
    prop_oneof![
        any::<char>(),
        proptest::sample::select(vec![
            char::from(64), char::from(46), char::from(32), char::from(0),
            '\u{200B}', '\u{202E}', '\u{2066}', '\u{FEFF}', '\u{E000}',
            '\u{0301}', '\u{00E9}', '\u{0130}', '\u{1E9E}', '\u{FB01}',
        ]),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20000))]

    // backend-neutral: pure parser fuzz stand-in, no database
    #[test]
    fn arbitrary_strings_do_not_panic(s in proptest::collection::vec(spicy_char(), 0..40)) {
        check(&s.into_iter().collect::<String>());
    }

    // backend-neutral: pure parser fuzz stand-in, no database
    #[test]
    fn address_shaped_strings(l in proptest::collection::vec(spicy_char(), 0..20),
                              d in "[a-z0-9.-]{1,20}") {
        check(&format!("{}@{d}.com", l.into_iter().collect::<String>()));
    }
}
