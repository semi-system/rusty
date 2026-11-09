//! Modified UTF-7 (RFC 3501 section 5.1.3): IMAP mailbox names on the wire. `imap-codec` hands
//! mailbox names over as the raw wire string, so the conversion to and from UTF-8 is ours.
//!
//! `&` introduces a run of UTF-16BE code units in a modified base64 (`,` instead of `/`, no
//! padding) that ends at `-`; a literal `&` is `&-`; printable ASCII (0x20..=0x7e) stands for
//! itself. Decoding is strict: anything the encoder would not produce (a non-canonical run, a
//! lone surrogate, bad base64 or padding bits) is refused, so a name has exactly one spelling.

/// Longest wire name accepted for decoding.
pub const MAX_WIRE_NAME: usize = 1024;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,";

fn is_printable(c: char) -> bool {
    ('\u{20}'..='\u{7e}').contains(&c)
}

fn flush(out: &mut String, pending: &mut Vec<u16>) {
    if pending.is_empty() {
        return;
    }
    let mut bytes = Vec::with_capacity(pending.len() * 2);
    for u in pending.drain(..) {
        bytes.extend_from_slice(&u.to_be_bytes());
    }
    out.push('&');
    for chunk in bytes.chunks(3) {
        let b0 = chunk.first().copied().unwrap_or(0);
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        let chars = chunk.len() + 1; // 1 byte -> 2 chars, 2 -> 3, 3 -> 4
        for i in 0..chars {
            let idx = ((n >> (18 - 6 * i)) & 0x3f) as usize;
            out.push(char::from(B64.get(idx).copied().unwrap_or(b'A')));
        }
    }
    out.push('-');
}

/// UTF-8 to modified UTF-7.
pub fn encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending: Vec<u16> = Vec::new();
    for c in name.chars() {
        if is_printable(c) {
            flush(&mut out, &mut pending);
            if c == '&' {
                out.push_str("&-");
            } else {
                out.push(c);
            }
        } else {
            let mut buf = [0u16; 2];
            pending.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    flush(&mut out, &mut pending);
    out
}

fn b64_value(b: u8) -> Option<u32> {
    B64.iter().position(|x| *x == b).map(|p| p as u32)
}

/// Decode one `&...-` run body (without the delimiters) to UTF-16 code units.
fn decode_run(run: &[u8]) -> Option<Vec<u16>> {
    let mut bytes: Vec<u8> = Vec::with_capacity(run.len() * 3 / 4 + 1);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for b in run {
        acc = (acc << 6) | b64_value(*b)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push(((acc >> bits) & 0xff) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    // Leftover bits must be fewer than 6 (else a whole base64 character carried no data) and zero.
    if bits >= 6 || acc != 0 || !bytes.len().is_multiple_of(2) {
        return None;
    }
    Some(bytes.chunks(2).map(|p| u16::from_be_bytes([p.first().copied().unwrap_or(0), p.get(1).copied().unwrap_or(0)])).collect())
}

/// Modified UTF-7 to UTF-8; `None` if it is not canonical modified UTF-7.
pub fn decode(wire: &str) -> Option<String> {
    let b = wire.as_bytes();
    if b.len() > MAX_WIRE_NAME || !b.iter().all(|c| (0x20..=0x7e).contains(c)) {
        return None;
    }
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        if c != b'&' {
            out.push(char::from(c));
            i += 1;
            continue;
        }
        let rest = b.get(i + 1..)?;
        let end = rest.iter().position(|x| *x == b'-')?;
        let run = rest.get(..end)?;
        if run.is_empty() {
            out.push('&');
        } else {
            let units = decode_run(run)?;
            for ch in char::decode_utf16(units) {
                out.push(ch.ok()?);
            }
        }
        i += end + 2;
    }
    // One spelling per name: whatever the encoder would not have produced is refused.
    (encode(&out) == wire).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn ascii_is_itself_and_ampersand_is_escaped() {
        assert_eq!(encode("INBOX"), "INBOX");
        assert_eq!(encode("Tom & Jerry"), "Tom &- Jerry");
        assert_eq!(decode("Tom &- Jerry").as_deref(), Some("Tom & Jerry"));
    }

    #[test]
    fn rfc_examples_round_trip() {
        // RFC 3501 section 5.1.3 examples.
        assert_eq!(encode("~peter/mail/\u{53f0}\u{5317}/\u{65e5}\u{672c}\u{8a9e}"), "~peter/mail/&U,BTFw-/&ZeVnLIqe-");
        assert_eq!(decode("~peter/mail/&U,BTFw-/&ZeVnLIqe-").as_deref(), Some("~peter/mail/\u{53f0}\u{5317}/\u{65e5}\u{672c}\u{8a9e}"));
    }

    #[test]
    fn vietnamese_and_astral_characters() {
        let n = "\u{110}\u{e3} g\u{1edf}i";
        let w = encode(n);
        assert!(w.is_ascii() && w.contains('&'), "{w}");
        assert_eq!(decode(&w).as_deref(), Some(n));
        let emoji = "x\u{1f600}y";
        assert_eq!(decode(&encode(emoji)).as_deref(), Some(emoji));
    }

    #[test]
    fn non_canonical_or_broken_input_is_refused() {
        for bad in [
            "&",                // unterminated
            "&AGE-",            // encodes plain ASCII 'a': not canonical
            "&-&-x&",           // trailing run
            "&2D0-",            // lone high surrogate
            "&3AA-",            // lone low surrogate
            "&U,BTFx-",         // non-zero padding bits
            "&U,B-",            // truncated code unit
            "\u{e9}",           // raw non-ASCII
            "a\tb",             // control character
            "&AAAAAAAAAA-&-x",  // NUL code units are not printable
        ] {
            assert_eq!(decode(bad), None, "{bad:?}");
        }
        let long = "a".repeat(MAX_WIRE_NAME + 1);
        assert_eq!(decode(&long), None);
    }

    #[test]
    fn a_name_whose_encoding_exceeds_the_cap_does_not_decode() {
        // Found by fuzz_imap_utf7: 770 NULs encode to more than MAX_WIRE_NAME characters.
        let long = "\u{110}".to_owned() + &"\u{0}".repeat(800);
        assert!(encode(&long).len() > MAX_WIRE_NAME);
        assert_eq!(decode(&encode(&long)), None);
        let ok = "\u{110}".repeat(100);
        assert_eq!(decode(&encode(&ok)).as_deref(), Some(ok.as_str()));
    }

    proptest! {
        #[test]
        fn encode_then_decode_is_the_identity(s in "\\PC{0,40}") {
            let w = encode(&s);
            prop_assert!(w.bytes().all(|b| (0x20..=0x7e).contains(&b)));
            let d = decode(&w);
            prop_assert_eq!(d.as_deref(), Some(s.as_str()));
        }

        #[test]
        fn decode_never_panics_and_accepts_only_canonical_spellings(s in "[ -~]{0,40}") {
            if let Some(d) = decode(&s) {
                prop_assert_eq!(encode(&d), s);
            }
        }
    }
}
