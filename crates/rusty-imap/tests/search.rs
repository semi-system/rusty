//! SEARCH evaluation: flags, sets, sizes and dates from the snapshot facts; headers and body from
//! the message bytes; three-valued logic so content is only loaded when it matters.

use rusty_imap::adapter::parse_command;
use rusty_imap::command::Body;
use rusty_core::Flags;
use rusty_imap::search::{eval, needs_content, Bounds, Content, Facts, Tri};

const MSG: &[u8] = b"From: Alice Example <alice@example.com>\r\n\
To: Bob <bob@example.org>\r\n\
Cc: carol@example.net\r\n\
Subject: =?utf-8?q?Caf=C3=A9_menu?=\r\n\
Date: Fri, 06 Nov 2026 10:00:00 +0000\r\n\
X-Custom: Hello World\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
The quick brown fox =E2=9C=93 jumps\r\n";

const B64: &[u8] = b"From: a@b.c\r\nSubject: x\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: base64\r\n\r\nU2VjcmV0IHBocmFzZQ==\r\n";

fn facts(flags: Flags) -> Facts<'static> {
    Facts { seq: 3, uid: 30, flags, keywords: &[], size: 1000, internal_date: 1_793_000_000 } // 2026-10-26
}

fn run(q: &str, f: &Facts<'_>, raw: Option<&[u8]>) -> Tri {
    let line = format!("a SEARCH {q}\r\n");
    let cmd = parse_command(line.as_bytes()).unwrap();
    let Body::Search { key: criteria, .. } = cmd.body else { panic!() };
    let b = Bounds { max_seq: 10, max_uid: 100 };
    match raw {
        None => eval(&criteria, f, b, None),
        Some(r) => eval(&criteria, f, b, Some(&Content::new(r))),
    }
}

fn yes(q: &str, f: &Facts<'_>, raw: &[u8]) -> bool {
    run(q, f, Some(raw)) == Tri::Yes
}

#[test]
fn flags_keywords_sets_and_sizes_decide_without_content() {
    let seen = facts(Flags::SEEN | Flags::FLAGGED);
    assert_eq!(run("SEEN", &seen, None), Tri::Yes);
    assert_eq!(run("UNSEEN", &seen, None), Tri::No);
    assert_eq!(run("FLAGGED UNDELETED UNANSWERED UNDRAFT", &seen, None), Tri::Yes);
    assert_eq!(run("DELETED", &seen, None), Tri::No);
    assert_eq!(run("NEW", &seen, None), Tri::No, "nothing is \\Recent");
    assert_eq!(run("OLD", &seen, None), Tri::Yes);
    assert_eq!(run("RECENT", &seen, None), Tri::No);
    assert_eq!(run("ALL", &seen, None), Tri::Yes);
    assert_eq!(run("2:4", &seen, None), Tri::Yes, "sequence number 3");
    assert_eq!(run("5:*", &seen, None), Tri::No);
    assert_eq!(run("UID 30", &seen, None), Tri::Yes);
    assert_eq!(run("UID 1:20", &seen, None), Tri::No);
    assert_eq!(run("LARGER 999", &seen, None), Tri::Yes);
    assert_eq!(run("SMALLER 1000", &seen, None), Tri::No);
    let kw = vec!["$label1".to_owned()];
    let with_kw = Facts { keywords: &kw, ..seen };
    assert_eq!(run("KEYWORD $Label1", &with_kw, None), Tri::Yes, "keywords are case-insensitive");
    assert_eq!(run("UNKEYWORD $label1", &with_kw, None), Tri::No);
    assert_eq!(run("KEYWORD other", &with_kw, None), Tri::No);
}

#[test]
fn internal_dates_compare_whole_days_in_utc() {
    let f = facts(Flags::empty()); // 2026-10-26 (UTC)
    assert_eq!(run("SINCE 26-Oct-2026", &f, None), Tri::Yes);
    assert_eq!(run("SINCE 27-Oct-2026", &f, None), Tri::No);
    assert_eq!(run("BEFORE 26-Oct-2026", &f, None), Tri::No);
    assert_eq!(run("BEFORE 27-Oct-2026", &f, None), Tri::Yes);
    assert_eq!(run("ON 26-Oct-2026", &f, None), Tri::Yes);
    assert_eq!(run("ON 25-Oct-2026", &f, None), Tri::No);
}

#[test]
fn boolean_logic_is_three_valued() {
    let f = facts(Flags::SEEN);
    // The flag alone decides: no content needed even though FROM would need it.
    assert_eq!(run("UNSEEN FROM alice", &f, None), Tri::No);
    assert_eq!(run("OR SEEN FROM nobody", &f, None), Tri::Yes);
    assert_eq!(run("SEEN FROM alice", &f, None), Tri::Needs);
    assert_eq!(run("NOT FROM alice", &f, None), Tri::Needs);
    assert_eq!(run("OR UNSEEN FROM alice", &f, None), Tri::Needs);
    assert!(!needs("SEEN"));
    assert!(needs("SEEN SUBJECT x"));
    assert!(needs("NOT OR SEEN BODY x"));
}

fn needs(q: &str) -> bool {
    let line = format!("a SEARCH {q}\r\n");
    let cmd = parse_command(line.as_bytes()).unwrap();
    let Body::Search { key: criteria, .. } = cmd.body else { panic!() };
    needs_content(&criteria)
}

#[test]
fn header_keys_match_decoded_values_case_insensitively() {
    let f = facts(Flags::empty());
    assert!(yes("FROM ALICE", &f, MSG));
    assert!(yes("FROM \"example.com\"", &f, MSG));
    assert!(!yes("FROM bob", &f, MSG));
    assert!(yes("TO bob@example.org", &f, MSG));
    assert!(yes("CC carol", &f, MSG));
    assert!(!yes("BCC carol", &f, MSG));
    assert!(yes("SUBJECT {5+}\r\ncaf\u{e9}", &f, MSG), "encoded words are decoded before matching");
    assert!(yes("SUBJECT MENU", &f, MSG));
    assert!(yes("HEADER X-Custom \"hello world\"", &f, MSG));
    assert!(yes("HEADER x-custom \"\"", &f, MSG), "an empty value matches any message that has the header");
    assert!(!yes("HEADER X-Missing \"\"", &f, MSG));
    assert!(yes("SENTSINCE 06-Nov-2026", &f, MSG));
    assert!(yes("SENTON 6-Nov-2026", &f, MSG));
    assert!(!yes("SENTBEFORE 06-Nov-2026", &f, MSG));
    assert!(yes("SENTBEFORE 07-Nov-2026", &f, MSG));
}

#[test]
fn body_and_text_search_decode_the_transfer_encoding() {
    let f = facts(Flags::empty());
    assert!(yes("BODY \"quick brown\"", &f, MSG));
    assert!(yes("BODY {3+}\r\n\u{2713}", &f, MSG), "quoted-printable is decoded");
    assert!(!yes("BODY alice", &f, MSG), "BODY does not look at headers");
    assert!(yes("TEXT alice", &f, MSG), "TEXT looks at headers and body");
    assert!(yes("TEXT JUMPS", &f, MSG));
    assert!(yes("BODY \"secret phrase\"", &f, B64), "base64 is decoded");
    assert!(!yes("BODY \"U2VjcmV0\"", &f, B64), "the encoded form is not searched");
}

#[test]
fn hostile_messages_do_not_panic() {
    let f = facts(Flags::empty());
    for raw in [&b""[..], b"\0\0\0", b"Subject: =?utf-8?q?=ZZ?=\r\n\r\n=\r\n=4", b"Content-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\n\r\n--x--"] {
        let _ = run("TEXT x SUBJECT y BODY z FROM q SENTSINCE 1-Jan-2000", &f, Some(raw));
    }
}

#[test]
fn an_absurd_date_header_year_is_ignored_not_an_overflow() {
    // Found by fuzz_imap_search_eval: a ten-digit year overflowed the day arithmetic.
    let f = facts(Flags::empty());
    let raw = b"Date: Fri, 06 Nov 99999999999 10:00:00 +0000\r\n\r\nx";
    assert!(!yes("SENTSINCE 1-Jan-2000", &f, raw));
    assert!(!yes("SENTBEFORE 1-Jan-2999", &f, raw));
}
