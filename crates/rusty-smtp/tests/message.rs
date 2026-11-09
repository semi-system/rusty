//! What is stored for an accepted message: normalised line endings, our trace headers on top,
//! forged trace headers stripped. Pure functions over bytes.

use std::net::IpAddr;

use rusty_smtp::message::{delivery_id_for, prepare_message, rfc5322_date, Trace};

fn trace<'a>() -> Trace<'a> {
    Trace {
        hostname: "mx.example.net",
        helo: "client.example.org",
        peer: "192.0.2.7".parse::<IpAddr>().unwrap(),
        tls: false,
        mail_from: "alice@example.org",
        queue_id: "0123456789ab",
        received_unix: 1_794_139_200, // 2026-11-08 12:00:00 UTC
        authenticated: false,
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

// backend-neutral: pure functions over bytes, no database
#[test]
fn dates_are_rfc5322() {
    assert_eq!(rfc5322_date(0), "Thu, 1 Jan 1970 00:00:00 +0000");
    assert_eq!(rfc5322_date(1_794_139_200), "Sun, 8 Nov 2026 12:00:00 +0000");
    assert_eq!(rfc5322_date(951_782_400), "Tue, 29 Feb 2000 00:00:00 +0000");
    assert_eq!(rfc5322_date(-1), "Wed, 31 Dec 1969 23:59:59 +0000");
}

// backend-neutral: pure functions over bytes, no database
#[test]
fn trace_headers_go_on_top_and_the_message_is_kept() {
    let orig = b"From: a@example.org\r\nSubject: hi\r\n\r\nbody line\r\n";
    let out = text(&prepare_message(orig, &trace()));
    let mut lines = out.split("\r\n");
    assert_eq!(lines.next(), Some("Return-Path: <alice@example.org>"));
    let received = lines.next().unwrap();
    assert!(received.starts_with("Received: from client.example.org ([192.0.2.7])"), "{received}");
    let rest = out.split("\r\n").collect::<Vec<_>>().join("\r\n");
    assert!(rest.contains("by mx.example.net with ESMTP id 0123456789ab;"), "{rest}");
    assert!(rest.contains("Sun, 8 Nov 2026 12:00:00 +0000"), "{rest}");
    assert!(rest.contains("Authentication-Results: mx.example.net; none\r\n"), "{rest}");
    assert!(out.ends_with("From: a@example.org\r\nSubject: hi\r\n\r\nbody line\r\n"), "{out}");
}

// backend-neutral: pure functions over bytes, no database
#[test]
fn the_received_header_names_no_recipient_and_marks_tls_and_ipv6() {
    let mut t = trace();
    t.tls = true;
    t.peer = "2001:db8::1".parse().unwrap();
    let out = text(&prepare_message(b"Subject: x\r\n\r\nb\r\n", &t));
    assert!(out.contains("([IPv6:2001:db8::1])"), "{out}");
    assert!(out.contains("with ESMTPS"), "{out}");
    assert!(!out.to_lowercase().contains(" for "), "a Received header must not name a recipient: {out}");
}

// backend-neutral: pure functions over bytes, no database
#[test]
fn a_null_sender_gives_an_empty_return_path() {
    let mut t = trace();
    t.mail_from = "";
    let out = text(&prepare_message(b"Subject: x\r\n\r\nb\r\n", &t));
    assert!(out.starts_with("Return-Path: <>\r\n"), "{out}");
}

// backend-neutral: pure functions over bytes, no database
#[test]
fn hostile_helo_and_sender_cannot_inject_headers() {
    let mut t = trace();
    t.helo = "evil\r\nX-Injected: 1 (comment";
    t.mail_from = "a@b.c>\r\nBcc: x@y.z";
    let out = text(&prepare_message(b"Subject: x\r\n\r\nb\r\n", &t));
    assert!(!out.contains("\r\nX-Injected"), "{out}");
    assert!(!out.contains("\r\nBcc:"), "{out}");
    let head = out.split("\r\n\r\n").next().unwrap();
    for l in head.split("\r\n") {
        assert!(!l.is_empty());
    }
}

// backend-neutral: pure functions over bytes, no database
#[test]
fn forged_trace_headers_are_stripped_including_folded_lines() {
    let orig = b"Authentication-Results: mx.example.net; dkim=pass\r\n\tsmtp.mailfrom=x\r\n\
        Received-SPF: pass (forged)\r\nx-rusty-queue: 1\r\nX-Rusty-Anything-Else: 2\r\n\
        authentication-results: other.example; spf=pass\r\n\
        Subject: kept\r\nFrom: a@example.org\r\n\r\nAuthentication-Results: in the body stays\r\n";
    let out = text(&prepare_message(orig, &trace()));
    // exactly one Authentication-Results: ours
    assert_eq!(out.matches("Authentication-Results:").count(), 2, "ours + the body line: {out}");
    assert!(out.contains("Authentication-Results: mx.example.net; none\r\n"));
    assert!(!out.contains("dkim=pass") && !out.contains("smtp.mailfrom") && !out.contains("spf=pass"), "{out}");
    assert!(!out.contains("Received-SPF") && !out.contains("forged"), "{out}");
    assert!(!out.to_lowercase().contains("x-rusty"), "{out}");
    assert!(out.contains("Subject: kept\r\nFrom: a@example.org\r\n\r\nAuthentication-Results: in the body stays\r\n"), "{out}");
}

// backend-neutral: pure functions over bytes, no database
#[test]
fn bare_lf_is_normalised_to_crlf_and_binary_bytes_are_kept() {
    let orig = b"Subject: a\nFrom: b@c.d\n\nline1\nline2\r\nline3\n\xc3\xa9\xff\n";
    let out = prepare_message(orig, &trace());
    let tail = b"Subject: a\r\nFrom: b@c.d\r\n\r\nline1\r\nline2\r\nline3\r\n\xc3\xa9\xff\r\n";
    assert!(out.ends_with(tail), "{:?}", text(&out));
}

// backend-neutral: pure functions over bytes, no database
#[test]
fn headers_only_and_empty_messages() {
    assert!(text(&prepare_message(b"", &trace())).ends_with("Authentication-Results: mx.example.net; none\r\n"));
    let out = text(&prepare_message(b"Subject: only headers", &trace()));
    assert!(out.contains("Subject: only headers"), "{out}");
    // a message that starts with the blank line has no header section: nothing is stripped from the body
    let out = text(&prepare_message(b"\r\nReceived-SPF: body text\r\n", &trace()));
    assert!(out.contains("\r\n\r\nReceived-SPF: body text\r\n"), "{out}");
}

// backend-neutral: pure functions over bytes, no database
#[test]
fn the_delivery_id_depends_on_sender_recipient_and_original_bytes_only() {
    let a = delivery_id_for("alice@example.org", 7, b"data");
    assert_eq!(a, delivery_id_for("alice@example.org", 7, b"data"), "a retry derives the same id");
    assert_ne!(a, delivery_id_for("alice@example.org", 8, b"data"));
    assert_ne!(a, delivery_id_for("bob@example.org", 7, b"data"));
    assert_ne!(a, delivery_id_for("alice@example.org", 7, b"data2"));
    // the null sender is a sender of its own
    assert_ne!(delivery_id_for("", 7, b"data"), delivery_id_for("alice@example.org", 7, b"data"));
}

proptest::proptest! {
    // backend-neutral: the header editor must never panic and must keep the body byte-exact
    #[test]
    fn prepare_never_panics_and_keeps_the_body(
        head in "[A-Za-z-]{1,12}: [ -~]{0,20}(\r\n[ \t][ -~]{0,10})?",
        body in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..64)
    ) {
        let mut orig = head.into_bytes();
        orig.extend_from_slice(b"\r\n\r\n");
        orig.extend_from_slice(&body);
        let out = prepare_message(&orig, &trace());
        // the body is normalised (bare LF to CRLF) but keeps every other byte
        let norm = rusty_smtp::message::normalise_crlf(&body);
        proptest::prop_assert!(out.ends_with(&norm));
    }
}
