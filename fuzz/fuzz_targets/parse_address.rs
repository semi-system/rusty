#![no_main]
use libfuzzer_sys::fuzz_target;

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

fuzz_target!(|s: &str| {
    if let Ok((local, domain)) = rusty_directory::parse_address(s) {
        assert!(local.chars().all(rusty_directory::allowed_local_char), "{local:?}");
        assert!(oracle_local_ok(&local), "oracle: {local:?}");
        assert!(!domain.chars().any(|c| c.is_whitespace() || c.is_control()));
        // Idempotent: normalizing the output again yields the same value.
        let again = rusty_directory::parse_address(&format!("{local}@{domain}"))
            .expect("normalized address must re-parse");
        assert_eq!(again, (local.clone(), domain.clone()));
        let (l2, d2) = rusty_directory::normalize_address(&local, &domain).expect("normalize_address");
        assert_eq!((l2, d2), (local, domain));
    }
});
