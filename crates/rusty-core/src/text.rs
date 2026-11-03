//! Small text-hygiene helpers shared by the crates that sanitize hostile text.

/// Code point ranges (inclusive) that are Unicode format (Cf) characters, line/paragraph
/// separators (Zl, Zp), or one of the extras below. Verified against Python `unicodedata`
/// 16.0. Extras beyond Cf/Zl/Zp: U+061C, U+00AD, U+180E (already Cf in 16.0, listed for
/// emphasis) and the whole tag block U+E0000..=U+E007F (invisible tag characters).
pub const FORMAT_OR_LINE_SEPARATOR_RANGES: &[(u32, u32)] = &[
    (0xAD, 0xAD),
    (0x600, 0x605),
    (0x61C, 0x61C),
    (0x6DD, 0x6DD),
    (0x70F, 0x70F),
    (0x890, 0x891),
    (0x8E2, 0x8E2),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x2028, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0000, 0xE007F),
];

/// Unicode format (Cf) characters, line/paragraph separators (Zl, Zp), and the tag block:
/// none belongs in an email address or a stored error text. The standard library has no
/// general-category lookup, hence the table.
pub fn is_format_or_line_separator(c: char) -> bool {
    let c = c as u32;
    FORMAT_OR_LINE_SEPARATOR_RANGES
        .iter()
        .any(|&(lo, hi)| (lo..=hi).contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fails if the table drifts from the committed list: the ranges are recomputed by
    /// scanning every code point and must equal `FORMAT_OR_LINE_SEPARATOR_RANGES`, which in
    /// turn is checked against the independently committed `EXPECTED` below.
    #[test]
    fn table_matches_committed_expected_ranges() {
        // Generated from Python 3 `unicodedata` (Unicode 16.0): categories Cf, Zl, Zp plus
        // U+061C, U+00AD, U+180E and U+E0000..=U+E007F.
        const EXPECTED: [(u32, u32); 20] = [
            (0xAD, 0xAD),
            (0x600, 0x605),
            (0x61C, 0x61C),
            (0x6DD, 0x6DD),
            (0x70F, 0x70F),
            (0x890, 0x891),
            (0x8E2, 0x8E2),
            (0x180E, 0x180E),
            (0x200B, 0x200F),
            (0x2028, 0x202E),
            (0x2060, 0x2064),
            (0x2066, 0x206F),
            (0xFEFF, 0xFEFF),
            (0xFFF9, 0xFFFB),
            (0x110BD, 0x110BD),
            (0x110CD, 0x110CD),
            (0x13430, 0x1343F),
            (0x1BCA0, 0x1BCA3),
            (0x1D173, 0x1D17A),
            (0xE0000, 0xE007F),
        ];
        let mut got: Vec<(u32, u32)> = Vec::new();
        for cp in 0..=0x10FFFFu32 {
            let Some(c) = char::from_u32(cp) else {
                continue;
            };
            if is_format_or_line_separator(c) {
                match got.last_mut() {
                    Some(last) if last.1 + 1 == cp => last.1 = cp,
                    _ => got.push((cp, cp)),
                }
            }
        }
        assert_eq!(got, EXPECTED);
        assert_eq!(FORMAT_OR_LINE_SEPARATOR_RANGES, EXPECTED);
    }

    #[test]
    fn spot_checks() {
        for c in [
            '\u{AD}',
            '\u{61C}',
            '\u{180E}',
            '\u{200B}',
            '\u{200C}',
            '\u{202E}',
            '\u{2028}',
            '\u{2029}',
            '\u{FEFF}',
            '\u{E0000}',
            '\u{E0041}',
            '\u{E007F}',
        ] {
            assert!(is_format_or_line_separator(c), "{c:?}");
        }
        for c in [
            'a',
            ' ',
            '\n',
            '\u{0}',
            '\u{E0080}',
            '\u{85}',
            'é',
            '\u{3000}',
        ] {
            assert!(!is_format_or_line_separator(c), "{c:?}");
        }
    }
}
