#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|s: &str| {
    if let Ok((local, domain)) = rusty_directory::parse_address(s) {
        assert!(local.chars().all(rusty_directory::allowed_local_char), "{local:?}");
        assert!(!domain.chars().any(|c| c.is_whitespace() || c.is_control()));
        // Idempotent: normalizing the output again yields the same value.
        let again = rusty_directory::parse_address(&format!("{local}@{domain}"))
            .expect("normalized address must re-parse");
        assert_eq!(again, (local.clone(), domain.clone()));
        let (l2, d2) = rusty_directory::normalize_address(&local, &domain).expect("normalize_address");
        assert_eq!((l2, d2), (local, domain));
    }
});
