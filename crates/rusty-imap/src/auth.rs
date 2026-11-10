//! SASL PLAIN as IMAP carries it (RFC 4616 over RFC 3501 AUTHENTICATE): strict base64 and the
//! `authzid NUL authcid NUL passwd` split. Pure parsing; the decision to accept the credentials
//! is the session's, through the injected `Authenticator`.

/// Longest credential blob (base64 text) accepted: a 1 KiB password plus two names.
pub const MAX_PLAIN_B64: usize = 4 * 1024;

/// Standard base64 with padding. Anything outside the alphabet, bad padding or non-zero padding
/// bits is refused (no leniency: this is an authentication payload).
pub fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    if !input.len().is_multiple_of(4) {
        return None;
    }
    let val = |b: u8| -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some(u32::from(b - b'A')),
            b'a'..=b'z' => Some(u32::from(b - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(b - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let chunks: Vec<&[u8]> = input.chunks(4).collect();
    for (ci, c) in chunks.iter().enumerate() {
        let last = ci + 1 == chunks.len();
        let pad = c.iter().rev().take_while(|b| **b == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut acc: u32 = 0;
        for (i, b) in c.iter().enumerate() {
            let v = if i >= 4 - pad { 0 } else { val(*b)? };
            acc = (acc << 6) | v;
        }
        let bytes = [(acc >> 16) as u8, (acc >> 8) as u8, acc as u8];
        let n = 3 - pad;
        // Padding bits must be zero.
        if pad == 1 && bytes[2] != 0 || pad == 2 && (bytes[1] != 0 || bytes[2] != 0) {
            return None;
        }
        out.extend_from_slice(bytes.get(..n)?);
    }
    Some(out)
}

/// The three fields of a PLAIN message.
#[derive(Debug, PartialEq, Eq)]
pub struct Plain {
    pub authzid: String,
    pub authcid: String,
    pub password: String,
}

/// Decode the base64 PLAIN payload. `None` for anything that is not exactly two NULs, valid
/// UTF-8, a non-empty authcid and a non-empty password.
pub fn parse_plain(b64: &[u8]) -> Option<Plain> {
    if b64.len() > MAX_PLAIN_B64 {
        return None;
    }
    parse_plain_bytes(&base64_decode(b64)?)
}

/// The decoded PLAIN message (`authzid NUL authcid NUL passwd`).
pub fn parse_plain_bytes(raw: &[u8]) -> Option<Plain> {
    if raw.len() > MAX_PLAIN_B64 {
        return None;
    }
    let mut it = raw.split(|b| *b == 0);
    let (authzid, authcid, password) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || authcid.is_empty() || password.is_empty() {
        return None;
    }
    Some(Plain {
        authzid: String::from_utf8(authzid.to_vec()).ok()?,
        authcid: String::from_utf8(authcid.to_vec()).ok()?,
        password: String::from_utf8(password.to_vec()).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_is_strict() {
        assert_eq!(base64_decode(b"").as_deref(), Some(&b""[..]));
        assert_eq!(base64_decode(b"Zg==").as_deref(), Some(&b"f"[..]));
        assert_eq!(base64_decode(b"Zm8=").as_deref(), Some(&b"fo"[..]));
        assert_eq!(base64_decode(b"Zm9v").as_deref(), Some(&b"foo"[..]));
        for bad in [&b"Zg="[..], b"Zg=a", b"=Zg=", b"Z===", b"Zg==Zg==", b"Zh==", b"Zm9=", b"Zm 9", b"Zm9\n", b"Zm9v!!!!", b"Zm-_"] {
            assert_eq!(base64_decode(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn plain_splits_into_three_fields() {
        // "\0alice\0pass"
        let p = parse_plain(b"AGFsaWNlAHBhc3M=").unwrap();
        assert_eq!(p, Plain { authzid: String::new(), authcid: "alice".into(), password: "pass".into() });
        // "alice\0alice\0pass"
        let p = parse_plain(b"YWxpY2UAYWxpY2UAcGFzcw==").unwrap();
        assert_eq!((p.authzid.as_str(), p.authcid.as_str()), ("alice", "alice"));
    }

    #[test]
    fn plain_rejects_malformed_payloads() {
        // one NUL, three NULs, empty authcid, empty password, invalid UTF-8, too long
        for b64 in ["YWxpY2UAcGFzcw==", "AGEAYgBj", "AABwYXNz", "AGFsaWNlAA==", "AGEA/w=="] {
            assert_eq!(parse_plain(b64.as_bytes()), None, "{b64}");
        }
        let long = "QUFB".repeat(MAX_PLAIN_B64);
        assert_eq!(parse_plain(long.as_bytes()), None);
    }
}
