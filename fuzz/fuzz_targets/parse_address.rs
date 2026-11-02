#![no_main]
use libfuzzer_sys::fuzz_target;
use unicode_general_category::{get_general_category, GeneralCategory as G};

fuzz_target!(|s: &str| {
    if let Ok((local, domain)) = rusty_directory::parse_address(s) {
        for c in local.chars() {
            assert!(!c.is_whitespace() && !c.is_control(), "whitespace/control in local");
            assert!(
                !matches!(get_general_category(c), G::Format | G::PrivateUse | G::Unassigned | G::Control),
                "forbidden category in local: {c:?}"
            );
        }
        assert!(!domain.chars().any(|c| c.is_whitespace() || c.is_control()));
        // Idempotent: normalizing the output again yields the same value.
        let again = rusty_directory::parse_address(&format!("{local}@{domain}"))
            .expect("normalized address must re-parse");
        assert_eq!(again, (local.clone(), domain.clone()));
        let (l2, d2) = rusty_directory::normalize_address(&local, &domain).expect("normalize_address");
        assert_eq!((l2, d2), (local, domain));
    }
});
