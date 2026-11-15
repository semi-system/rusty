//! AWS Signature Version 4 for S3 (the `Authorization` header form). The payload hash is always signed (`x-amz-content-sha256` is the SHA-256 of the body), so a server that
//! verifies it rejects a body that was changed on the way.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = <HmacSha256 as KeyInit>::new_from_slice(key).expect("HMAC accepts a key of any length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// Hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Percent-encoding of S3's canonical form: unreserved characters stay, everything else is `%XX` in upper case; `/` stays only when `keep_slash`.
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// What the signature covers. `headers` are `(lower-case name, value)`, sorted by name, values trimmed; they must be the headers that are sent.
pub struct Request<'a> {
    pub method: &'a str,
    /// Already percent-encoded path (`/bucket/ab/cd/key`).
    pub canonical_uri: &'a str,
    /// Already percent-encoded and sorted `name=value&...`, or empty.
    pub canonical_query: &'a str,
    pub headers: &'a [(String, String)],
    pub payload_sha256: &'a str,
    /// `20130524T000000Z`.
    pub amz_date: &'a str,
    pub region: &'a str,
    pub access_key: &'a str,
    pub secret_key: &'a str,
}

/// The `Authorization` header value.
pub fn authorization(r: &Request<'_>) -> String {
    let day = &r.amz_date[..8];
    let signed_headers = r.headers.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(";");
    let canonical_headers: String = r.headers.iter().map(|(n, v)| format!("{n}:{v}\n")).collect();
    let canonical_request = format!("{}\n{}\n{}\n{}\n{}\n{}", r.method, r.canonical_uri, r.canonical_query, canonical_headers, signed_headers, r.payload_sha256);
    let scope = format!("{day}/{}/s3/aws4_request", r.region);
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{}\n{scope}\n{}", r.amz_date, sha256_hex(canonical_request.as_bytes()));
    let k_date = hmac(format!("AWS4{}", r.secret_key).as_bytes(), day.as_bytes());
    let k_region = hmac(&k_date, r.region.as_bytes());
    let k_service = hmac(&k_region, b"s3");
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex::encode(hmac(&k_signing, string_to_sign.as_bytes()));
    format!("AWS4-HMAC-SHA256 Credential={}/{scope},SignedHeaders={signed_headers},Signature={signature}", r.access_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// "Example 1: GET Object" of the AWS Signature Version 4 documentation for S3.
    #[test]
    fn the_published_get_object_vector_signs_to_the_published_signature() {
        let headers = vec![
            ("host".to_owned(), "examplebucket.s3.amazonaws.com".to_owned()),
            ("range".to_owned(), "bytes=0-9".to_owned()),
            ("x-amz-content-sha256".to_owned(), EMPTY.to_owned()),
            ("x-amz-date".to_owned(), "20130524T000000Z".to_owned()),
        ];
        let a = authorization(&Request {
            method: "GET",
            canonical_uri: "/test.txt",
            canonical_query: "",
            headers: &headers,
            payload_sha256: EMPTY,
            amz_date: "20130524T000000Z",
            region: "us-east-1",
            access_key: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        });
        assert_eq!(
            a,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,\
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn a_changed_input_changes_the_signature_and_the_encoding_is_canonical() {
        assert_eq!(sha256_hex(b""), EMPTY);
        assert_eq!(uri_encode("a b/c~d", true), "a%20b/c~d");
        assert_eq!(uri_encode("a b/c", false), "a%20b%2Fc");
        assert_eq!(uri_encode("é$", true), "%C3%A9%24");
        let base = |secret: &'static str, uri: &'static str| {
            let h = vec![("host".to_owned(), "h".to_owned())];
            authorization(&Request { method: "GET", canonical_uri: uri, canonical_query: "", headers: &h, payload_sha256: EMPTY, amz_date: "20130524T000000Z", region: "r", access_key: "k", secret_key: secret })
        };
        assert_ne!(base("s1", "/a"), base("s2", "/a"));
        assert_ne!(base("s1", "/a"), base("s1", "/b"));
        assert_eq!(base("s1", "/a"), base("s1", "/a"));
    }
}
