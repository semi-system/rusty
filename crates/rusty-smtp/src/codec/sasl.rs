//! SASL payload decoding for `AUTH PLAIN` and `AUTH LOGIN` (RFC 4616, RFC 4954): strict base64
//! and the PLAIN message. No I/O, bounded input, and no error ever carries the input (it is a
//! credential). Every failure is a fixed [`SaslError`].

/// Longest SASL response line accepted (RFC 4954 section 4 allows 12288 octets).
pub const MAX_SASL_LINE: usize = 12_288;
/// Longest account name and longest password the server looks at.
pub const MAX_IDENTITY_BYTES: usize = 320;
pub const MAX_PASSWORD_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaslError {
    /// Not strict base64 (alphabet, padding, length).
    Base64,
    /// Decoded data is not a well-formed PLAIN message or LOGIN value.
    Malformed,
    /// A value is longer than the server accepts.
    TooLong,
}

fn sextet(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Strict RFC 4648 base64: the standard alphabet, length a multiple of four, `=` only as final
/// padding, no whitespace, and no stray bits in the last quantum. The decoded size is capped.
pub fn decode_base64(text: &str, max_decoded: usize) -> Result<Vec<u8>, SaslError> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(SaslError::Base64);
    }
    if bytes.len() / 4 * 3 > max_decoded.saturating_add(2) {
        return Err(SaslError::TooLong);
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let quanta = bytes.len() / 4;
    #[allow(clippy::chunks_exact_to_as_chunks)]
    for (i, q) in bytes.chunks_exact(4).enumerate() {
        let last = i + 1 == quanta;
        let pad = q.iter().rev().take_while(|&&b| b == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return Err(SaslError::Base64);
        }
        let mut acc: u32 = 0;
        for &b in q.iter().take(4 - pad) {
            acc = (acc << 6) | u32::from(sextet(b).ok_or(SaslError::Base64)?);
        }
        acc <<= 6 * pad as u32;
        let raw = acc.to_be_bytes();
        let take = 3 - pad;
        // Bits that do not belong to the data must be zero (canonical encoding).
        if pad > 0 && raw.get(1 + take..4).is_some_and(|rest| rest.iter().any(|&b| b != 0)) {
            return Err(SaslError::Base64);
        }
        out.extend_from_slice(raw.get(1..1 + take).unwrap_or_default());
    }
    if out.len() > max_decoded {
        return Err(SaslError::TooLong);
    }
    Ok(out)
}

/// A decoded PLAIN message.
#[derive(Clone, PartialEq, Eq)]
pub struct PlainCredentials {
    pub authzid: String,
    pub authcid: String,
    pub password: String,
}

impl std::fmt::Debug for PlainCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PlainCredentials(<redacted>)")
    }
}

/// `[authzid] NUL authcid NUL passwd`: exactly two NULs, UTF-8, non-empty authcid and password,
/// no NUL inside a field.
pub fn parse_plain(raw: &[u8]) -> Result<PlainCredentials, SaslError> {
    let mut parts = raw.split(|&b| b == 0);
    let (Some(z), Some(c), Some(p), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(SaslError::Malformed);
    };
    if c.is_empty() || p.is_empty() {
        return Err(SaslError::Malformed);
    }
    if z.len() > MAX_IDENTITY_BYTES || c.len() > MAX_IDENTITY_BYTES || p.len() > MAX_PASSWORD_BYTES {
        return Err(SaslError::TooLong);
    }
    let text = |b: &[u8]| String::from_utf8(b.to_vec()).map_err(|_| SaslError::Malformed);
    Ok(PlainCredentials { authzid: text(z)?, authcid: text(c)?, password: text(p)? })
}

/// One LOGIN value (user name or password) from its base64 text: non-empty UTF-8, no NUL.
pub fn parse_login_value(text: &str, max: usize) -> Result<String, SaslError> {
    let raw = decode_base64(text, max)?;
    if raw.is_empty() || raw.contains(&0) {
        return Err(SaslError::Malformed);
    }
    String::from_utf8(raw).map_err(|_| SaslError::Malformed)
}
