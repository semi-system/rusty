//! Library evaluation of `imap-codec` 1.0.0 (plan U6, first step): the command lines the target
//! clients send, run through the decoder. The table is built from the command forms Thunderbird,
//! K-9 Mail and Apple Mail are documented/known to send (RFC examples and the clients' public
//! traces), NOT from live captures: no client was available here. The U8 manual Thunderbird
//! checklist is where a real capture confirms or extends it.
//!
//! Result, which is the decision record for the dependency:
//! * every command of the U6 core and the U7 extras that `imap-codec` 1.0.0 knows decodes with
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

use imap_codec::decode::{CommandDecodeError, Decoder};
use imap_codec::imap_types::command::CommandBody;
use imap_codec::CommandCodec;

fn name(line: &str) -> Result<&'static str, String> {
    match CommandCodec::new().decode(line.as_bytes()) {
        Ok((rest, cmd)) => {
            assert!(rest.is_empty(), "{line:?}: trailing input {rest:?}");
            Ok(cmd.body.name())
        }
        Err(CommandDecodeError::Incomplete) => Err("incomplete".into()),
        Err(CommandDecodeError::LiteralFound { .. }) => Err("literal".into()),
        Err(CommandDecodeError::Failed) => Err("failed".into()),
    }
}

// backend-neutral: pure parser over bytes, no store involved
#[test]
fn commands_of_the_target_clients_decode() {
    let table: &[(&str, &str)] = &[
        // Any state
        ("1 capability\r\n", "CAPABILITY"),
        ("2 NOOP\r\n", "NOOP"),
        ("3 LOGOUT\r\n", "LOGOUT"),
        // Not authenticated
        ("4 STARTTLS\r\n", "STARTTLS"),
        ("5 login \"user@example.com\" \"s3cret\"\r\n", "LOGIN"),
        ("6 LOGIN user@example.com s3cret\r\n", "LOGIN"),
        ("7 authenticate PLAIN\r\n", "AUTHENTICATE"),
        ("8 AUTHENTICATE PLAIN AGFsaWNlAHBhc3M=\r\n", "AUTHENTICATE"),
        // Authenticated
        ("9 select \"INBOX\"\r\n", "SELECT"),
        ("10 EXAMINE INBOX\r\n", "EXAMINE"),
        ("11 CREATE \"Projects/2026\"\r\n", "CREATE"),
        ("12 DELETE Old\r\n", "DELETE"),
        ("13 RENAME \"A\" \"B/C\"\r\n", "RENAME"),
        ("14 SUBSCRIBE \"Sent\"\r\n", "SUBSCRIBE"),
        ("15 UNSUBSCRIBE \"Sent\"\r\n", "UNSUBSCRIBE"),
        ("16 list \"\" \"*\"\r\n", "LIST"),
        ("17 LIST \"\" \"INBOX\"\r\n", "LIST"),
        ("18 LIST \"\" \"%\"\r\n", "LIST"),
        ("19 LSUB \"\" \"*\"\r\n", "LSUB"),
        ("20 STATUS \"INBOX\" (UIDNEXT MESSAGES UNSEEN UIDVALIDITY)\r\n", "STATUS"),
        ("21 STATUS INBOX (MESSAGES UIDNEXT)\r\n", "STATUS"),
        ("22 APPEND \"Sent\" (\\Seen) {5+}\r\nhello\r\n", "APPEND"),
        ("23 APPEND Drafts (\\Seen \\Draft) \"07-Nov-2026 10:00:00 +0000\" {3+}\r\nabc\r\n", "APPEND"),
        ("24 ENABLE UTF8=ACCEPT\r\n", "ENABLE"),
        ("25 IDLE\r\n", "IDLE"),
        ("26 UNSELECT\r\n", "UNSELECT"),
        // Selected
        ("27 CHECK\r\n", "CHECK"),
        ("28 CLOSE\r\n", "CLOSE"),
        ("29 EXPUNGE\r\n", "EXPUNGE"),
        ("30 UID FETCH 1:* (FLAGS)\r\n", "FETCH"),
        ("31 UID FETCH 1:5 (UID RFC822.SIZE FLAGS BODY.PEEK[HEADER.FIELDS (From To Cc Bcc Subject Date Message-ID Priority X-Priority References Newsgroups In-Reply-To Content-Type Reply-To)])\r\n", "FETCH"),
        ("32 UID FETCH 5 (BODY.PEEK[]<0.20480>)\r\n", "FETCH"),
        ("33 UID FETCH 5 (BODYSTRUCTURE)\r\n", "FETCH"),
        ("34 FETCH 1:10 (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)\r\n", "FETCH"),
        ("35 FETCH 2 (BODY.PEEK[1.2.MIME] BODY[TEXT]<10.100>)\r\n", "FETCH"),
        ("36 FETCH 1:3 FAST\r\n", "FETCH"),
        ("37 UID STORE 5 +FLAGS (\\Seen)\r\n", "STORE"),
        ("38 UID STORE 5 +FLAGS.SILENT (\\Deleted)\r\n", "STORE"),
        ("39 STORE 1:3 -FLAGS ($label1 \\Flagged)\r\n", "STORE"),
        ("40 UID COPY 5 \"Trash\"\r\n", "COPY"),
        ("41 UID MOVE 5 \"Trash\"\r\n", "MOVE"),
        ("42 MOVE 1:2 Archive\r\n", "MOVE"),
        ("43 UID SEARCH CHARSET UTF-8 SUBJECT \"x\"\r\n", "SEARCH"),
        ("44 SEARCH UNSEEN\r\n", "SEARCH"),
        ("45 UID SEARCH UNDELETED 1:*\r\n", "SEARCH"),
        ("46 SEARCH OR FROM \"a\" TO \"b\" NOT SEEN SINCE 1-Jan-2026 LARGER 1000\r\n", "SEARCH"),
        ("47 SEARCH HEADER Message-ID \"<x@y>\" BODY \"hello\" TEXT \"world\"\r\n", "SEARCH"),
        ("48 SEARCH KEYWORD $label1\r\n", "SEARCH"),
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
fn what_1_0_0_does_not_know_is_exactly_the_u7_list() {
    // These all FAIL in imap-codec 1.0.0 (see the module docs): src/ext.rs parses them.
    for line in [
        "1 ID (\"name\" \"Thunderbird\" \"version\" \"115.0\")\r\n",
        "2 ID NIL\r\n",
        "3 NAMESPACE\r\n",
        "4 UID EXPUNGE 5:7\r\n",
        "5 LIST (SUBSCRIBED) \"\" \"*\" RETURN (SPECIAL-USE)\r\n",
        "6 LIST \"\" \"*\" RETURN (CHILDREN)\r\n",
    ] {
        assert_eq!(name(line), Err("failed".to_owned()), "{line:?} became supported: drop it from src/ext.rs");
    }
}

// backend-neutral: pure parser over bytes, no store involved
#[test]
fn literals_are_reported_not_buffered() {
    // A synchronising literal stops the decoder at the literal; the caller (our framer) decides.
    match CommandCodec::new().decode(b"a LOGIN {5}\r\n") {
        Err(CommandDecodeError::LiteralFound { length, .. }) => assert_eq!(length, 5),
        other => panic!("{other:?}"),
    }
    // A literal that is complete decodes.
    assert!(matches!(
        CommandCodec::new().decode(b"a LOGIN {5}\r\nalice {4}\r\npass\r\n"),
        Ok((_, c)) if matches!(c.body, CommandBody::Login { .. })
    ));
    // Missing CRLF terminator or bare LF: refused (the framer passes the bytes through).
    assert!(matches!(CommandCodec::new().decode(b"a NOOP\n"), Err(CommandDecodeError::Failed)));
}
