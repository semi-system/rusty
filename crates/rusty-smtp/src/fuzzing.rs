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
        authenticated: false,
        auth_results: None,
    };
    let out = prepare_message(data, &trace);
    assert!(out.starts_with(b"Return-Path: <a@b.c>\r\nReceived: from "));
    let head = header_section(&out);
    for (i, &b) in head.iter().enumerate() {
        if b == b'\r' {
            assert_eq!(head.get(i + 1), Some(&b'\n'), "a lone CR survived in the header block");
        }
    }
    let mut ar = 0;
    let mut rp = 0;
    for line in head.split(|&b| b == b'\n') {
        if matches!(line.first(), Some(b' ' | b'\t')) {
            continue;
        }
        let lower: Vec<u8> = line.iter().map(u8::to_ascii_lowercase).collect();
        if lower.starts_with(b"authentication-results:") {
            ar += 1;
        }
        if lower.starts_with(b"return-path:") {
            rp += 1;
        }
        assert!(!lower.starts_with(b"received-spf:"), "a forged Received-SPF survived");
        assert!(!lower.starts_with(b"x-rusty-"), "a forged X-Rusty header survived");
    }
    assert_eq!(ar, 1, "exactly our own Authentication-Results");
    assert_eq!(rp, 1, "exactly our own Return-Path");
    // The part after the header section is the normalised original body.
    let norm = normalise_crlf(data);
    if let Some(p) = norm.windows(4).position(|w| w == b"\r\n\r\n") {
        let body = norm.get(p + 2..).unwrap_or_default();
        assert!(out.ends_with(body));
    }
}

/// SASL decoding and the submission AUTH exchange: arbitrary bytes after `AUTH PLAIN` / `AUTH LOGIN`
/// never panic, decoded values keep their bounds, and credentials never appear in `Debug` output.
pub fn check_auth(data: &[u8]) {
    use crate::codec::{decode_base64, parse_plain};
    use crate::session::{Action, PeerFacts, Session, SessionConfig};

    let text = String::from_utf8_lossy(data);
    if let Ok(raw) = decode_base64(&text, 2048) {
        assert!(raw.len() <= 2048);
        assert!(raw.len() / 3 * 4 <= text.len());
        if let Ok(c) = parse_plain(&raw) {
            assert!(!c.authcid.is_empty() && !c.password.is_empty());
            assert!(!c.authcid.contains('\0') && !c.password.contains('\0') && !c.authzid.contains('\0'));
        }
    }
    let cfg = SessionConfig {
        hostname: "mx.example.net".into(),
        max_message_bytes: 4096,
        max_recipients: 5,
        max_accounts: 50,
        max_transactions: 5,
        max_commands_without_transaction: 100,
        max_errors: 50,
        max_line: LINE_MAX,
        starttls_available: true,
        submission: true,
    };
    for opener in [&b"AUTH PLAIN\r\n"[..], &b"AUTH LOGIN\r\n"[..]] {
        let (mut s, _) = Session::new(cfg.clone(), PeerFacts { ip: IpAddr::from([192, 0, 2, 1]), tls: true });
        let _ = s.feed(b"EHLO x\r\n");
        let _ = s.feed(opener);
        for piece in data.chunks(97) {
            let actions = s.feed(piece);
            for a in &actions {
                if let Action::Authenticate { password, .. } = a {
                    assert!(format!("{password:?}") == "<redacted>" && format!("{a:?}").contains("<redacted>"));
                }
            }
        }
    }
}

/// The reply parser of the outbound client: arbitrary bytes from a hostile server never panic; the
/// reported length stays inside the input; a parsed reply honours the limits; and a complete reply
/// parsed from a prefix equals the one parsed from the whole input (no dependence on chunking).
pub fn check_reply(data: &[u8]) {
    use crate::outbound::smtp_reply::{parse_reply, MAX_LINES, MAX_REPLY};
    if let Ok(Some((reply, used))) = parse_reply(data) {
            assert!(used <= data.len() && used <= MAX_REPLY + 1);
            assert!(reply.lines.len() <= MAX_LINES && (100..600).contains(&reply.code));
            let again = parse_reply(data.get(..used).unwrap_or_default()).ok().flatten();
            assert_eq!(again, Some((reply, used)), "a reply is self-delimiting");
    }
}

/// The header copier of a bounce: never panics, never contains the body, never exceeds the cap, never
/// holds a line that could pass for a MIME boundary or a control character.
pub fn check_dsn_headers(data: &[u8]) {
    use crate::outbound::dsn::{header_section, MAX_HEADERS_BYTES};
    let out = header_section(data, MAX_HEADERS_BYTES);
    assert!(out.len() <= MAX_HEADERS_BYTES);
    assert!(out.is_empty() || out.ends_with(b"\r\n"));
    for line in out.split(|&b| b == b'\n') {
        assert!(!line.starts_with(b"--"));
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        assert!(line.iter().all(|&b| b == b'\t' || !b.is_ascii_control()));
    }
    if let Some(p) = data.windows(4).position(|w| w == b"\r\n\r\n") {
        let body = data.get(p + 4..).unwrap_or_default();
        if body.len() > 8 {
            assert!(!out.windows(body.len()).any(|w| w == body), "the body must not be copied");
        }
    }
}

/// The queue payload decoder: any JSON value decodes or is refused without a panic, and what decodes
/// is safe to put on the wire (no CR, LF, angle brackets or spaces in any address) and round-trips.
pub fn check_queue_payload(data: &[u8]) {
    use crate::outbound::payload::OutboundPayload;
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) else { return };
    if let Ok(p) = OutboundPayload::decode(&v) {
        for a in p.recipients.iter().chain(std::iter::once(&p.envelope_from)) {
            assert!(!a.chars().any(|c| c.is_control() || c.is_whitespace() || c == '<' || c == '>'));
        }
        assert_eq!(OutboundPayload::decode(&p.encode()).ok(), Some(p));
    }
}

/// A hostile DKIM key record: arbitrary bytes as the `p=` of an RSA and of an Ed25519 key record (the DER
/// reader that unwraps a SubjectPublicKeyInfo and measures the modulus is code we patched in), and a
/// signature that must then verify to a verdict without a panic.
pub fn check_dkim_key_record(data: &[u8]) {
    use base64::Engine;
    use crate::authres::AuthInput;
    use crate::resolve::StaticDns;
    let p = base64::engine::general_purpose::STANDARD.encode(data);
    let mut dns = StaticDns::default();
    dns.txt.insert("s1._domainkey.example.org".into(), format!("v=DKIM1; k=rsa; p={p}"));
    dns.txt.insert("s2._domainkey.example.org".into(), format!("v=DKIM1; k=ed25519; p={p}"));
    let Ok(stack) = crate::stack::DnsStack::with_static(dns, "mx.example.net") else { return };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().build() else { return };
    for (alg, sel) in [("rsa-sha256", "s1"), ("ed25519-sha256", "s2")] {
        let msg = format!(
            "DKIM-Signature: v=1; a={alg}; c=relaxed/relaxed; d=example.org; s={sel}; h=from; bh=2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=; b=AAAA\r\nFrom: a@example.org\r\n\r\nx\r\n"
        );
        let r = rt.block_on(stack.checker.check(&AuthInput { peer: [192, 0, 2, 1].into(), helo: "h", mail_from: "a@example.org", message: msg.as_bytes() }));
        assert!(r.dkim != "pass", "a garbage key must never verify a garbage signature");
    }
}
