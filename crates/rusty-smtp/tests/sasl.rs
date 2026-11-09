//! The SASL codec: strict base64 and the PLAIN message. Pure functions over bytes.

use proptest::prelude::*;
use rusty_smtp::codec::{decode_base64, parse_login_value, parse_plain, SaslError};

fn encode(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    out
}

// backend-neutral: pure codec, no database
#[test]
fn known_vectors_rfc4648() {
    for (plain, enc) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
        assert_eq!(decode_base64(enc, 100).unwrap(), plain.as_bytes(), "{enc}");
    }
}

// backend-neutral: pure codec, no database
#[test]
fn strictness() {
    for bad in ["Zg", "Zg=", "Zm9", "Z===", "Zg==Zg==", "Zm=v", "Zm9v ", " Zm9v", "Zm9v\n", "Zm9!", "Zm9v====", "Zh==", "Zm9=", "=Zm9", "Zm9v-_"] {
        assert_eq!(decode_base64(bad, 100), Err(SaslError::Base64), "{bad:?}");
    }
    // the decoded size cap
    assert_eq!(decode_base64(&encode(&[1u8; 40]), 10), Err(SaslError::TooLong));
    assert!(decode_base64(&encode(&[1u8; 10]), 10).is_ok());
}

// backend-neutral: pure codec, no database
#[test]
fn plain_messages() {
    let c = parse_plain(b"\0alice\0secret").unwrap();
    assert_eq!((c.authzid.as_str(), c.authcid.as_str(), c.password.as_str()), ("", "alice", "secret"));
    let c = parse_plain(b"alice\0alice\0se\xc3\xa9").unwrap();
    assert_eq!(c.password, "se\u{e9}");
    for bad in [&b"alice\0secret"[..], b"\0\0secret", b"\0alice\0", b"\0a\0b\0c", b"", b"\0a\0\xff\xfe"] {
        assert_eq!(parse_plain(bad).unwrap_err(), SaslError::Malformed, "{bad:?}");
    }
    let long = format!("\0{}\0x", "a".repeat(400));
    assert_eq!(parse_plain(long.as_bytes()).unwrap_err(), SaslError::TooLong);
    let dbg = format!("{:?}", parse_plain(b"\0alice\0secret").unwrap());
    assert!(!dbg.contains("secret") && !dbg.contains("alice"), "{dbg}");
}

// backend-neutral: pure codec, no database
#[test]
fn login_values() {
    assert_eq!(parse_login_value("YWxpY2U=", 100).unwrap(), "alice");
    for bad in ["", "AA==", "/w==", "====", "YWxpY2U"] {
        assert!(parse_login_value(bad, 100).is_err(), "{bad:?}");
    }
}

proptest! {
    // backend-neutral: encode/decode round trip and no panic on arbitrary text
    #[test]
    fn round_trip(data in proptest::collection::vec(any::<u8>(), 0..300)) {
        prop_assert_eq!(decode_base64(&encode(&data), 1000).unwrap(), data);
    }

    // backend-neutral: arbitrary text never panics
    #[test]
    fn arbitrary_text_never_panics(s in "\\PC{0,200}") {
        let _ = decode_base64(&s, 64);
        let _ = parse_login_value(&s, 64);
    }
}
