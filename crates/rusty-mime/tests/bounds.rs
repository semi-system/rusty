//! Resource bounds and output sanitization of `parse_envelope` (hostile input).

use std::time::{Duration, Instant};

use rusty_core::Error;
use rusty_mime::{
    header_section, parse_envelope, Envelope, MAX_ADDRESSES, MAX_ADDRESS_FIELD_BYTES,
    MAX_HEADER_BYTES, MAX_ID_BYTES, MAX_REFERENCES, MAX_SUBJECT_BYTES,
};

const LIMIT: Duration = Duration::from_secs(1);

fn p(raw: &[u8]) -> Envelope {
    parse_envelope(raw).unwrap()
}

fn timed(raw: &[u8]) -> (Envelope, Duration) {
    let t = Instant::now();
    let e = p(raw);
    (e, t.elapsed())
}

// ---------- A1: header-only parsing, O(header bytes) ----------

/// ~50 MiB made of ~1M tiny MIME parts: only the header section is looked at.
#[test]
fn huge_multipart_body_is_not_parsed() {
    let head = "From: a@x.org\r\nSubject: many parts\r\nMIME-Version: 1.0\r\n\
                Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n";
    let part = "--b\r\nContent-Type: text/plain\r\n\r\nx\r\n"; // 37 bytes
    let target = 50 * 1024 * 1024;
    let mut raw = String::with_capacity(target + 64);
    raw.push_str(head);
    let mut parts = 0usize;
    while raw.len() + part.len() < target {
        raw.push_str(part);
        parts += 1;
    }
    raw.push_str("--b--\r\n");
    assert!(parts > 1_000_000, "{parts} parts");
    let raw = raw.into_bytes();

    // Bounded-input proxy: the parser is handed the header section only.
    assert_eq!(header_section(&raw).unwrap().len(), head.len());

    let (e, took) = timed(&raw);
    assert_eq!(e.subject, "many parts");
    assert_eq!(e.from, "a@x.org");
    assert!(took < LIMIT, "parse took {took:?}");
}

/// 100,000 levels of nested multiparts.
#[test]
fn deeply_nested_multipart_is_not_parsed() {
    let head = "From: a@x.org\r\nSubject: nested\r\n\
                Content-Type: multipart/mixed; boundary=\"b0\"\r\n\r\n";
    let mut raw = String::from(head);
    for i in 0..100_000 {
        raw.push_str(&format!(
            "--b{i}\r\nContent-Type: multipart/mixed; boundary=\"b{}\"\r\n\r\n",
            i + 1
        ));
    }
    let raw = raw.into_bytes();
    assert_eq!(header_section(&raw).unwrap().len(), head.len());
    let (e, took) = timed(&raw);
    assert_eq!(e.subject, "nested");
    assert!(took < LIMIT, "parse took {took:?}");
}

/// Nested message/rfc822 parts are not descended into either.
#[test]
fn nested_rfc822_is_not_parsed() {
    let head = "From: a@x.org\r\nSubject: outer\r\nContent-Type: message/rfc822\r\n\r\n";
    let mut raw = String::from(head);
    for _ in 0..50_000 {
        raw.push_str("Subject: inner\r\nContent-Type: message/rfc822\r\n\r\n");
    }
    let raw = raw.into_bytes();
    let (e, took) = timed(&raw);
    assert_eq!(e.subject, "outer");
    assert!(took < LIMIT, "parse took {took:?}");
}

/// Worst case inside the cap: a header section of almost 256 KiB of ids and encoded words.
#[test]
fn full_size_header_section_parses_fast() {
    let mut refs = String::from("References:");
    while refs.len() < MAX_HEADER_BYTES / 2 {
        refs.push_str(" <a@b>");
    }
    let mut subj = String::from("Subject:");
    while subj.len() < MAX_HEADER_BYTES / 2 - 100 {
        subj.push_str(" =?utf-8?q?=C3=A9?=");
    }
    let raw = format!("{refs}\r\n{subj}\r\n\r\nbody");
    assert!(raw.len() < MAX_HEADER_BYTES);
    let (e, took) = timed(raw.as_bytes());
    assert_eq!(e.references.len(), MAX_REFERENCES);
    assert!(e.subject.len() <= MAX_SUBJECT_BYTES);
    assert!(took < LIMIT, "parse took {took:?}");
}

/// The header section (including its terminating empty line) must fit in the cap; beyond
/// that the message is `Invalid` (no partial header parse).
#[test]
fn header_section_cap_boundary() {
    let filler_line = |len: usize| {
        // "X-Filler: " + 'a'* + "\r\n" == len
        let mut s = String::from("X-Filler: ");
        s.push_str(&"a".repeat(len - s.len() - 2));
        s.push_str("\r\n");
        s
    };
    let prefix = "Subject: s\r\n";
    // Exactly fits: prefix + filler + "\r\n" == MAX_HEADER_BYTES.
    let fit = format!(
        "{prefix}{}\r\nbody body body",
        filler_line(MAX_HEADER_BYTES - prefix.len() - 2)
    );
    assert_eq!(header_section(fit.as_bytes()).unwrap().len(), MAX_HEADER_BYTES);
    assert_eq!(p(fit.as_bytes()).subject, "s");

    // One byte more: Invalid, even though a terminator follows later.
    let over = format!(
        "{prefix}{}\r\nbody",
        filler_line(MAX_HEADER_BYTES - prefix.len() - 1)
    );
    assert!(matches!(header_section(over.as_bytes()), Err(Error::Invalid(_))));
    assert!(matches!(parse_envelope(over.as_bytes()), Err(Error::Invalid(_))));

    // No terminator at all, 50 MiB of header-like bytes: Invalid, fast.
    let endless = "X-A: b\r\n".repeat(50 * 1024 * 1024 / 8);
    let t = Instant::now();
    assert!(matches!(parse_envelope(endless.as_bytes()), Err(Error::Invalid(_))));
    assert!(t.elapsed() < LIMIT);

    // No terminator but the whole input fits in the cap: parsed as headers only.
    assert_eq!(p(b"Subject: only headers\r\n").subject, "only headers");
}

#[test]
fn header_section_starts_with_empty_line() {
    assert_eq!(header_section(b"\r\nSubject: body").unwrap(), b"\r\n");
    assert_eq!(header_section(b"\nSubject: body").unwrap(), b"\n");
    assert_eq!(p(b"\nSubject: in body\n").subject, "");
}

// ---------- A2: sanitized, capped fields ----------

fn assert_clean(s: &str) {
    assert!(!s.chars().any(char::is_control), "control char in {s:?}");
}

#[test]
fn controls_are_removed_from_encoded_subject() {
    let e = p(b"From: a@x.org\r\nSubject: =?utf-8?q?a=00b=1Bc?=\r\n\r\nx");
    assert_eq!(e.subject, "abc");
}

#[test]
fn tabs_become_spaces_and_runs_collapse() {
    let e = p(b"Subject: =?utf-8?q?a=09=09b___c=0D=0Ad?=\r\n\r\nx");
    assert_eq!(e.subject, "a b cd");
    let e = p(b"Subject: =?utf-8?q?=C2=85x=C2=9By?=\r\n\r\nx"); // C1 controls
    assert_eq!(e.subject, "xy");
}

#[test]
fn subject_is_capped_on_a_char_boundary() {
    let long = "é".repeat(5000); // 10,000 bytes
    let e = p(format!("Subject: {long}\r\n\r\nx").as_bytes());
    assert!(e.subject.len() <= MAX_SUBJECT_BYTES);
    assert!(e.subject.len() > MAX_SUBJECT_BYTES - 4);
    assert!(e.subject.chars().all(|c| c == 'é'));
}

#[test]
fn nul_in_message_id_and_from_is_dropped() {
    let e = p(b"From: a\x00b@x.org, c@x.org\r\nMessage-ID: <a\x00b@x.org>\r\n\
                In-Reply-To: <p\x1bq@x.org>\r\nReferences: <r\x00@x> <ok@x>\r\n\r\nx");
    assert_eq!(e.message_id, None);
    assert_eq!(e.in_reply_to, None);
    assert_eq!(e.references, vec!["ok@x"]);
    assert_clean(&e.from);
    assert!(!e.from.contains("a\u{0}b"));
    assert!(e.from.ends_with("c@x.org"), "{:?}", e.from);
}

#[test]
fn space_in_id_is_dropped() {
    let e = p(b"Message-ID: <a b@x.org>\r\n\r\nx");
    assert_eq!(e.message_id, None);
}

#[test]
fn oversized_message_id_is_none() {
    let id = "a".repeat(100 * 1024);
    let e = p(format!("Message-ID: <{id}@x>\r\nIn-Reply-To: <{id}@y>\r\n\r\nx").as_bytes());
    assert_eq!(e.message_id, None);
    assert_eq!(e.in_reply_to, None);
    let ok = "a".repeat(MAX_ID_BYTES - 2);
    let e = p(format!("Message-ID: <{ok}@x>\r\n\r\nx").as_bytes());
    assert_eq!(e.message_id.as_deref().map(str::len), Some(MAX_ID_BYTES));
    let e = p(format!("Message-ID: <{ok}@xy>\r\n\r\nx").as_bytes());
    assert_eq!(e.message_id, None);
}

#[test]
fn references_keep_the_last_32() {
    let refs: Vec<String> = (0..10_000).map(|i| format!("<r{i}@x>")).collect();
    let e = p(format!("References: {}\r\n\r\nx", refs.join(" ")).as_bytes());
    assert_eq!(e.references.len(), MAX_REFERENCES);
    assert_eq!(e.references.first().unwrap(), "r9968@x");
    assert_eq!(e.references.last().unwrap(), "r9999@x");
}

#[test]
fn oversized_reference_is_dropped() {
    let big = "z".repeat(MAX_ID_BYTES + 1);
    let e = p(format!("References: <a@x> <{big}> <b@x>\r\n\r\nx").as_bytes());
    assert_eq!(e.references, vec!["a@x", "b@x"]);
}

#[test]
fn address_lists_are_capped_by_count() {
    let to: Vec<String> = (0..500).map(|i| format!("u{i}@x.org")).collect();
    let e = p(format!("To: {}\r\n\r\nx", to.join(", ")).as_bytes());
    let got: Vec<&str> = e.to.split(", ").collect();
    assert_eq!(got.len(), MAX_ADDRESSES);
    assert_eq!(got[0], "u0@x.org");
    assert_eq!(got[MAX_ADDRESSES - 1], "u99@x.org");
}

#[test]
fn address_lists_are_capped_by_bytes_keeping_whole_addresses() {
    // 40 addresses of ~200 bytes: only whole addresses that fit in 4096 bytes remain.
    let to: Vec<String> = (0..40)
        .map(|i| format!("{}{i:02}@x.org", "a".repeat(190)))
        .collect();
    let e = p(format!("To: {}\r\n\r\nx", to.join(", ")).as_bytes());
    assert!(e.to.len() <= MAX_ADDRESS_FIELD_BYTES);
    let got: Vec<&str> = e.to.split(", ").collect();
    assert!(got.len() < 40);
    for (i, a) in got.iter().enumerate() {
        assert_eq!(*a, to[i], "address {i} cut");
    }
    // A single address longer than the cap is dropped, not truncated.
    let huge = format!("{}@x.org", "b".repeat(MAX_ADDRESS_FIELD_BYTES));
    assert_eq!(p(format!("From: {huge}\r\n\r\nx").as_bytes()).from, "");
}

#[test]
fn every_field_is_clean_for_control_heavy_input() {
    let mut raw = Vec::new();
    for h in ["Subject", "From", "To", "Message-ID", "In-Reply-To", "References"] {
        raw.extend_from_slice(h.as_bytes());
        raw.extend_from_slice(b": ");
        raw.extend((0u8..32).chain([127]).flat_map(|c| [b'<', b'a', c, b'@', b'x', b'>', b' ']));
        raw.extend_from_slice(b"\r\n");
    }
    raw.extend_from_slice(b"\r\nbody");
    let e = p(&raw);
    for f in [&e.subject, &e.from, &e.to] {
        assert_clean(f);
    }
    for id in e.message_id.iter().chain(e.in_reply_to.iter()).chain(e.references.iter()) {
        assert_clean(id);
        assert!(!id.contains(char::is_whitespace));
    }
}
