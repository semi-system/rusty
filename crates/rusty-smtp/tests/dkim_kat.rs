//! Known-answer tests for DKIM verification: the RFC 8463 appendix A example (both signatures), and
//! signatures made by an INDEPENDENT implementation
//! (OpenSSL 3.5 plus shell canonicalisation, relaxed/relaxed, see the recipe below), so that the verifier is
//! not only checked against our own signer. RSA PKCS #1 v1.5 and Ed25519 are deterministic, so a fixed key
//! and message give one fixed signature that every correct implementation reproduces.
//!
//! Recipe (RFC 6376 section 3.7 relaxed/relaxed): body hash = base64(SHA-256(body)); the signed data is the
//! relaxed-canonicalised header fields listed in h= (lower-case name, ":", value, CRLF), followed by the
//! DKIM-Signature field with an empty b= (no trailing CRLF). rsa-sha256: openssl dgst -sha256 -sign; 
//! ed25519-sha256 (RFC 8463): Ed25519 over the SHA-256 of that data (openssl pkeyutl -sign -rawin).

use rusty_smtp::authres::AuthInput;
use rusty_smtp::resolve::StaticDns;
use rusty_smtp::stack::DnsStack;

const BODY: &str = "Hi.\r\n\r\nWe lost the game. Are you hungry yet?\r\n\r\nJoe.\r\n";
const HEADERS: &str = "From: Joe SixPack <joe@football.example>\r\nTo: Suzie Q <suzie@shopping.example>\r\nSubject: Is dinner ready?\r\nDate: Fri, 11 Jul 2003 21:00:37 -0700 (PDT)\r\nMessage-ID: <20030712040037.46341.5F8J@football.example.com>\r\n";
const BH: &str = "2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=";
const RSA_PUB: &str = "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAvT2hRYpCvqeGYey0Jrfb/qAUUaLt1Y4nU5whkqlf5XsOwPmn/nGMeT6vx6W4+2LnA6iQPe9sWHIZe2nZTYB6avpiBYaQvvTFVJ0q4ZK7nU9KJvB6Y+VK5UvP7CJtxFJLx+mjkIrez2Vx7lfQ485aWyDgdEycVM9VAxfi26o/D8TIoVqJny39i2qAAxz+Q051l7HIm3mOWsdFPREWBkehSVlHNhKOwa1hGfoYqBLl+dN18Sg3vLSgHlDNpLafUJkn+LuapaqevMe3k3JzaS0LbYH6CrVhYsA7QXxlLPnIRWdkCbkHWWWPAcaqa/zjvgqCu7HAATuixvD3hnwrsbj+MQIDAQAB";
const ED_PUB: &str = "1zACgqcre65OGRc35oHTQfKkKavRIDzKZIRzPZBzpWc=";
const RSA_SIG: &str = "VxtgN9BsbHri12vuMpNeqUVcZriU8B+0FkzdArFsTjniQCh/ftgiTl/7qhNzS+YY1smnN10taBbF6fiptgy7X29NG3bnuIlkSeqEHMcrnOjEe9CXA9Pl3/KtA2c+LSBdrIHYU9kGrEsMLvDQpXbVKhb8dGTv7dS1rTnPL60JXRkAiFg25+xmtsn14K8QPnymLHXgyR7OkeGczgZsXI8hjApsAlChm281YGEW1QYKgC1NP36dowwzoWx0HWQ2pDs9lKMpFY/xzai5GdqBoDeYpC8IYvemOKUM7aFOz0e6AG+h/JHX6kCPvHXAsYe1yoIGdJ5IkAYTsxc89OQVji1gSA==";
const ED_SIG: &str = "vewvoE+RfkbI2nXAV6yalP16nzGKxqPgP2YFsKqcI14lrCi+1xSVvq7ajiYhJxPltacN9DGVOA2BI1V5OgyyDg==";

fn signed(alg: &str, selector: &str, sig: &str, body: &str) -> Vec<u8> {
    format!(
        "DKIM-Signature: v=1; a={alg}; c=relaxed/relaxed; d=example.org; s={selector}; h=from:to:subject:date:message-id; bh={BH}; b={sig}\r\n{HEADERS}\r\n{body}"
    )
    .into_bytes()
}

fn stack() -> DnsStack {
    let mut dns = StaticDns::default();
    dns.txt.insert("test._domainkey.example.org".into(), format!("v=DKIM1; k=rsa; p={RSA_PUB}"));
    dns.txt.insert("edtest._domainkey.example.org".into(), format!("v=DKIM1; k=ed25519; p={ED_PUB}"));
    DnsStack::with_static(dns, "mx.local.test").unwrap()
}

async fn dkim(msg: &[u8]) -> String {
    stack()
        .checker
        .check(&AuthInput { peer: "192.0.2.1".parse().unwrap(), helo: "h.example", mail_from: "joe@football.example", message: msg })
        .await
        .dkim
}

#[tokio::test]
async fn an_openssl_made_rsa_sha256_signature_verifies_and_breaks_on_any_change() {
    assert_eq!(dkim(&signed("rsa-sha256", "test", RSA_SIG, BODY)).await, "pass");
    assert_ne!(dkim(&signed("rsa-sha256", "test", RSA_SIG, &format!("{BODY}extra\r\n"))).await, "pass", "body change");
    let mut altered = signed("rsa-sha256", "test", RSA_SIG, BODY);
    let at = altered.windows(8).position(|w| w == b"Subject:").unwrap();
    altered[at + 9] = b'X';
    assert_ne!(dkim(&altered).await, "pass", "header change");
    let wrong_sig = RSA_SIG.replacen('V', "W", 1);
    assert_ne!(dkim(&signed("rsa-sha256", "test", &wrong_sig, BODY)).await, "pass", "signature change");
}

#[tokio::test]
async fn an_openssl_made_ed25519_sha256_signature_verifies_and_breaks_on_any_change() {
    assert_eq!(dkim(&signed("ed25519-sha256", "edtest", ED_SIG, BODY)).await, "pass");
    assert_ne!(dkim(&signed("ed25519-sha256", "edtest", ED_SIG, &format!("{BODY}extra\r\n"))).await, "pass");
    // An rsa signature does not verify under an ed25519 key record and the other way round.
    assert_ne!(dkim(&signed("rsa-sha256", "edtest", RSA_SIG, BODY)).await, "pass");
    assert_ne!(dkim(&signed("ed25519-sha256", "test", ED_SIG, BODY)).await, "pass");
}

// ------------------------------------------------------------------ RFC 8463 appendix A

/// RFC 8463 A.2 key records (the ed25519 example key and the RFC's 1024-bit RSA example key; a 1024-bit
/// key still VERIFIES, as RFC 8301 requires, although Rusty only signs with 2048 bits or more).
const RFC8463_ED_RECORD: &str = "v=DKIM1; k=ed25519; p=11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
const RFC8463_RSA_RECORD: &str = "v=DKIM1; k=rsa; p=MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDkHlOQoBTzWRiGs5V6NpP3idY6Wk08a5qhdR6wy5bdOKb2jLQiY/J16JYi0Qvx/byYzCNb3W91y3FutACDfzwQ/BC/e/8uBsCR+yz1Lxj+PL6lHvqMKrM3rG4hstT5QjvHO9PzoxZyVYLzBfO2EeC3Ip3G+2kryOTIKT+l/K4w3QIDAQAB";

/// RFC 8463 A.3, the signed message, one signature at a time (as published, folded lines included).
const RFC8463_ED_SIG: &str = "DKIM-Signature: v=1; a=ed25519-sha256; c=relaxed/relaxed;\r\n d=football.example.com; i=@football.example.com;\r\n q=dns/txt; s=brisbane; t=1528637909; h=from : to :\r\n subject : date : message-id : from : subject : date;\r\n bh=2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=;\r\n b=/gCrinpcQOoIfuHNQIbq4pgh9kyIK3AQUdt9OdqQehSwhEIug4D11Bus\r\n Fa3bT3FY5OsU7ZbnKELq+eXdp1Q1Dw==\r\n";
const RFC8463_RSA_SIG: &str = "DKIM-Signature: v=1; a=rsa-sha256; c=relaxed/relaxed;\r\n d=football.example.com; i=@football.example.com;\r\n q=dns/txt; s=test; t=1528637909; h=from : to : subject :\r\n date : message-id : from : subject : date;\r\n bh=2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=;\r\n b=F45dVWDfMbQDGHJFlXUNB2HKfbCeLRyhDXgFpEL8GwpsRe0IeIixNTe3\r\n DhCVlUrSjV4BwcVcOF6+FF3Zo9Rpo1tFOeS9mPYQTnGdaSGsgeefOsk2Jz\r\n dA+L10TeYt9BgDfQNZtKdN1WO//KgIqXP7OdEFE4LjFYNcUxZQ4FADY+8=\r\n";
const RFC8463_MESSAGE: &str = "From: Joe SixPack <joe@football.example.com>\r\nTo: Suzie Q <suzie@shopping.example.net>\r\nSubject: Is dinner ready?\r\nDate: Fri, 11 Jul 2003 21:00:37 -0700 (PDT)\r\nMessage-ID: <20030712040037.46341.5F8J@football.example.com>\r\n\r\nHi.\r\n\r\nWe lost the game.  Are you hungry yet?\r\n\r\nJoe.\r\n";

async fn rfc8463(signature: &str, message: &str) -> String {
    let mut dns = StaticDns::default();
    dns.txt.insert("brisbane._domainkey.football.example.com".into(), RFC8463_ED_RECORD.into());
    dns.txt.insert("test._domainkey.football.example.com".into(), RFC8463_RSA_RECORD.into());
    let stack = DnsStack::with_static(dns, "mx.local.test").unwrap();
    let msg = format!("{signature}{message}");
    stack
        .checker
        .check(&AuthInput { peer: "192.0.2.1".parse().unwrap(), helo: "h.example", mail_from: "joe@football.example.com", message: msg.as_bytes() })
        .await
        .dkim
}

#[tokio::test]
async fn the_rfc8463_example_signatures_verify_and_break_on_any_change() {
    for sig in [RFC8463_ED_SIG, RFC8463_RSA_SIG] {
        assert_eq!(rfc8463(sig, RFC8463_MESSAGE).await, "pass", "{sig}");
        let body = RFC8463_MESSAGE.replace("hungry", "thirsty");
        assert_ne!(rfc8463(sig, &body).await, "pass", "body change: {sig}");
        let header = RFC8463_MESSAGE.replace("Is dinner ready?", "Is dinner ready!");
        assert_ne!(rfc8463(sig, &header).await, "pass", "header change: {sig}");
        // Relaxed canonicalisation: whitespace changes inside header values and at line ends do not matter.
        let relaxed = RFC8463_MESSAGE.replace("Subject: Is dinner ready?", "Subject:  Is   dinner ready?  ");
        assert_eq!(rfc8463(sig, &relaxed).await, "pass", "relaxed header whitespace: {sig}");
    }
    // Each signature needs its own key: swapping the selectors makes both fail.
    let ed_under_rsa_key = RFC8463_ED_SIG.replace("s=brisbane", "s=test");
    assert_ne!(rfc8463(&ed_under_rsa_key, RFC8463_MESSAGE).await, "pass");
    let rsa_under_ed_key = RFC8463_RSA_SIG.replace("s=test", "s=brisbane");
    assert_ne!(rfc8463(&rsa_under_ed_key, RFC8463_MESSAGE).await, "pass");
}
