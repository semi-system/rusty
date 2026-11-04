//! Exhaustive check of the allow list over the four Latin ranges.

use rusty_directory::{allowed_local_char, parse_address};
use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_normalization::UnicodeNormalization;

/// Number of code points accepted by `allowed_local_char` in the ranges below, with the
/// Unicode tables of unicode-general-category 1.x / unicode-normalization 0.1 (unicode-general-category 1.1.0;
/// the Latin blocks are stable across recent Unicode versions). If this changes after a dependency update, review why.
const EXPECTED_ALLOWED: usize = 315;

fn ranges() -> impl Iterator<Item = char> {
    [0xDFu32..=0xFF, 0x100..=0x17F, 0x180..=0x24F, 0x1E00..=0x1EFF]
        .into_iter()
        .flatten()
        .filter_map(char::from_u32)
}

// backend-neutral: pure unit test over the local-part allow list, no database
#[test]
fn allow_list_is_exactly_stable_lowercase_letters_in_latin_ranges() {
    let mut allowed = 0;
    for c in ranges() {
        let expected = get_general_category(c) == GeneralCategory::LowercaseLetter
            && std::iter::once(c).nfkc().eq(std::iter::once(c))
            && c != '\u{F7}';
        assert_eq!(allowed_local_char(c), expected, "U+{:04X}", c as u32);
        if expected {
            allowed += 1;
            let s = c.to_string();
            let (local, _) = parse_address(&format!("{s}@example.com")).unwrap();
            assert_eq!(local, s, "idempotence U+{:04X}", c as u32);
        }
    }
    assert_eq!(allowed, EXPECTED_ALLOWED);
}

// backend-neutral: pure unit test over the local-part allow list, no database
#[test]
fn nine_nfkc_unstable_letters_are_rejected() {
    for cp in [0x133u32, 0x140, 0x149, 0x17F, 0x1C6, 0x1C9, 0x1CC, 0x1F3, 0x1E9A] {
        assert!(!allowed_local_char(char::from_u32(cp).unwrap()), "U+{cp:04X}");
    }
}
