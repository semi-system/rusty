use rusty_core::Error;
use rusty_mime::parse_envelope;

fn fx(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
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
    let e = parse_envelope(&fx("bad_charset.eml")).unwrap();
    assert_eq!(e.subject, "Odd charset");
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
