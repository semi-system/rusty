//! Cookies: a bounded reader for the `Cookie` header and the builder of the one cookie the admin sets, the
//! session cookie. It is a `__Host-` cookie by construction (`Secure`, `Path=/`, no `Domain`), `HttpOnly` and
//! `SameSite=Strict`; a name or value that could break the header or weaken those rules cannot be built.

use std::time::Duration;

/// A `Cookie` header longer than this is not read at all.
const MAX_HEADER: usize = 4096;
/// At most this many well-formed pairs are read from one header.
const MAX_PAIRS: usize = 32;
const MAX_VALUE: usize = 256;
const HOST_PREFIX: &str = "__Host-";

/// A token character (RFC 9110): what a cookie name may be made of.
fn tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// A cookie octet (RFC 6265): printable ASCII except space, double quote, comma, semicolon and backslash.
fn cookie_octet(b: u8) -> bool {
    matches!(b, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e)
}

/// The well-formed `name=value` pairs of a `Cookie` header, in order. A malformed pair (no `=`, an empty or non-token
/// name, a quoted or otherwise invalid value) is skipped without affecting the others; an over-long header gives none.
pub fn parse_cookies(header: &str) -> Vec<(&str, &str)> {
    if header.len() > MAX_HEADER {
        return Vec::new();
    }
    let mut out = Vec::new();
    for pair in header.split(';') {
        let pair = pair.trim_start_matches([' ', '\t']);
        let Some((name, value)) = pair.split_once('=') else { continue };
        if name.is_empty() || !name.bytes().all(tchar) || !value.bytes().all(cookie_octet) {
            continue;
        }
        out.push((name, value));
        if out.len() == MAX_PAIRS {
            break;
        }
    }
    out
}

fn host_name_ok(name: &str) -> bool {
    name.strip_prefix(HOST_PREFIX).is_some_and(|rest| !rest.is_empty() && rest.bytes().all(tchar))
}

const ATTRIBUTES: &str = "Path=/; Secure; HttpOnly; SameSite=Strict";

/// The `Set-Cookie` value of a session cookie that lives `max_age`. `None` when the name is not a `__Host-` name, or the
/// value is not 1 to 256 characters of `A-Z a-z 0-9 . _ ~ -` (the alphabet of random identifiers), or the lifetime is
/// zero (use [`clear_cookie`]).
pub fn session_cookie(name: &str, value: &str, max_age: Duration) -> Option<String> {
    let secs = max_age.as_secs();
    let value_ok = !value.is_empty() && value.len() <= MAX_VALUE && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b));
    (host_name_ok(name) && value_ok && secs > 0).then(|| format!("{name}={value}; Max-Age={secs}; {ATTRIBUTES}"))
}

/// The `Set-Cookie` value that removes the cookie (same attributes, empty value, `Max-Age=0`).
pub fn clear_cookie(name: &str) -> Option<String> {
    host_name_ok(name).then(|| format!("{name}=; Max-Age=0; {ATTRIBUTES}"))
}
