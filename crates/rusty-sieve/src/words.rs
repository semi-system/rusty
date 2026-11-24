//! RFC 2047 encoded words (`=?utf-8?B?...?=`, `=?iso-8859-1?Q?...?=`) in a header value, decoded for the tests of a script (RFC 5228 section 5.7: a header test sees the text a person sees). Charsets known are UTF-8,
//! US-ASCII and the Latin ones (read as Latin-1: the code points 0 to 255 are the same, Windows-1252's extra signs are not mapped); any other charset is read as UTF-8 with the bad bytes replaced. A word that is not
//! well formed is left as it is. No panic, and the result is never longer than the value plus what the words expand to (a word of N bytes gives at most N characters of text).

fn b64_value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn base64(text: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in text.iter().filter(|c| **c != b'=') {
        acc = (acc << 6) | u32::from(b64_value(c)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).ok()?);
        }
    }
    Some(out)
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// The `Q` encoding: `_` is a space, `=XX` a byte.
fn quoted(text: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while let Some(&c) = text.get(i) {
        match c {
            b'_' => {
                out.push(b' ');
                i += 1;
            }
            b'=' => {
                out.push(hex(*text.get(i + 1)?)? * 16 + hex(*text.get(i + 2)?)?);
                i += 3;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Some(out)
}

fn to_text(bytes: &[u8], charset: &str) -> String {
    match charset.trim().to_ascii_lowercase().as_str() {
        "iso-8859-1" | "latin1" | "iso-8859-15" | "windows-1252" | "cp1252" => bytes.iter().map(|&b| char::from(b)).collect(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// `value` with its encoded words decoded; white space between two encoded words is dropped (RFC 2047 section 6.2).
pub fn decode(value: &str) -> String {
    if !value.contains("=?") {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    let mut last_was_word = false;
    while let Some(start) = rest.find("=?") {
        let before = rest.get(..start).unwrap_or("");
        let tail = rest.get(start + 2..).unwrap_or("");
        let decoded = (|| {
            let (charset, tail) = tail.split_once('?')?;
            let (enc, tail) = tail.split_once('?')?;
            let (text, after) = tail.split_once("?=")?;
            let bytes = match enc {
                "B" | "b" => base64(text.as_bytes())?,
                "Q" | "q" => quoted(text.as_bytes())?,
                _ => return None,
            };
            Some((to_text(&bytes, charset.split('*').next().unwrap_or(charset)), after))
        })();
        match decoded {
            Some((text, after)) => {
                if !(last_was_word && before.trim().is_empty()) {
                    out.push_str(before);
                }
                out.push_str(&text);
                rest = after;
                last_was_word = true;
            }
            None => {
                out.push_str(before);
                out.push_str("=?");
                rest = tail;
                last_was_word = false;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn encoded_words_are_decoded_and_the_rest_is_left_alone() {
        assert_eq!(decode("plain text"), "plain text");
        assert_eq!(decode("=?utf-8?Q?Caf=C3=A9_menu?="), "Café menu");
        assert_eq!(decode("=?UTF-8?B?Q2Fmw6k=?= and more"), "Café and more");
        assert_eq!(decode("=?iso-8859-1?Q?Caf=E9?="), "Café");
        assert_eq!(decode("Re: =?utf-8?B?w6k=?= =?utf-8?Q?=C3=A9?="), "Re: éé", "space between two words is dropped");
        assert_eq!(decode("a =?utf-8?Q?b?= c"), "a b c");
        assert_eq!(decode("=?utf-8*en?Q?hi?="), "hi", "a language tag is ignored");
    }

    #[test]
    fn a_word_that_is_not_well_formed_stays_as_written() {
        for bad in ["=?utf-8?Q?abc", "=?utf-8?X?abc?=", "=?utf-8?Q?=Z1?=", "=?utf-8?B?!!!?=", "=?", "?=", "=?utf-8?", "=?=?=?"] {
            let d = decode(bad);
            assert!(d.contains("=?") || bad == "?=", "{bad} -> {d}");
        }
        assert_eq!(decode("=?utf-8?Q?=C3?="), "\u{fffd}", "a cut multi-byte character is replaced, not a panic");
    }
}

#[cfg(test)]
mod property {
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(3000))]

        #[test]
        fn decoding_is_total_and_bounded(s in "([ a-z=?]|=[?][a-z0-9-]{0,8}[?][BbQqX][?][A-Za-z0-9+/=_]{0,16}([?]=)?){0,12}") {
            let d = super::decode(&s);
            prop_assert!(d.len() <= s.len() * 4 + 8, "{} -> {}", s.len(), d.len());
        }
    }
}
