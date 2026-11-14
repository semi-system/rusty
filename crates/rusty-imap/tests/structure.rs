//! MIME structure: BODYSTRUCTURE, ENVELOPE and BODY[section] extraction over fixed messages, and
//! the behaviour on hostile ones (the same properties are fuzzed by fuzz_imap_structure).

use rusty_imap::adapter::parse_command;
use rusty_imap::command::{Body, FetchItem};
use rusty_imap::structure::{parse, Mime, MAX_DEPTH, MAX_PARTS};

fn section(m: &Mime, raw: &[u8], spec: &str) -> Option<String> {
    let line = format!("a FETCH 1 (BODY.PEEK[{spec}])\r\n");
    let cmd = parse_command(line.as_bytes()).unwrap();
    let Body::Fetch { items, .. } = cmd.body else { panic!("not a fetch") };
    match items.into_iter().next().unwrap() {
        FetchItem::BodyExt { section: Some(sec), .. } => {
            m.section(raw, &sec).map(|v| String::from_utf8(v).unwrap())
        }
        other => panic!("{other:?}"),
    }
}

fn s(v: Vec<u8>) -> String {
    String::from_utf8(v).unwrap()
}

const SIMPLE: &[u8] = b"From: Alice <alice@example.com>\r\n\
To: bob@example.org, \"Carol C\" <carol@example.net>\r\n\
Subject: Hello\r\n\
Date: Fri, 06 Nov 2026 10:00:00 +0000\r\n\
Message-ID: <m1@example.com>\r\n\
\r\n\
Hi Bob\r\nSecond line\r\n";

const MIXED: &[u8] = b"MIME-Version: 1.0\r\nSubject: S\r\nContent-Type: multipart/mixed; boundary=\"outer\"\r\n\r\n\
preamble\r\n\
--outer\r\n\
Content-Type: multipart/alternative; boundary=inner\r\n\r\n\
--inner\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nplain =E2=9C=93\r\n\
--inner\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<b>html</b>\r\n\
--inner--\r\n\
--outer\r\n\
Content-Type: application/pdf; name=\"a.pdf\"\r\nContent-Disposition: attachment; filename=\"a.pdf\"\r\nContent-Transfer-Encoding: base64\r\nContent-ID: <cid1>\r\n\r\nJVBERi0=\r\n\
--outer--\r\n\
epilogue";

const FWD: &[u8] = b"Subject: Fwd\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nsee attached\r\n\
--b\r\nContent-Type: message/rfc822\r\n\r\n\
From: x@y.z\r\nSubject: inner\r\nContent-Type: text/plain\r\n\r\ninner body\r\n\
--b--\r\n";

#[test]
fn a_plain_message_has_one_text_part() {
    let m = parse(SIMPLE);
    assert_eq!(
        s(m.body_structure(SIMPLE, true)),
        r#"("TEXT" "PLAIN" ("CHARSET" "us-ascii") NIL NIL "7BIT" 21 2 NIL NIL NIL NIL)"#
    );
    assert_eq!(
        s(m.body_structure(SIMPLE, false)),
        r#"("TEXT" "PLAIN" ("CHARSET" "us-ascii") NIL NIL "7BIT" 21 2)"#
    );
    assert_eq!((m.part_count(), m.depth()), (1, 0));
}

#[test]
fn envelope_lists_the_addresses() {
    let m = parse(SIMPLE);
    assert_eq!(
        s(m.envelope(SIMPLE)),
        r#"("Fri, 06 Nov 2026 10:00:00 +0000" "Hello" (("Alice" NIL "alice" "example.com")) (("Alice" NIL "alice" "example.com")) (("Alice" NIL "alice" "example.com")) ((NIL NIL "bob" "example.org")("Carol C" NIL "carol" "example.net")) NIL NIL NIL "<m1@example.com>")"#
    );
}

#[test]
fn groups_comments_and_odd_addresses_in_the_envelope() {
    let raw = b"From: =?utf-8?q?J=C3=BCrgen?= <j@x.de>\r\nTo: Team: a@x.com, b@y.com;, c@z.com\r\nCc: undisclosed-recipients:;\r\nReply-To: legacy@old.example (Old Name)\r\nSubject: =?utf-8?q?h=C3=A9?=\r\n continued\r\n\r\n";
    let e = s(parse(raw).envelope(raw));
    assert!(e.contains(r#"(("=?utf-8?q?J=C3=BCrgen?=" NIL "j" "x.de"))"#), "{e}");
    assert!(
        e.contains(r#"((NIL NIL "Team" NIL)(NIL NIL "a" "x.com")(NIL NIL "b" "y.com")(NIL NIL NIL NIL)(NIL NIL "c" "z.com"))"#),
        "{e}"
    );
    assert!(e.contains(r#"((NIL NIL "undisclosed-recipients" NIL)(NIL NIL NIL NIL))"#), "{e}");
    assert!(e.contains(r#"(("Old Name" NIL "legacy" "old.example"))"#), "{e}");
    // The subject keeps its encoded words and is unfolded.
    assert!(e.contains(r#""=?utf-8?q?h=C3=A9?= continued""#), "{e}");
}

#[test]
fn nested_multipart_structure_with_an_attachment() {
    let m = parse(MIXED);
    assert_eq!(
        s(m.body_structure(MIXED, true)),
        concat!(
            r#"((("TEXT" "PLAIN" ("CHARSET" "utf-8") NIL NIL "QUOTED-PRINTABLE" 15 1 NIL NIL NIL NIL)"#,
            r#"("TEXT" "HTML" ("CHARSET" "utf-8") NIL NIL "7BIT" 11 1 NIL NIL NIL NIL) "ALTERNATIVE" ("BOUNDARY" "inner") NIL NIL NIL)"#,
            r#"("APPLICATION" "PDF" ("NAME" "a.pdf") "<cid1>" NIL "BASE64" 8 NIL ("ATTACHMENT" ("FILENAME" "a.pdf")) NIL NIL) "#,
            r#""MIXED" ("BOUNDARY" "outer") NIL NIL NIL)"#
        )
    );
    assert_eq!(m.part_count(), 5);
    assert_eq!(m.depth(), 2);
}

#[test]
fn sections_of_a_nested_multipart() {
    let m = parse(MIXED);
    let get = |spec: &str| section(&m, MIXED, spec);
    assert_eq!(get("1.1").as_deref(), Some("plain =E2=9C=93"));
    assert_eq!(get("1.2").as_deref(), Some("<b>html</b>"));
    assert_eq!(get("2").as_deref(), Some("JVBERi0="));
    assert_eq!(get("1.1.MIME").as_deref(), Some("Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n"));
    assert_eq!(get("2.MIME").unwrap().lines().next(), Some("Content-Type: application/pdf; name=\"a.pdf\""));
    assert_eq!(get("1.3"), None, "no such part");
    assert_eq!(get("3"), None);
    assert_eq!(get("HEADER.FIELDS (subject)").as_deref(), Some("Subject: S\r\n\r\n"));
    assert_eq!(get("HEADER.FIELDS.NOT (subject mime-version)").as_deref(), Some("Content-Type: multipart/mixed; boundary=\"outer\"\r\n\r\n"));
    assert!(get("HEADER").unwrap().ends_with("\r\n\r\n"));
    assert!(get("TEXT").unwrap().starts_with("preamble\r\n--outer"));
    // HEADER of a part that is not a message/rfc822 does not exist.
    assert_eq!(get("1.HEADER"), None);
}

#[test]
fn an_embedded_message_has_its_own_envelope_and_sections() {
    let m = parse(FWD);
    assert_eq!(
        s(m.body_structure(FWD, true)),
        concat!(
            r#"(("TEXT" "PLAIN" NIL NIL NIL "7BIT" 12 1 NIL NIL NIL NIL)"#,
            r#"("MESSAGE" "RFC822" NIL NIL NIL "7BIT" 67 "#,
            r#"(NIL "inner" ((NIL NIL "x" "y.z")) ((NIL NIL "x" "y.z")) ((NIL NIL "x" "y.z")) NIL NIL NIL NIL NIL) "#,
            r#"("TEXT" "PLAIN" NIL NIL NIL "7BIT" 10 1 NIL NIL NIL NIL) 5 NIL NIL NIL NIL) "#,
            r#""MIXED" ("BOUNDARY" "b") NIL NIL NIL)"#
        )
    );
    let get = |spec: &str| section(&m, FWD, spec);
    assert_eq!(get("2.HEADER").as_deref(), Some("From: x@y.z\r\nSubject: inner\r\nContent-Type: text/plain\r\n\r\n"));
    assert_eq!(get("2.TEXT").as_deref(), Some("inner body"));
    assert_eq!(get("2.1").as_deref(), Some("inner body"), "the body of a single-part embedded message is part 2.1");
    assert_eq!(get("2.1.MIME").as_deref(), Some("From: x@y.z\r\nSubject: inner\r\nContent-Type: text/plain\r\n\r\n"));
    assert_eq!(get("2.HEADER.FIELDS (subject)").as_deref(), Some("Subject: inner\r\n\r\n"));
    assert!(get("2").unwrap().starts_with("From: x@y.z"));
    assert_eq!(get("2.2"), None);
}

#[test]
fn hostile_structures_degrade_to_a_single_text_part() {
    // multipart without a boundary parameter
    let a = b"Content-Type: multipart/mixed\r\n\r\nbody";
    assert_eq!(s(parse(a).body_structure(a, false)), r#"("TEXT" "PLAIN" ("CHARSET" "us-ascii") NIL NIL "7BIT" 4 1)"#);
    // multipart with a boundary that never occurs
    let b = b"Content-Type: multipart/mixed; boundary=zzz\r\n\r\nnothing here\r\n";
    assert_eq!(parse(b).part_count(), 1);
    // header block without a blank line: no body
    let c = b"Subject: x";
    assert_eq!(s(parse(c).body_structure(c, false)), r#"("TEXT" "PLAIN" ("CHARSET" "us-ascii") NIL NIL "7BIT" 0 0)"#);
    // empty message
    assert_eq!(parse(b"").part_count(), 1);
    // unterminated multipart: the last part runs to the end
    let d = b"Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\n\r\none\r\n--b\r\n\r\ntwo";
    let m = parse(d);
    assert_eq!(m.part_count(), 3);
    assert_eq!(section(&m, d, "2").as_deref(), Some("two"));
    // bare LF line ends
    let e = b"Content-Type: multipart/mixed; boundary=b\n\n--b\n\none\n--b--\n";
    assert_eq!(parse(e).part_count(), 2);
}

#[test]
fn deep_nesting_is_cut_at_the_depth_limit() {
    let mut raw = Vec::new();
    let n = 2000;
    for i in 0..n {
        raw.extend_from_slice(format!("Content-Type: multipart/mixed; boundary=b{i}\r\n\r\n--b{i}\r\n").as_bytes());
    }
    raw.extend_from_slice(b"Content-Type: text/plain\r\n\r\ndeep\r\n");
    for i in (0..n).rev() {
        raw.extend_from_slice(format!("--b{i}--\r\n").as_bytes());
    }
    let start = std::time::Instant::now();
    let m = parse(&raw);
    assert!(m.depth() <= MAX_DEPTH, "{}", m.depth());
    let bs = m.body_structure(&raw, true);
    assert!(bs.len() < 64 * 1024);
    assert!(start.elapsed().as_secs() < 5);
}

#[test]
fn a_part_bomb_is_capped() {
    let mut raw = b"Content-Type: multipart/mixed; boundary=b\r\n\r\n".to_vec();
    for _ in 0..50_000 {
        raw.extend_from_slice(b"--b\r\n\r\nx\r\n");
    }
    raw.extend_from_slice(b"--b--\r\n");
    let m = parse(&raw);
    assert!(m.part_count() <= MAX_PARTS, "{}", m.part_count());
    assert!(m.part_count() > 100);
}

#[test]
fn section_partial_boundaries_are_the_callers_slice_of_the_section() {
    // The section is complete; partial slicing is applied by FETCH on the returned bytes.
    let m = parse(SIMPLE);
    let all = section(&m, SIMPLE, "TEXT").unwrap();
    assert_eq!(all, "Hi Bob\r\nSecond line\r\n");
}
