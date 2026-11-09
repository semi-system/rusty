//! Entry points shared by the cargo-fuzz targets (`fuzz/fuzz_targets/fuzz_smtp_*.rs`) and their
//! stable-toolchain proptest stand-ins (`tests/fuzz_smoke.rs`): each takes arbitrary bytes, must
//! never panic, and asserts the invariants of the layer it drives.

use std::net::IpAddr;

use crate::codec::{parse_command, Command, DataEnd, DataReader, LineEvent, LineReader};
use crate::message::{normalise_crlf, prepare_message, Trace};

const LINE_MAX: usize = 1000;
const DATA_MAX: usize = 4096;

/// Line framing plus command parsing over a byte stream.
pub fn check_command_stream(data: &[u8]) {
    let mut reader = LineReader::new(LINE_MAX);
    let mut input = data;
    while let Some(ev) = reader.read_line(&mut input) {
        if let LineEvent::Line(line) = ev {
            assert!(line.len() <= LINE_MAX);
            let _ = parse_command(&line);
        }
        assert!(reader.buffered() <= LINE_MAX + 1);
    }
    assert!(reader.buffered() <= LINE_MAX + 1);
}

fn run_data(data: &[u8], chunk: usize) -> (Option<DataEnd>, usize) {
    let mut reader = DataReader::new(DATA_MAX);
    let mut consumed = 0;
    for piece in data.chunks(chunk.max(1)) {
        let mut input = piece;
        let end = reader.feed(&mut input);
        consumed += piece.len() - input.len();
        if end.is_some() {
            return (end, consumed);
        }
    }
    (None, consumed)
}

/// DATA framing: the cap holds, NUL is reported exactly, the result does not depend on how the
/// bytes are chunked, and nothing past the terminator is consumed.
pub fn check_data(data: &[u8]) {
    let (whole, used_whole) = run_data(data, data.len().max(1));
    let split = usize::from(data.first().copied().unwrap_or(1)).max(1);
    let (chunked, used_chunked) = run_data(data, split);
    assert_eq!(whole, chunked, "chunking changed the result");
    assert_eq!(used_whole, used_chunked, "chunking changed what was consumed");
    if let Some(DataEnd::Complete { data: out, has_nul }) = &whole {
        assert!(out.len() <= DATA_MAX);
        assert_eq!(*has_nul, out.contains(&0));
        assert!(used_whole <= data.len());
    }
}

/// MAIL / RCPT paths and parameters: arbitrary text after the verb never panics, and an accepted
/// path never carries framing characters.
pub fn check_path(data: &[u8]) {
    for verb in [&b"MAIL FROM:"[..], &b"RCPT TO:"[..], &b"MAIL FROM:<a@b.c> "[..], &b"RCPT TO:<a@b.c> "[..]] {
        let mut line = verb.to_vec();
        line.extend_from_slice(data);
        if let Ok(Command::Mail { path, .. } | Command::Rcpt { path }) = parse_command(&line) {
            assert!(!path.contains(['\r', '\n', '\0']));
            assert!(path.len() <= LINE_MAX);
        }
    }
}

fn header_section(out: &[u8]) -> &[u8] {
    match out.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(p) => out.get(..p).unwrap_or_default(),
        None => out,
    }
}

/// Header editing: the output starts with our trace headers, carries exactly one
/// `Authentication-Results` (ours) and no `Received-SPF` or `X-Rusty-*` in its header section, and
/// keeps the body (line endings normalised).
pub fn check_headers(data: &[u8]) {
    let trace = Trace {
        hostname: "mx.example.net",
        helo: "client.example",
        peer: IpAddr::from([192, 0, 2, 1]),
        tls: false,
        mail_from: "a@b.c",
        queue_id: "000000000000",
        received_unix: 0,
    };
    let out = prepare_message(data, &trace);
    assert!(out.starts_with(b"Return-Path: <a@b.c>\r\nReceived: from "));
    let head = header_section(&out);
    let mut ar = 0;
    for line in head.split(|&b| b == b'\n') {
        if matches!(line.first(), Some(b' ' | b'\t')) {
            continue;
        }
        let lower: Vec<u8> = line.iter().map(u8::to_ascii_lowercase).collect();
        if lower.starts_with(b"authentication-results:") {
            ar += 1;
        }
        assert!(!lower.starts_with(b"received-spf:"), "a forged Received-SPF survived");
        assert!(!lower.starts_with(b"x-rusty-"), "a forged X-Rusty header survived");
    }
    assert_eq!(ar, 1, "exactly our own Authentication-Results");
    // The part after the header section is the normalised original body.
    let norm = normalise_crlf(data);
    if let Some(p) = norm.windows(4).position(|w| w == b"\r\n\r\n") {
        let body = norm.get(p + 2..).unwrap_or_default();
        assert!(out.ends_with(body));
    }
}
