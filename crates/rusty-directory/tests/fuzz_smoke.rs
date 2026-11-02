//! Stable-toolchain stand-in for fuzz/fuzz_targets/parse_address.rs: same entry points and
//! invariants, driven by proptest. Run by scripts/ci.sh when cargo-fuzz/nightly are missing.

use proptest::prelude::*;
use unicode_general_category::{get_general_category, GeneralCategory as G};

fn check(s: &str) {
    if let Ok((local, domain)) = rusty_directory::parse_address(s) {
        for c in local.chars() {
            assert!(!c.is_whitespace() && !c.is_control(), "{c:?}");
            assert!(
                !matches!(
                    get_general_category(c),
                    G::Format | G::PrivateUse | G::Unassigned | G::Control
                ),
                "{c:?}"
            );
        }
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

    #[test]
    fn arbitrary_strings_do_not_panic(s in proptest::collection::vec(spicy_char(), 0..40)) {
        check(&s.into_iter().collect::<String>());
    }

    #[test]
    fn address_shaped_strings(l in proptest::collection::vec(spicy_char(), 0..20),
                              d in "[a-z0-9.-]{1,20}") {
        check(&format!("{}@{d}.com", l.into_iter().collect::<String>()));
    }
}
