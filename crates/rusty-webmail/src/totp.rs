//! Time-based one-time passwords (RFC 6238, with RFC 4226): HMAC-SHA1, 30-second steps, six digits, the settings every authenticator app understands (docs/design-2fa.md).

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

/// Seconds of one step.
pub const STEP_SECS: u64 = 30;
/// Bytes of a secret (160 bits, the size RFC 4226 recommends).
pub const SECRET_LEN: usize = 20;
/// A code is accepted for the step it was made in and this many steps either side (a clock a little off).
pub const WINDOW: i64 = 1;

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// The code of `step` for `secret`.
pub fn code(secret: &[u8], step: u64) -> u32 {
    // HMAC accepts a key of any length: new_from_slice cannot fail.
    let Ok(mut mac) = <Hmac<Sha1> as KeyInit>::new_from_slice(secret) else { return 0 };
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest.last().copied().unwrap_or(0) & 0x0f);
    let word = digest.get(offset..offset + 4).map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) & 0x7fff_ffff);
    word % 1_000_000
}

/// The step a unix time falls in.
pub fn step_of(unix: u64) -> u64 {
    unix / STEP_SECS
}

/// Check what the person typed against the steps around `now`; the step that matched, if any, and only a step **after** `after` (the last one accepted), so a code is never accepted twice.
pub fn verify(secret: &[u8], typed: &str, now: u64, after: Option<u64>) -> Option<u64> {
    let digits: String = typed.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let typed: u32 = digits.parse().ok()?;
    let now = i64::try_from(step_of(now)).ok()?;
    let mut found = None;
    // All steps are compared (no early exit on the first match) so the time taken tells nothing.
    for d in -WINDOW..=WINDOW {
        let Ok(step) = u64::try_from(now + d) else { continue };
        if code(secret, step) == typed && after.is_none_or(|a| step > a) && found.is_none_or(|f| step > f) {
            found = Some(step);
        }
    }
    found
}

/// The secret as unpadded base32, the text an authenticator app is given.
pub fn base32(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 8 / 5 + 1);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in bytes {
        acc = (acc << 8) | u32::from(*b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(BASE32[usize::try_from((acc >> bits) & 31).unwrap_or(0)]));
        }
    }
    if bits > 0 {
        out.push(char::from(BASE32[usize::try_from((acc << (5 - bits)) & 31).unwrap_or(0)]));
    }
    out
}

/// The `otpauth://` link (the Key URI format of the authenticator apps) for an account.
pub fn uri(issuer: &str, account: &str, secret: &[u8]) -> String {
    fn enc(s: &str) -> String {
        let mut o = String::new();
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                o.push(char::from(b));
            } else {
                o.push_str(&format!("%{b:02X}"));
            }
        }
        o
    }
    format!("otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits=6&period=30", enc(issuer), enc(account), base32(secret), enc(issuer))
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 appendix B, the SHA-1 secret "12345678901234567890" and the six low digits of the eight-digit codes.
    const SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn the_codes_of_rfc_6238_come_out() {
        for (unix, expected) in [(59u64, 94_287_082u32), (1_111_111_109, 7_081_804), (1_111_111_111, 14_050_471), (1_234_567_890, 89_005_924), (2_000_000_000, 69_279_037), (20_000_000_000, 65_353_130)] {
            assert_eq!(code(SECRET, step_of(unix)), expected % 1_000_000, "at {unix}");
        }
    }

    #[test]
    fn a_code_is_accepted_one_step_either_side_never_twice_and_never_when_it_is_not_six_digits() {
        let now = 1_700_000_000u64;
        let step = step_of(now);
        let typed = format!("{:06}", code(SECRET, step));
        assert_eq!(verify(SECRET, &typed, now, None), Some(step));
        assert_eq!(verify(SECRET, &typed, now + 30, None), Some(step), "one step later");
        assert_eq!(verify(SECRET, &typed, now + 90, None), None, "three steps later");
        assert_eq!(verify(SECRET, &typed, now, Some(step)), None, "the same step again");
        assert_eq!(verify(SECRET, &typed, now, Some(step - 1)), Some(step));
        assert_eq!(verify(SECRET, &format!(" {} {} ", &typed[..3], &typed[3..]), now, None), Some(step), "spaces are ignored");
        for bad in ["", "12345", "1234567", "12a456", "０１２３４５"] {
            assert_eq!(verify(SECRET, bad, now, None), None, "{bad:?}");
        }
    }

    #[test]
    fn base32_and_the_link_are_what_the_apps_expect() {
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32(b"fooba"), "MZXW6YTB");
        assert_eq!(base32(b"f"), "MY");
        let u = uri("Rusty mail", "alice@one.test", b"fooba");
        assert_eq!(u, "otpauth://totp/Rusty%20mail:alice%40one.test?secret=MZXW6YTB&issuer=Rusty%20mail&algorithm=SHA1&digits=6&period=30");
    }
}
