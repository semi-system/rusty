//! The codec layer of rusty-smtp: bytes in, typed values out. No I/O, no state beyond the
//! reader buffers. Line framing, command parsing and DATA framing (dot-unstuffing, size cap,
//! smuggling-safe terminator).

use rusty_smtp::codec::{parse_command, Command, DataEnd, DataReader, LineEvent, LineReader, ParseError};

fn lines(max: usize, chunks: &[&[u8]]) -> Vec<String> {
    let mut r = LineReader::new(max);
    let mut out = Vec::new();
    for c in chunks {
        let mut input: &[u8] = c;
        while let Some(ev) = r.read_line(&mut input) {
            out.push(match ev {
                LineEvent::Line(l) => String::from_utf8_lossy(&l).into_owned(),
                LineEvent::TooLong => "<TOOLONG>".to_owned(),
            });
        }
        assert!(input.is_empty(), "the reader must consume everything it is given");
    }
    out
}

// backend-neutral: pure codec, no database
#[test]
fn lines_are_split_on_crlf_and_bare_lf_across_chunks() {
    assert_eq!(lines(100, &[b"EHLO a\r\nNOOP\r\n"]), ["EHLO a", "NOOP"]);
    assert_eq!(lines(100, &[b"EH", b"LO a\r", b"\nQU", b"IT\n"]), ["EHLO a", "QUIT"]);
    assert_eq!(lines(100, &[b"\r\n\r\n"]), ["", ""]);
    // a partial line is kept until its terminator arrives
    assert!(lines(100, &[b"EHLO a"]).is_empty());
}

// backend-neutral: pure codec, no database
#[test]
fn an_overlong_line_is_reported_once_and_skipped_to_its_end() {
    let long = vec![b'a'; 50];
    let mut a = long.clone();
    a.extend_from_slice(b"\r\nNOOP\r\n");
    assert_eq!(lines(10, &[&a]), ["<TOOLONG>", "NOOP"]);
    // split over chunks, still bounded
    assert_eq!(lines(10, &[&long[..20], &long[20..], b"\r\nQUIT\r\n"]), ["<TOOLONG>", "QUIT"]);
    // exactly the limit is fine, one more is not
    assert_eq!(lines(4, &[b"abcd\r\n"]), ["abcd"]);
    assert_eq!(lines(4, &[b"abcde\r\n"]), ["<TOOLONG>"]);
}

// backend-neutral: pure codec, no database
#[test]
fn the_line_buffer_never_exceeds_the_limit() {
    let mut r = LineReader::new(16);
    let junk = vec![b'x'; 1 << 20];
    let mut input: &[u8] = &junk;
    let mut too_long = 0;
    while let Some(ev) = r.read_line(&mut input) {
        if matches!(ev, LineEvent::TooLong) {
            too_long += 1;
        }
    }
    assert!(r.buffered() <= 17, "buffered {}", r.buffered());
    assert!(too_long <= 1);
}

fn cmd(s: &str) -> Result<Command, ParseError> {
    parse_command(s.as_bytes())
}

// backend-neutral: pure codec, no database
#[test]
fn verbs_are_case_insensitive_and_typed() {
    assert_eq!(cmd("EHLO client.example").unwrap(), Command::Ehlo("client.example".into()));
    assert_eq!(cmd("ehlo [1.2.3.4]").unwrap(), Command::Ehlo("[1.2.3.4]".into()));
    assert_eq!(cmd("HeLo x").unwrap(), Command::Helo("x".into()));
    assert_eq!(cmd("DATA").unwrap(), Command::Data);
    assert_eq!(cmd("rset").unwrap(), Command::Rset);
    assert_eq!(cmd("NOOP").unwrap(), Command::Noop);
    assert_eq!(cmd("NOOP anything").unwrap(), Command::Noop);
    assert_eq!(cmd("QUIT").unwrap(), Command::Quit);
    assert_eq!(cmd("STARTTLS").unwrap(), Command::StartTls);
    assert_eq!(cmd("VRFY someone").unwrap(), Command::Vrfy);
    assert_eq!(cmd("HELP").unwrap(), Command::Help);
}

// backend-neutral: pure codec, no database
#[test]
fn mail_and_rcpt_paths() {
    match cmd("MAIL FROM:<alice@example.org>").unwrap() {
        Command::Mail { path, params } => {
            assert_eq!(path, "alice@example.org");
            assert_eq!((params.size, params.body_8bit, params.smtputf8), (None, false, false));
        }
        other => panic!("{other:?}"),
    }
    match cmd("mail from:<>").unwrap() {
        Command::Mail { path, .. } => assert_eq!(path, ""),
        other => panic!("{other:?}"),
    }
    match cmd("MAIL FROM:<a@b.c> SIZE=1234 BODY=8BITMIME SMTPUTF8").unwrap() {
        Command::Mail { params, .. } => {
            assert_eq!(params.size, Some(1234));
            assert!(params.body_8bit && params.smtputf8);
        }
        other => panic!("{other:?}"),
    }
    match cmd("MAIL FROM:<a@b.c> BODY=7BIT").unwrap() {
        Command::Mail { params, .. } => assert!(!params.body_8bit),
        other => panic!("{other:?}"),
    }
    assert_eq!(cmd("RCPT TO:<bob@example.com>").unwrap(), Command::Rcpt { path: "bob@example.com".into() });
    // RFC 5321 4.5.1: a bare `<postmaster>` (no domain) must be accepted; the server decides which domain
    assert_eq!(cmd("rcpt to:<Postmaster>").unwrap(), Command::Rcpt { path: "postmaster".into() });
    assert_eq!(cmd("RCPT TO: <POSTMASTER>").unwrap(), Command::Rcpt { path: "postmaster".into() });
    // but a bare name that is not postmaster is not an address
    assert_eq!(cmd("RCPT TO:<alice>").unwrap_err(), ParseError::BadAddress);
    assert_eq!(cmd("RCPT TO:<>").unwrap_err(), ParseError::BadAddress);
    // the 8-bit local part / domain travels as UTF-8
    assert_eq!(cmd("RCPT TO:<ünal@bücher.example>").unwrap(), Command::Rcpt { path: "ünal@bücher.example".into() });
}

// backend-neutral: pure codec, no database
#[test]
fn bad_commands_are_classified() {
    assert_eq!(cmd("FROB").unwrap_err(), ParseError::Unknown);
    assert_eq!(cmd("").unwrap_err(), ParseError::Unknown);
    assert!(matches!(cmd("MAIL").unwrap_err(), ParseError::Syntax(_)));
    assert!(matches!(cmd("MAIL FROM:a@b.c").unwrap_err(), ParseError::Syntax(_) | ParseError::BadAddress));
    assert!(matches!(cmd("RCPT TO:<a@b.c").unwrap_err(), ParseError::Syntax(_) | ParseError::BadAddress));
    assert!(matches!(cmd("RCPT").unwrap_err(), ParseError::Syntax(_)));
    assert!(matches!(cmd("EHLO").unwrap_err(), ParseError::Syntax(_)));
    assert!(matches!(cmd("MAIL FROM:<a@b.c> SIZE=abc").unwrap_err(), ParseError::Syntax(_) | ParseError::Unsupported(_)));
    // parameters we do not advertise are refused, not ignored
    assert!(matches!(cmd("MAIL FROM:<a@b.c> BODY=BINARYMIME").unwrap_err(), ParseError::Unsupported(_)));
    assert!(matches!(cmd("MAIL FROM:<a@b.c> RET=FULL").unwrap_err(), ParseError::Unsupported(_)));
    assert!(matches!(cmd("RCPT TO:<a@b.c> NOTIFY=SUCCESS").unwrap_err(), ParseError::Unsupported(_)));
    assert!(matches!(cmd("MAIL FROM:<a@b.c> NOSUCH=1").unwrap_err(), ParseError::Unsupported(_) | ParseError::Syntax(_)));
}

// backend-neutral: pure codec, no database
#[test]
fn control_bytes_and_invalid_utf8_are_refused_before_parsing() {
    for bad in [&b"EHLO a\x00b"[..], b"EHLO a\x01", b"MAIL FROM:<a\x00@b.c>", b"EHLO \xff\xfe", b"RCPT TO:<a@b.c>\x7f"] {
        assert!(matches!(parse_command(bad), Err(ParseError::Syntax(_))), "{bad:?}");
    }
    // TAB is whitespace, not a control error
    assert!(parse_command(b"EHLO\tx").is_ok());
}

fn data(max: usize, chunks: &[&[u8]]) -> (Option<DataEnd>, Vec<u8>) {
    let mut r = DataReader::new(max);
    let mut rest = Vec::new();
    for (i, c) in chunks.iter().enumerate() {
        let mut input: &[u8] = c;
        if let Some(end) = r.feed(&mut input) {
            rest.extend_from_slice(input);
            for later in &chunks[i + 1..] {
                rest.extend_from_slice(later);
            }
            return (Some(end), rest);
        }
        assert!(input.is_empty());
    }
    (None, rest)
}

fn complete(end: Option<DataEnd>) -> (Vec<u8>, bool) {
    match end {
        Some(DataEnd::Complete { data, has_nul }) => (data, has_nul),
        other => panic!("not complete: {other:?}"),
    }
}

// backend-neutral: pure codec, no database
#[test]
fn data_is_unstuffed_and_ends_at_crlf_dot_crlf() {
    let (end, rest) = data(1000, &[b"hello\r\nworld\r\n.\r\nNEXT\r\n"]);
    let (d, nul) = complete(end);
    assert_eq!((d.as_slice(), nul), (&b"hello\r\nworld\r\n"[..], false));
    assert_eq!(rest, b"NEXT\r\n", "bytes after the terminator are left for the command reader");
    // an empty body: the terminator right away
    let (d, _) = complete(data(1000, &[b".\r\n"]).0);
    assert!(d.is_empty());
    // dot-stuffing: leading dots are removed once
    let (d, _) = complete(data(1000, &[b"..\r\n.a\r\n...b\r\nx.\r\n.\r\n"]).0);
    assert_eq!(d, b".\r\na\r\n..b\r\nx.\r\n");
}

// backend-neutral: pure codec, no database
#[test]
fn data_framing_is_independent_of_chunking() {
    let whole: &[u8] = b"a\r\n..b\r\n\r\n.\r\nrest";
    let (d_all, _) = complete(data(1000, &[whole]).0);
    for split in 1..whole.len() {
        let (end, _) = data(1000, &[&whole[..split], &whole[split..]]);
        assert_eq!(complete(end).0, d_all, "split at {split}");
    }
    // one byte at a time
    let bytes: Vec<&[u8]> = whole.chunks(1).collect();
    assert_eq!(complete(data(1000, &bytes).0).0, d_all);
}

// backend-neutral: pure codec, no database
#[test]
fn bare_lf_and_bare_cr_terminators_do_not_end_the_data_smuggling() {
    // The classic smuggling payloads end with a "dot line" that is not CRLF.CRLF; none ends DATA.
    for payload in [&b"a\n.\r\nMAIL FROM:<x@y.z>\r\n"[..], b"a\r\n.\nMAIL", b"a\r.\r\nMAIL", b"a\n.\nMAIL"] {
        let (end, _) = data(1000, &[payload]);
        assert!(end.is_none(), "{:?} ended the data", String::from_utf8_lossy(payload));
    }
    // and the real terminator after such bytes still works, the smuggled text is message body
    let (end, rest) = data(1000, &[b"a\n.\r\nMAIL FROM:<x@y.z>\r\n.\r\nQUIT\r\n"]);
    let (d, _) = complete(end);
    assert_eq!(d, b"a\n.\r\nMAIL FROM:<x@y.z>\r\n");
    assert_eq!(rest, b"QUIT\r\n");
}

// backend-neutral: pure codec, no database
#[test]
fn data_over_the_cap_is_too_large_and_stops_buffering() {
    let mut r = DataReader::new(10);
    let big = vec![b'a'; 100];
    let mut input: &[u8] = &big;
    assert_eq!(r.feed(&mut input), Some(DataEnd::TooLarge));
    // the cap counts the unstuffed bytes (CRLFs included): exactly at it is fine, one over is not
    let (d, _) = complete(data(6, &[b"abcd\r\n.\r\n"]).0);
    assert_eq!(d, b"abcd\r\n");
    assert_eq!(data(5, &[b"abcd\r\n.\r\n"]).0, Some(DataEnd::TooLarge));
}

// backend-neutral: pure codec, no database
#[test]
fn nul_bytes_are_flagged() {
    let (d, nul) = complete(data(1000, &[b"a\x00b\r\n.\r\n"]).0);
    assert!(nul);
    assert_eq!(d, b"a\x00b\r\n");
}

proptest::proptest! {
    // backend-neutral: the codecs must never panic nor buffer beyond their caps
    #[test]
    fn arbitrary_bytes_never_panic_or_overrun(
        chunks in proptest::collection::vec(proptest::collection::vec(proptest::prelude::any::<u8>(), 0..200), 0..8)
    ) {
        let mut lr = LineReader::new(64);
        let mut dr = DataReader::new(256);
        for c in &chunks {
            let mut input: &[u8] = c;
            while let Some(LineEvent::Line(l)) = lr.read_line(&mut input) {
                let _ = parse_command(&l);
            }
            let mut input: &[u8] = c;
            let _ = dr.feed(&mut input);
        }
        proptest::prop_assert!(lr.buffered() <= 65);
    }
}
