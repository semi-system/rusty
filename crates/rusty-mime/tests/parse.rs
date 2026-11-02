use proptest::strategy::Strategy;
use rusty_core::Error;
use rusty_mime::parse_envelope;

fn fx(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[test]
fn plain_message_fields() {
    let e = parse_envelope(&fx("plain.eml")).unwrap();
    assert_eq!(e.subject, "Hello world");
    assert_eq!(e.from, "alice@example.com");
    assert_eq!(e.to, "bob@example.com, carol@example.com");
    assert_eq!(e.message_id.as_deref(), Some("abc123@example.com"));
    assert_eq!(e.date, Some(1_793_613_600));
}

#[test]
fn multipart_subject_decoded() {
    let e = parse_envelope(&fx("multipart.eml")).unwrap();
    assert_eq!(e.subject, "Chào bạn");
}

#[test]
fn missing_message_id_is_none() {
    let e = parse_envelope(&fx("no_message_id.eml")).unwrap();
    assert_eq!(e.message_id, None);
}

#[test]
fn bad_charset_still_parses() {
    // Unknown charset in a Subject encoded word plus invalid bytes in another header.
    // Defined behavior: no panic, Ok returned, other fields intact; the subject is
    // decoded as best effort and must still contain the payload "Hello".
    let e = parse_envelope(&fx("bad_charset.eml")).unwrap();
    assert!(e.subject.contains("Hello"), "subject was {:?}", e.subject);
    assert_eq!(e.from, "alice@example.com");
    assert_eq!(e.message_id.as_deref(), Some("cs@example.com"));
}

#[test]
fn crlf_parses() {
    let raw = b"From: a@x.org\r\nSubject: hi\r\n\r\nbody\r\n";
    let e = parse_envelope(raw).unwrap();
    assert_eq!(e.subject, "hi");
    assert_eq!(e.from, "a@x.org");
}

#[test]
fn empty_input_is_invalid() {
    assert!(matches!(parse_envelope(b""), Err(Error::Invalid(_))));
}

#[test]
fn no_headers_returns_defaults() {
    let e = parse_envelope(b"\x00\x01\x02").unwrap();
    assert_eq!(e.subject, "");
    assert_eq!(e.message_id, None);
}

#[test]
fn uppercase_addresses_are_lowercased() {
    let e = parse_envelope(b"From: ALICE@Example.COM\nTo: Bob@EXAMPLE.com\n\nx\n").unwrap();
    assert_eq!(e.from, "alice@example.com");
    assert_eq!(e.to, "bob@example.com");
}

#[test]
fn display_names_are_stripped() {
    let e = parse_envelope(b"From: \"Doe, John\" <John@x.org>\nTo: A B <a@x.org>\n\nx\n").unwrap();
    assert_eq!(e.from, "john@x.org");
    assert_eq!(e.to, "a@x.org");
}

#[test]
fn thread_headers() {
    let raw = b"From: a@x.org\nMessage-ID: <c@x.org>\nIn-Reply-To: <b@x.org>\nReferences: <a1@x.org> <b@x.org>\n\nx\n";
    let e = parse_envelope(raw).unwrap();
    assert_eq!(e.in_reply_to.as_deref(), Some("b@x.org"));
    assert_eq!(e.references, vec!["a1@x.org", "b@x.org"]);
}

proptest::proptest! {
    #[test]
    fn garbage_bytes_do_not_panic(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..2048)) {
        let _ = parse_envelope(&bytes);
    }
}

fn p(raw: &str) -> rusty_mime::Envelope {
    parse_envelope(raw.as_bytes()).unwrap()
}

#[test]
fn missing_date_is_none() {
    assert_eq!(p("From: a@x.org\nSubject: s\n\nb\n").date, None);
}

#[test]
fn invalid_dates_are_none() {
    assert_eq!(p("From: a@x.org\nDate: garbage\n\nb\n").date, None);
    assert_eq!(
        p("From: a@x.org\nDate: 32 Foo 2026 99:99:99 +0000\n\nb\n").date,
        None
    );
}

#[test]
fn pre_1970_date_is_negative() {
    let e = p("From: a@x.org\nDate: Wed, 31 Dec 1969 23:00:00 +0000\n\nb\n");
    assert_eq!(e.date, Some(-3600));
}

#[test]
fn extreme_date_does_not_panic() {
    let _ = p("From: a@x.org\nDate: Fri, 31 Dec 9999 23:59:59 +9959\n\nb\n");
    let _ = p("From: a@x.org\nDate: Fri, 31 Dec 9999 23:59:59 -9959\n\nb\n");
}

#[test]
fn empty_ids_are_filtered() {
    let e = p("From: a@x.org\nMessage-ID: <>\nIn-Reply-To: <>\nReferences: <> <>\n\nb\n");
    assert_eq!(e.message_id, None);
    assert_eq!(e.in_reply_to, None);
    assert!(e.references.is_empty());
}

#[test]
fn multiple_from_headers_do_not_panic() {
    let e = p("From: a@x.org\nFrom: b@x.org\n\nb\n");
    assert!(e.from == "a@x.org" || e.from == "b@x.org" || e.from == "a@x.org, b@x.org");
}

#[test]
fn group_empty_to_and_display_name_only_yield_empty() {
    assert_eq!(
        p("From: a@x.org\nTo: undisclosed-recipients:;\n\nb\n").to,
        ""
    );
    assert_eq!(p("From: a@x.org\nTo:\n\nb\n").to, "");
    assert_eq!(p("From: Just A Name\n\nb\n").from, "");
}

fn header_name() -> impl proptest::strategy::Strategy<Value = &'static str> {
    proptest::prop_oneof![
        proptest::strategy::Just("From"),
        proptest::strategy::Just("To"),
        proptest::strategy::Just("Subject"),
        proptest::strategy::Just("Date"),
        proptest::strategy::Just("Message-ID"),
        proptest::strategy::Just("In-Reply-To"),
        proptest::strategy::Just("References"),
    ]
}

proptest::proptest! {
    #[test]
    fn header_shaped_input_is_sane(
        headers in proptest::collection::vec(
            (header_name(), proptest::prop_oneof![
                "[ -~]{0,60}",
                proptest::collection::vec(proptest::prelude::any::<u8>(), 0..40)
                    .prop_map(|b| String::from_utf8_lossy(&b).into_owned()),
            ]), 0..10),
        crlf in proptest::prelude::any::<bool>(),
        body in "[ -~]{0,50}",
    ) {
        let nl = if crlf { "\r\n" } else { "\n" };
        let mut raw = String::new();
        for (n, v) in &headers {
            raw.push_str(&format!("{n}: {v}{nl}"));
        }
        raw.push_str(nl);
        raw.push_str(&body);
        if let Ok(e) = parse_envelope(raw.as_bytes()) {
            for f in [&e.from, &e.to] {
                proptest::prop_assert_eq!(f.to_lowercase(), f.clone());
                proptest::prop_assert!(!f.contains('<') && !f.contains('>'));
            }
        }
    }
}
