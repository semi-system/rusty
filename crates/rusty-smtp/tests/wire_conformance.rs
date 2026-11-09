//! Wire-level SMTP conformance, independent of any parsing library: byte-for-byte scripted
//! dialogues and a command corpus, driven through the session by BYTES ONLY (the `Session::feed`
//! boundary plus a scripted answerer). A future hand-written command parser must pass this file
//! unchanged. Nothing here may name a third-party protocol library (scripts/check-wrapped-libs.sh).

use proptest::prelude::*;
use rusty_core::AccountId;
use rusty_smtp::reply::Reply;
use rusty_smtp::session::{Action, Answer, PeerFacts, RcptVerdict, Recipient, Session, SessionConfig};

fn cfg() -> SessionConfig {
    SessionConfig {
        hostname: "mx.example.net".into(),
        max_message_bytes: 10_000,
        max_recipients: 10,
        max_transactions: 50,
        max_commands_without_transaction: 1000,
        max_errors: 1000,
        max_line: 1000,
        starttls_available: false,
    }
}

/// The server's bytes for a client's bytes, answering lookups and deliveries from a fixed script.
struct Wire {
    s: Session,
    out: Vec<u8>,
}

impl Wire {
    fn new() -> Wire {
        let (s, a) = Session::new(cfg(), PeerFacts { ip: "192.0.2.1".parse().unwrap(), tls: false });
        let mut w = Wire { s, out: Vec::new() };
        w.run(a);
        w
    }

    fn run(&mut self, actions: Vec<Action>) {
        let mut q: std::collections::VecDeque<Action> = actions.into();
        while let Some(a) = q.pop_front() {
            match a {
                Action::Reply(r) => self.out.extend(r.render()),
                Action::CheckRcpt { address } => {
                    let v = match address.split('@').next().unwrap_or("") {
                        "alice" => RcptVerdict::Accept(Recipient { address, accounts: vec![AccountId(1)] }),
                        "nobody" => RcptVerdict::Reject(Reply::new(550, "5.1.1", "No such user here")),
                        _ => RcptVerdict::Reject(Reply::new(554, "5.7.1", "Relay access denied")),
                    };
                    q.extend(self.s.answer(Answer::Rcpt(v)));
                }
                Action::Deliver(_) => q.extend(self.s.answer(Answer::Delivery(Reply::new(250, "2.0.0", "Ok: queued")))),
                Action::StartTls | Action::Close => {}
            }
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        let a = self.s.feed(bytes);
        self.run(a);
    }

    fn take(&mut self) -> String {
        String::from_utf8(std::mem::take(&mut self.out)).unwrap()
    }
}

const DIALOGUE: &[(&str, &str)] = &[
    ("", "220 mx.example.net ESMTP\r\n"),
    (
        "EHLO client.example.org\r\n",
        "250-mx.example.net\r\n250-PIPELINING\r\n250-8BITMIME\r\n250-SMTPUTF8\r\n250-SIZE 10000\r\n250 ENHANCEDSTATUSCODES\r\n",
    ),
    ("MAIL FROM:<sender@remote.example> SIZE=100\r\n", "250 2.1.0 Sender ok\r\n"),
    ("RCPT TO:<alice@example.net>\r\n", "250 2.1.5 Recipient ok\r\n"),
    ("RCPT TO:<nobody@example.net>\r\n", "550 5.1.1 No such user here\r\n"),
    ("RCPT TO:<x@elsewhere.example>\r\n", "554 5.7.1 Relay access denied\r\n"),
    ("DATA\r\n", "354 End data with <CR><LF>.<CR><LF>\r\n"),
    ("Subject: t\r\n\r\n..stuffed\r\n.\r\n", "250 2.0.0 Ok: queued\r\n"),
    ("RSET\r\n", "250 2.0.0 Ok\r\n"),
    ("NOOP\r\n", "250 2.0.0 Ok\r\n"),
    ("QUIT\r\n", "221 2.0.0 Bye\r\n"),
];

// backend-neutral: byte-level session dialogue, no database
#[test]
fn the_reference_dialogue_is_byte_exact() {
    let mut w = Wire::new();
    for (i, (send, expect)) in DIALOGUE.iter().enumerate() {
        w.send(send.as_bytes());
        assert_eq!(w.take(), *expect, "step {i}: after {send:?}");
    }
}

// backend-neutral: byte-level session dialogue, no database
#[test]
fn the_dialogue_is_the_same_when_sent_in_one_piece_or_byte_by_byte() {
    let all: String = DIALOGUE.iter().map(|(s, _)| *s).collect();
    let expect: String = DIALOGUE.iter().map(|(_, e)| *e).collect();
    let mut once = Wire::new();
    once.send(all.as_bytes());
    assert_eq!(once.take(), expect);
    let mut bytes = Wire::new();
    for b in all.as_bytes() {
        bytes.send(std::slice::from_ref(b));
    }
    assert_eq!(bytes.take(), expect);
}

/// (client line, status code and enhanced status that must start the reply)
const CORPUS: &[(&str, &str)] = &[
    ("HELO x", "250 "),
    ("ehlo x", "250-"),
    ("EhLo x", "250-"),
    ("FROB", "500 5.5.2"),
    ("", "500 5.5.2"),
    ("EHLO", "501 5.5.4"),
    ("MAIL FROM:garbage", "501 5.5."),
    ("MAIL", "501 5.5.4"),
    ("VRFY x", "252 2.5.2"),
    ("EXPN x", "502 5.5.1"),
    ("HELP", "214 "),
    ("NOOP", "250 2.0.0"),
    ("NOOP with args", "250 2.0.0"),
    ("RSET", "250 2.0.0"),
    ("STARTTLS", "454 4.7.0"),
    ("AUTH PLAIN AGFAYg==", "503 5.5.1"),
    ("BDAT 5 LAST", "502 5.5.1"),
    ("DATA", "503 5.5.1"),
    ("RCPT TO:<alice@example.net>", "503 5.5.1"),
    ("MAIL FROM:<a@b.c> BODY=BINARYMIME", "555 5.5.4"),
    ("MAIL FROM:<a@b.c> SIZE=99999", "552 5.3.4"),
    ("MAIL FROM:<a@b.c> SIZE=100 BODY=8BITMIME SMTPUTF8", "250 2.1.0"),
    ("MAIL FROM:<>", "250 2.1.0"),
];

// backend-neutral: byte-level session dialogue, no database
#[test]
fn every_corpus_line_gets_its_status_after_a_greeting() {
    for (line, expect) in CORPUS {
        let mut w = Wire::new();
        w.send(b"EHLO c.example\r\n");
        w.take();
        w.send(format!("{line}\r\n").as_bytes());
        let out = w.take();
        assert!(out.starts_with(expect), "{line:?} -> {out:?}, wanted {expect:?}");
        assert!(out.ends_with("\r\n"));
        for l in out.lines() {
            assert!(l.len() > 3 && l.as_bytes()[..3].iter().all(u8::is_ascii_digit), "bad reply line {l:?}");
        }
    }
}

// backend-neutral: byte-level session dialogue, no database
#[test]
fn bare_lf_line_endings_are_accepted_for_commands_and_the_transaction_still_works() {
    let mut w = Wire::new();
    w.send(b"EHLO c.example\nMAIL FROM:<a@b.c>\nRCPT TO:<alice@example.net>\nDATA\n");
    let out = w.take();
    assert!(out.contains("354 "), "{out}");
}

proptest! {
    // backend-neutral: any chunking of the same client bytes yields the same server bytes
    #[test]
    fn chunking_never_changes_the_server_output(cuts in proptest::collection::vec(1usize..40, 1..30)) {
        let all: String = DIALOGUE.iter().map(|(s, _)| *s).collect();
        let mut once = Wire::new();
        once.send(all.as_bytes());
        let expected = once.take();
        let mut w = Wire::new();
        let mut rest = all.as_bytes();
        let mut i = 0;
        while !rest.is_empty() {
            let n = cuts[i % cuts.len()].min(rest.len());
            let (a, b) = rest.split_at(n);
            w.send(a);
            rest = b;
            i += 1;
        }
        prop_assert_eq!(w.take(), expected);
    }

    // backend-neutral: every reply is well-formed whatever the client sends
    #[test]
    fn every_reply_is_well_formed(chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..120), 0..8)) {
        let mut w = Wire::new();
        for c in &chunks {
            w.send(c);
        }
        let out = w.out.clone();
        let text = String::from_utf8_lossy(&out);
        for l in text.split("\r\n").filter(|l| !l.is_empty()) {
            let b = l.as_bytes();
            prop_assert!(b.len() >= 4 && b[..3].iter().all(u8::is_ascii_digit) && (b[3] == b' ' || b[3] == b'-'), "{:?}", l);
        }
    }
}
