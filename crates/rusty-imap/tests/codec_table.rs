//! Library evaluation of the grammar library 1.0.0 (plan U6, first step): the command lines the target
//! clients send, run through the decoder. The table is built from the command forms Thunderbird,
//! K-9 Mail and Apple Mail are documented/known to send (RFC examples and the clients' public
//! traces), NOT from live captures: no client was available here. The U8 manual Thunderbird
//! checklist is where a real capture confirms or extends it.
//!
//! Result, which is the decision record for the dependency:
//! * every command of the U6 core and the U7 extras that the grammar library 1.0.0 knows decodes with
//!   the features `starttls` (plus the default quirks): CAPABILITY, STARTTLS, LOGIN,
//!   AUTHENTICATE (with and without SASL-IR), SELECT/EXAMINE, CREATE/DELETE/RENAME,
//!   SUBSCRIBE/UNSUBSCRIBE, LIST/LSUB, STATUS, APPEND (literal, LITERAL+, flags, date), CHECK,
//!   CLOSE, UNSELECT, EXPUNGE, SEARCH (charset, OR/NOT, UID, header and date keys), FETCH
//!   (macros, BODY.PEEK sections, HEADER.FIELDS, partials), STORE, COPY, IDLE, ENABLE, MOVE
//!   and the UID forms of FETCH/STORE/SEARCH/COPY/MOVE;
//! * literal handling is under our control: the decoder reports `LiteralFound` and the framer
//!   (src/frame.rs) enforces the size caps before anything is read (see its tests);
//! * MISSING in 1.0.0 (all U7 items): `ID`, `NAMESPACE`, `UID EXPUNGE` (UIDPLUS), LIST selection
//!   and return options (`LIST (SUBSCRIBED) ... RETURN (SPECIAL-USE)`), and the APPENDUID /
//!   COPYUID response codes. 2.0.0-alpha.10 has ext_id, ext_namespace and ext_list_extended, but
//!   it is an alpha with a different API; the decision is to stay on the stable 1.0.0 and parse
//!   these few verbs ourselves in `src/ext.rs` (small, fuzzed with the rest) - they are a few
//!   fixed grammars - and to write the response codes ourselves (we write all responses
//!   ourselves anyway, see src/wire.rs). SPECIAL-USE needs no parsing: the `\Sent` attributes go
//!   into ordinary LIST replies.
//!
//! The assertions below pin both halves, so a library upgrade that changes either shows up here.

use rusty_imap::adapter::parse_command;

/// The variant name of the parsed command, or `"failed"`.
fn name(line: &str) -> Result<String, String> {
    match parse_command(line.as_bytes()) {
        Ok(c) => {
            let d = format!("{:?}", c.body);
            Ok(d.split(|ch: char| !ch.is_ascii_alphanumeric()).next().unwrap_or("").to_owned())
        }
        Err(_) => Err("failed".into()),
    }
}

// backend-neutral: pure parser over bytes, no store involved
#[test]
fn commands_of_the_target_clients_decode() {
    let table: &[(&str, &str)] = &[
        // Any state
        ("1 capability\r\n", "Capability"),
        ("2 NOOP\r\n", "Noop"),
        ("3 LOGOUT\r\n", "Logout"),
        // Not authenticated
        ("4 STARTTLS\r\n", "StartTls"),
        ("5 login \"user@example.com\" \"s3cret\"\r\n", "Login"),
        ("6 LOGIN user@example.com s3cret\r\n", "Login"),
        ("7 authenticate PLAIN\r\n", "Authenticate"),
        ("8 AUTHENTICATE PLAIN AGFsaWNlAHBhc3M=\r\n", "Authenticate"),
        // Authenticated
        ("9 select \"INBOX\"\r\n", "Select"),
        ("10 EXAMINE INBOX\r\n", "Examine"),
        ("11 CREATE \"Projects/2026\"\r\n", "Create"),
        ("12 DELETE Old\r\n", "Delete"),
        ("13 RENAME \"A\" \"B/C\"\r\n", "Rename"),
        ("14 SUBSCRIBE \"Sent\"\r\n", "Subscribe"),
        ("15 UNSUBSCRIBE \"Sent\"\r\n", "Unsubscribe"),
        ("16 list \"\" \"*\"\r\n", "List"),
        ("17 LIST \"\" \"INBOX\"\r\n", "List"),
        ("18 LIST \"\" \"%\"\r\n", "List"),
        ("19 LSUB \"\" \"*\"\r\n", "Lsub"),
        ("20 STATUS \"INBOX\" (UIDNEXT MESSAGES UNSEEN UIDVALIDITY)\r\n", "Status"),
        ("21 STATUS INBOX (MESSAGES UIDNEXT)\r\n", "Status"),
        ("22 APPEND \"Sent\" (\\Seen) {5+}\r\nhello\r\n", "Append"),
        ("23 APPEND Drafts (\\Seen \\Draft) \"07-Nov-2026 10:00:00 +0000\" {3+}\r\nabc\r\n", "Append"),
        ("24 ENABLE UTF8=ACCEPT\r\n", "Enable"),
        ("25 IDLE\r\n", "Idle"),
        ("26 UNSELECT\r\n", "Unselect"),
        // Selected
        ("27 CHECK\r\n", "Check"),
        ("28 CLOSE\r\n", "Close"),
        ("29 EXPUNGE\r\n", "Expunge"),
        ("30 UID FETCH 1:* (FLAGS)\r\n", "Fetch"),
        ("31 UID FETCH 1:5 (UID RFC822.SIZE FLAGS BODY.PEEK[HEADER.FIELDS (From To Cc Bcc Subject Date Message-ID Priority X-Priority References Newsgroups In-Reply-To Content-Type Reply-To)])\r\n", "Fetch"),
        ("32 UID FETCH 5 (BODY.PEEK[]<0.20480>)\r\n", "Fetch"),
        ("33 UID FETCH 5 (BODYSTRUCTURE)\r\n", "Fetch"),
        ("34 FETCH 1:10 (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)\r\n", "Fetch"),
        ("35 FETCH 2 (BODY.PEEK[1.2.MIME] BODY[TEXT]<10.100>)\r\n", "Fetch"),
        ("36 FETCH 1:3 FAST\r\n", "Fetch"),
        ("37 UID STORE 5 +FLAGS (\\Seen)\r\n", "Store"),
        ("38 UID STORE 5 +FLAGS.SILENT (\\Deleted)\r\n", "Store"),
        ("39 STORE 1:3 -FLAGS ($label1 \\Flagged)\r\n", "Store"),
        ("40 UID COPY 5 \"Trash\"\r\n", "Copy"),
        ("41 UID MOVE 5 \"Trash\"\r\n", "Move"),
        ("42 MOVE 1:2 Archive\r\n", "Move"),
        ("43 UID SEARCH CHARSET UTF-8 SUBJECT \"x\"\r\n", "Search"),
        ("44 SEARCH UNSEEN\r\n", "Search"),
        ("45 UID SEARCH UNDELETED 1:*\r\n", "Search"),
        ("46 SEARCH OR FROM \"a\" TO \"b\" NOT SEEN SINCE 1-Jan-2026 LARGER 1000\r\n", "Search"),
        ("47 SEARCH HEADER Message-ID \"<x@y>\" BODY \"hello\" TEXT \"world\"\r\n", "Search"),
        ("48 SEARCH KEYWORD $label1\r\n", "Search"),
    ];
    let mut bad = vec![];
    for (line, want) in table {
        match name(line) {
            Ok(got) if got == *want => {}
            other => bad.push(format!("{line:?}: wanted {want}, got {other:?}")),
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

// backend-neutral: pure parser over bytes, no store involved
#[test]
fn what_the_library_lacks_is_parsed_by_our_own_layer() {
    // The grammar library (1.0.0) fails on all of these (pinned by the evaluation recorded in the
    // module docs); the adapter answers them with src/ext.rs.
    for (line, want) in [
        ("1 ID (\"name\" \"Thunderbird\" \"version\" \"115.0\")\r\n", "Id"),
        ("2 ID NIL\r\n", "Id"),
        ("3 NAMESPACE\r\n", "Namespace"),
        ("4 UID EXPUNGE 5:7\r\n", "UidExpunge"),
        ("5 LIST (SUBSCRIBED) \"\" \"*\" RETURN (SPECIAL-USE)\r\n", "ListExt"),
        ("6 LIST \"\" \"*\" RETURN (CHILDREN)\r\n", "ListExt"),
    ] {
        assert_eq!(name(line), Ok(want.to_owned()), "{line:?}");
    }
}

// backend-neutral: pure parser over bytes, no store involved
#[test]
fn literals_are_the_framers_business() {
    // A command that still announces literal data is not complete: the adapter refuses it (the
    // framer never hands such a frame over; the size caps are tested in src/frame.rs).
    assert!(parse_command(b"a LOGIN {5}\r\n").is_err());
    // A complete one parses, and a bare LF terminator is refused.
    assert!(parse_command(b"a LOGIN {5}\r\nalice {4}\r\npass\r\n").is_ok());
    assert!(parse_command(b"a NOOP\n").is_err());
    // Mailbox names are decoded from modified UTF-7 by the adapter; a broken one is an error.
    assert!(parse_command(b"a SELECT \"&AGE-\"\r\n").is_err());
}
