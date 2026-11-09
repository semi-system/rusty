//! The sans-I/O session state machine: bytes and answers in, actions out. No sockets, no clock,
//! no database. The "driver" here is a scripted simulator.

use std::net::IpAddr;

use rusty_core::AccountId;
use rusty_smtp::reply::Reply;
use rusty_smtp::session::{Action, Answer, PeerFacts, Phase, RcptVerdict, Recipient, Session, SessionConfig, Transaction};

fn cfg() -> SessionConfig {
    SessionConfig {
        hostname: "mx.example.net".into(),
        max_message_bytes: 10_000,
        max_recipients: 3,
        max_accounts: 1000,
        max_transactions: 5,
        max_commands_without_transaction: 100,
        max_errors: 20,
        max_line: 1000,
        starttls_available: true,
        submission: false,
    }
}

fn peer(tls: bool) -> PeerFacts {
    PeerFacts { ip: "192.0.2.7".parse::<IpAddr>().unwrap(), tls }
}

/// A scripted driver: answers `CheckRcpt` from a table and every `Deliver` with 250.
struct Sim {
    s: Session,
    out: String,
    closed: bool,
    starttls: usize,
    delivered: Vec<Transaction>,
    checks: Vec<String>,
    /// local-part -> verdict; anything else is "relay denied".
    deliver_reply: Option<Reply>,
}

impl Sim {
    fn with(cfg: SessionConfig, tls: bool) -> Sim {
        let (s, actions) = Session::new(cfg, peer(tls));
        let mut sim = Sim { s, out: String::new(), closed: false, starttls: 0, delivered: vec![], checks: vec![], deliver_reply: None };
        sim.apply(actions);
        sim
    }

    fn new() -> Sim {
        Sim::with(cfg(), false)
    }

    fn apply(&mut self, actions: Vec<Action>) {
        let mut queue: std::collections::VecDeque<Action> = actions.into();
        while let Some(a) = queue.pop_front() {
            match a {
                Action::Reply(r) => self.out.push_str(&String::from_utf8_lossy(&r.render())),
                Action::Close => self.closed = true,
                Action::StartTls => self.starttls += 1,
                Action::Authenticate { .. } | Action::CheckSender { .. } => unreachable!("port 25 never authenticates"),
                Action::CheckRcpt { address } => {
                    self.checks.push(address.clone());
                    let verdict = match address.split('@').next().unwrap_or("") {
                        "alice" => RcptVerdict::Accept(Recipient { address: address.clone(), accounts: vec![AccountId(1)] }),
                        "bob" => RcptVerdict::Accept(Recipient { address: address.clone(), accounts: vec![AccountId(2)] }),
                        "carol" => RcptVerdict::Accept(Recipient { address: address.clone(), accounts: vec![AccountId(3)] }),
                        "nobody" => RcptVerdict::Reject(Reply::new(550, "5.1.1", "No such user")),
                        _ => RcptVerdict::Reject(Reply::new(554, "5.7.1", "Relay access denied")),
                    };
                    queue.extend(self.s.answer(Answer::Rcpt(verdict)));
                }
                Action::Deliver(t) => {
                    self.delivered.push(*t);
                    let r = self.deliver_reply.clone().unwrap_or_else(|| Reply::new(250, "2.0.0", "Ok: queued"));
                    queue.extend(self.s.answer(Answer::Delivery(r)));
                }
            }
        }
    }

    fn send(&mut self, bytes: &[u8]) -> String {
        self.out.clear();
        let actions = self.s.feed(bytes);
        self.apply(actions);
        std::mem::take(&mut self.out)
    }

    fn greeted(&mut self) {
        self.send(b"EHLO client.example.org\r\n");
    }
}

fn codes(out: &str) -> Vec<&str> {
    out.lines().map(|l| l.get(..3).unwrap_or("")).collect()
}

// backend-neutral: pure state machine, no database
#[test]
fn greeting_and_ehlo_capabilities() {
    let mut sim = Sim::new();
    assert_eq!(std::mem::take(&mut sim.out), "220 mx.example.net ESMTP\r\n");
    let out = sim.send(b"EHLO client.example.org\r\n");
    assert!(out.starts_with("250-mx.example.net"), "{out}");
    for cap in ["250-PIPELINING", "250-8BITMIME", "250-SMTPUTF8", "250-SIZE 10000", "250-ENHANCEDSTATUSCODES", "250 STARTTLS"] {
        assert!(out.contains(cap), "missing {cap}: {out}");
    }
    assert!(!out.contains("AUTH"), "port 25 never offers AUTH: {out}");
    assert!(!out.contains("CHUNKING"), "BDAT is not implemented: {out}");
    assert!(out.ends_with("\r\n") && out.lines().last().unwrap().starts_with("250 "), "{out}");
}

// backend-neutral: pure state machine, no database
#[test]
fn starttls_is_offered_only_when_it_can_work() {
    let mut c = cfg();
    c.starttls_available = false;
    let mut sim = Sim::with(c, false);
    assert!(!sim.send(b"EHLO x\r\n").contains("STARTTLS"));
    let mut sim = Sim::with(cfg(), true); // already inside TLS
    assert!(!sim.send(b"EHLO x\r\n").contains("STARTTLS"));
    assert!(sim.send(b"STARTTLS\r\n").starts_with("503 5.5.1"));
    let mut c = cfg();
    c.starttls_available = false;
    let mut sim = Sim::with(c, false);
    sim.greeted();
    assert!(sim.send(b"STARTTLS\r\n").starts_with("454 4.7.0"));
    assert_eq!(sim.starttls, 0);
}

// backend-neutral: pure state machine, no database
#[test]
fn helo_gets_a_plain_reply() {
    let mut sim = Sim::new();
    assert_eq!(sim.send(b"HELO client.example.org\r\n"), "250 mx.example.net\r\n");
}

// backend-neutral: pure state machine, no database
#[test]
fn commands_out_of_sequence_are_503() {
    let mut sim = Sim::new();
    assert!(sim.send(b"MAIL FROM:<a@b.c>\r\n").starts_with("503 5.5.1"));
    sim.greeted();
    assert!(sim.send(b"RCPT TO:<alice@example.net>\r\n").starts_with("503 5.5.1"));
    assert!(sim.send(b"DATA\r\n").starts_with("503 5.5.1"));
    assert!(sim.send(b"MAIL FROM:<a@b.c>\r\n").starts_with("250 2.1.0"));
    assert!(sim.send(b"MAIL FROM:<a@b.c>\r\n").starts_with("503 5.5.1"), "nested MAIL");
    assert!(sim.send(b"DATA\r\n").starts_with("503 5.5.1"), "DATA without a recipient");
    assert!(sim.checks.is_empty(), "no recipient lookup happened for out-of-sequence commands");
}

// backend-neutral: pure state machine, no database
#[test]
fn a_full_transaction_hands_over_the_exact_envelope_and_bytes() {
    let mut sim = Sim::new();
    sim.greeted();
    assert!(sim.send(b"MAIL FROM:<sender@remote.example> SIZE=100 BODY=8BITMIME\r\n").starts_with("250 2.1.0"));
    assert!(sim.send(b"RCPT TO:<alice@example.net>\r\n").starts_with("250 2.1.5"));
    assert!(sim.send(b"RCPT TO:<bob@example.net>\r\n").starts_with("250 2.1.5"));
    assert!(sim.send(b"DATA\r\n").starts_with("354 "));
    assert_eq!(sim.s.phase(), Phase::Data);
    let out = sim.send(b"Subject: hi\r\n\r\n..dot\r\n8bit \xc3\xa9\r\n.\r\n");
    assert!(out.starts_with("250 2.0.0"), "{out}");
    assert_eq!(sim.delivered.len(), 1);
    let t = &sim.delivered[0];
    assert_eq!((t.helo.as_str(), t.mail_from.as_str()), ("client.example.org", "sender@remote.example"));
    assert_eq!(t.recipients.iter().map(|r| r.address.as_str()).collect::<Vec<_>>(), ["alice@example.net", "bob@example.net"]);
    assert_eq!(t.data, b"Subject: hi\r\n\r\n.dot\r\n8bit \xc3\xa9\r\n");
    assert_eq!(t.declared_size, Some(100));
    assert!(!t.peer.tls);
    // ready for the next transaction
    assert_eq!(sim.s.phase(), Phase::Command);
    assert!(sim.send(b"MAIL FROM:<>\r\n").starts_with("250 2.1.0"));
}

// backend-neutral: pure state machine, no database
#[test]
fn the_null_sender_is_an_empty_string() {
    let mut sim = Sim::new();
    sim.greeted();
    sim.send(b"MAIL FROM:<>\r\nRCPT TO:<alice@example.net>\r\nDATA\r\n");
    sim.send(b"x\r\n.\r\n");
    assert_eq!(sim.delivered[0].mail_from, "");
}

// backend-neutral: pure state machine, no database
#[test]
fn pipelined_commands_wait_for_each_asynchronous_answer() {
    let mut s = Session::new(cfg(), peer(false)).0;
    s.feed(b"EHLO x\r\n");
    // the whole group arrives at once
    let actions = s.feed(b"MAIL FROM:<a@b.c>\r\nRCPT TO:<alice@example.net>\r\nRCPT TO:<bob@example.net>\r\nRSET\r\n");
    let checks: Vec<_> = actions.iter().filter(|a| matches!(a, Action::CheckRcpt { .. })).collect();
    assert_eq!(checks.len(), 1, "the second RCPT must wait for the first answer");
    assert_eq!(s.phase(), Phase::Waiting);
    // more input while waiting is held, not dropped
    assert!(s.feed(b"NOOP\r\n").is_empty());
    let more = s.answer(Answer::Rcpt(RcptVerdict::Accept(Recipient { address: "alice@example.net".into(), accounts: vec![AccountId(1)] })));
    assert!(more.iter().any(|a| matches!(a, Action::CheckRcpt { address } if address == "bob@example.net")));
    let rest = s.answer(Answer::Rcpt(RcptVerdict::Accept(Recipient { address: "bob@example.net".into(), accounts: vec![AccountId(2)] })));
    let replies: Vec<String> = rest
        .iter()
        .filter_map(|a| if let Action::Reply(r) = a { Some(String::from_utf8_lossy(&r.render()).into_owned()) } else { None })
        .collect();
    // RCPT ok, RSET ok, NOOP ok, in order
    assert_eq!(replies.iter().map(|r| &r[..3]).collect::<Vec<_>>(), ["250", "250", "250"]);
    assert_eq!(s.phase(), Phase::Command);
}

// backend-neutral: pure state machine, no database
#[test]
fn rejected_recipients_do_not_end_the_transaction() {
    let mut sim = Sim::new();
    sim.greeted();
    sim.send(b"MAIL FROM:<a@b.c>\r\n");
    assert!(sim.send(b"RCPT TO:<nobody@example.net>\r\n").starts_with("550 5.1.1"));
    assert!(sim.send(b"RCPT TO:<stranger@elsewhere.example>\r\n").starts_with("554 5.7.1"));
    assert!(sim.send(b"DATA\r\n").starts_with("554 5.5.1"), "no valid recipient");
    assert!(sim.send(b"RCPT TO:<alice@example.net>\r\n").starts_with("250"));
    assert!(sim.send(b"DATA\r\n").starts_with("354"));
}

// backend-neutral: pure state machine, no database
#[test]
fn the_recipient_cap_is_452_and_keeps_the_accepted_ones() {
    let mut sim = Sim::new();
    sim.greeted();
    sim.send(b"MAIL FROM:<a@b.c>\r\n");
    for who in ["alice", "bob", "carol"] {
        assert!(sim.send(format!("RCPT TO:<{who}@example.net>\r\n").as_bytes()).starts_with("250"));
    }
    let before = sim.checks.len();
    assert!(sim.send(b"RCPT TO:<alice@example.net>\r\n").starts_with("452 4.5.3"));
    assert_eq!(sim.checks.len(), before, "over the cap the lookup is not even made");
    sim.send(b"DATA\r\n");
    sim.send(b"x\r\n.\r\n");
    assert_eq!(sim.delivered[0].recipients.len(), 3);
}

// backend-neutral: pure state machine, no database
#[test]
fn a_declared_size_over_the_limit_is_refused_at_mail() {
    let mut sim = Sim::new();
    sim.greeted();
    assert!(sim.send(b"MAIL FROM:<a@b.c> SIZE=10001\r\n").starts_with("552 5.3.4"));
    assert!(sim.send(b"RCPT TO:<alice@example.net>\r\n").starts_with("503"), "no transaction was opened");
    assert!(sim.send(b"MAIL FROM:<a@b.c> SIZE=10000\r\n").starts_with("250"));
}

// backend-neutral: pure state machine, no database
#[test]
fn data_over_the_limit_is_552_and_closes_the_connection() {
    let mut sim = Sim::new();
    sim.greeted();
    sim.send(b"MAIL FROM:<a@b.c>\r\nRCPT TO:<alice@example.net>\r\nDATA\r\n");
    let big = vec![b'a'; 20_000];
    let out = sim.send(&big);
    assert!(out.starts_with("552 5.3.4"), "{out}");
    assert!(sim.closed);
    assert!(sim.delivered.is_empty());
    assert!(sim.send(b"NOOP\r\n").is_empty(), "nothing is processed after close");
}

// backend-neutral: pure state machine, no database
#[test]
fn nul_bytes_in_data_are_a_permanent_rejection_and_the_session_goes_on() {
    let mut sim = Sim::new();
    sim.greeted();
    sim.send(b"MAIL FROM:<a@b.c>\r\nRCPT TO:<alice@example.net>\r\nDATA\r\n");
    let out = sim.send(b"a\x00b\r\n.\r\n");
    assert!(out.starts_with("554 5.6.0"), "{out}");
    assert!(sim.delivered.is_empty() && !sim.closed);
    assert!(sim.send(b"MAIL FROM:<a@b.c>\r\n").starts_with("250"), "the transaction was reset");
}

// backend-neutral: pure state machine, no database
#[test]
fn a_failed_delivery_reply_is_passed_through_and_resets_the_transaction() {
    let mut sim = Sim::new();
    sim.deliver_reply = Some(Reply::new(451, "4.3.0", "Temporary local problem"));
    sim.greeted();
    sim.send(b"MAIL FROM:<a@b.c>\r\nRCPT TO:<alice@example.net>\r\nDATA\r\n");
    let out = sim.send(b"x\r\n.\r\n");
    assert!(out.starts_with("451 4.3.0"), "{out}");
    assert!(sim.send(b"MAIL FROM:<a@b.c>\r\n").starts_with("250"), "the sender retries with a fresh transaction");
}

// backend-neutral: pure state machine, no database
#[test]
fn rset_noop_vrfy_expn_help_quit() {
    let mut sim = Sim::new();
    sim.greeted();
    sim.send(b"MAIL FROM:<a@b.c>\r\nRCPT TO:<alice@example.net>\r\n");
    assert!(sim.send(b"RSET\r\n").starts_with("250 2.0.0"));
    assert!(sim.send(b"DATA\r\n").starts_with("503"));
    assert!(sim.send(b"NOOP\r\n").starts_with("250 2.0.0"));
    assert!(sim.send(b"VRFY alice\r\n").starts_with("252 2.5.2"), "VRFY never discloses");
    assert!(sim.send(b"EXPN list\r\n").starts_with("502 5.5.1"));
    assert!(sim.send(b"HELP\r\n").starts_with("214 "));
    assert!(sim.send(b"QUIT\r\n").starts_with("221 2.0.0"));
    assert!(sim.closed);
    assert!(sim.send(b"NOOP\r\n").is_empty());
}

// backend-neutral: pure state machine, no database
#[test]
fn auth_and_bdat_are_not_available_on_port_25() {
    let mut sim = Sim::new();
    sim.greeted();
    assert!(sim.send(b"AUTH PLAIN AGFAYg==\r\n").starts_with("503 5.5.1"));
    assert!(sim.send(b"BDAT 10 LAST\r\n").starts_with("502 5.5.1"));
}

// backend-neutral: pure state machine, no database
#[test]
fn syntax_errors_unknown_commands_and_long_lines() {
    let mut sim = Sim::new();
    sim.greeted();
    assert!(sim.send(b"FROB\r\n").starts_with("500 5.5.2"));
    assert!(sim.send(b"MAIL FROM:garbage\r\n").starts_with("501 5.5."));
    assert!(sim.send(b"MAIL FROM:<a@b.c> BODY=BINARYMIME\r\n").starts_with("555 5.5.4"));
    let long = format!("NOOP {}\r\n", "x".repeat(2000));
    assert!(sim.send(long.as_bytes()).starts_with("500 5.5.2"));
    assert!(sim.send(b"NOOP\r\n").starts_with("250"), "the session survives a long line");
    assert!(sim.send(b"EHLO a\x00b\r\n").starts_with("501 5.5."));
}

// backend-neutral: pure state machine, no database
#[test]
fn too_many_errors_end_the_session() {
    let mut c = cfg();
    c.max_errors = 3;
    let mut sim = Sim::with(c, false);
    sim.greeted();
    sim.send(b"FROB\r\nFROB\r\n");
    let out = sim.send(b"FROB\r\nFROB\r\n");
    assert!(out.contains("421 4.7.0"), "{out}");
    assert!(sim.closed);
}

// backend-neutral: pure state machine, no database
#[test]
fn transaction_and_idle_command_caps() {
    let mut c = cfg();
    c.max_transactions = 2;
    let mut sim = Sim::with(c, false);
    sim.greeted();
    for _ in 0..2 {
        assert!(sim.send(b"MAIL FROM:<a@b.c>\r\n").starts_with("250"));
        sim.send(b"RSET\r\n");
    }
    assert!(sim.send(b"MAIL FROM:<a@b.c>\r\n").starts_with("421 4.7.0"));
    assert!(sim.closed);

    let mut c = cfg();
    c.max_commands_without_transaction = 3;
    let mut sim = Sim::with(c, false);
    sim.greeted();
    sim.send(b"NOOP\r\nNOOP\r\n");
    let out = sim.send(b"NOOP\r\nNOOP\r\n");
    assert!(out.contains("421 4.7.0"), "{out}");
    assert!(sim.closed);
}

// backend-neutral: pure state machine, no database
#[test]
fn starttls_resets_the_session_per_rfc_3207() {
    let mut sim = Sim::new();
    sim.greeted();
    sim.send(b"MAIL FROM:<a@b.c>\r\n");
    let out = sim.send(b"STARTTLS\r\n");
    assert!(out.starts_with("220 2.0.0"), "{out}");
    assert_eq!(sim.starttls, 1);
    let actions = sim.s.tls_started();
    sim.apply(actions);
    // everything learned before the handshake is forgotten: EHLO again, the transaction is gone
    assert!(sim.send(b"MAIL FROM:<a@b.c>\r\n").starts_with("503 5.5.1"));
    let ehlo = sim.send(b"EHLO client.example.org\r\n");
    assert!(!ehlo.contains("STARTTLS"), "no second STARTTLS: {ehlo}");
    sim.send(b"MAIL FROM:<a@b.c>\r\nRCPT TO:<alice@example.net>\r\nDATA\r\nx\r\n.\r\n");
    assert!(sim.delivered[0].peer.tls, "the transaction knows it ran inside TLS");
}

// backend-neutral: pure state machine, no database
#[test]
fn bytes_pipelined_after_starttls_close_the_connection() {
    // plaintext injected behind STARTTLS must never be processed after the handshake (CVE-2011-0411 class)
    let mut sim = Sim::new();
    sim.greeted();
    let out = sim.send(b"STARTTLS\r\nMAIL FROM:<evil@x.y>\r\n");
    assert!(out.contains("554 5.5.0") || out.contains("503"), "{out}");
    assert!(sim.closed);
    assert_eq!(sim.starttls, 0, "no handshake is started over a poisoned buffer");
}

// backend-neutral: pure state machine, no database
#[test]
fn shutdown_and_timeout_say_421_and_close() {
    let mut sim = Sim::new();
    let a = sim.s.shutdown();
    sim.apply(a);
    assert!(sim.out.contains("421 4.3.2") && sim.closed);
    let mut sim = Sim::new();
    let a = sim.s.timed_out();
    sim.apply(a);
    assert!(sim.out.contains("421 4.4.2") && sim.closed);
}

// backend-neutral: pure state machine, no database
#[test]
fn no_reply_leaks_more_than_the_status_and_fixed_text() {
    let mut sim = Sim::new();
    sim.greeted();
    // hostile input is never echoed back
    let out = sim.send(b"FROB-secret-token-123\r\n");
    assert!(!out.contains("secret-token-123"), "{out}");
    let out = sim.send(b"MAIL FROM:<secret-token-456>\r\n");
    assert!(!out.contains("secret-token-456"), "{out}");
    assert_eq!(codes(&out).len(), out.lines().count());
}

proptest::proptest! {
    // backend-neutral: the session must never panic or grow without bound on arbitrary input
    #[test]
    fn arbitrary_input_never_panics(
        chunks in proptest::collection::vec(proptest::collection::vec(proptest::prelude::any::<u8>(), 0..300), 0..10)
    ) {
        let mut sim = Sim::new();
        for c in &chunks {
            sim.send(c);
        }
    }

    // backend-neutral: a plausible-command fuzz walks the real transitions
    #[test]
    fn plausible_command_soup_never_panics(
        cmds in proptest::collection::vec(
            proptest::prop_oneof![
                proptest::strategy::Just("EHLO x\r\n".to_string()),
                proptest::strategy::Just("MAIL FROM:<a@b.c>\r\n".to_string()),
                proptest::strategy::Just("RCPT TO:<alice@example.net>\r\n".to_string()),
                proptest::strategy::Just("DATA\r\n".to_string()),
                proptest::strategy::Just("x\r\n.\r\n".to_string()),
                proptest::strategy::Just("RSET\r\n".to_string()),
                proptest::strategy::Just("STARTTLS\r\n".to_string()),
                proptest::strategy::Just("QUIT\r\n".to_string()),
            ], 0..40)
    ) {
        let mut sim = Sim::new();
        for c in &cmds {
            sim.send(c.as_bytes());
        }
    }
}

// backend-neutral: pure state machine, no database
#[test]
fn the_advertised_recipient_count_is_really_accepted_with_default_caps() {
    // review I3: RCPT and MAIL used to count against the "commands without a transaction" cap, so with the
    // defaults (100 recipients, 100 commands) the 99th RCPT closed the connection
    let mut c = cfg();
    c.max_recipients = 100;
    c.max_commands_without_transaction = 100;
    c.max_errors = 20;
    c.max_transactions = 5;
    let mut sim = Sim::with(c, false);
    sim.greeted();
    sim.send(b"MAIL FROM:<a@b.c>\r\n");
    for n in 1..=100 {
        let out = sim.send(b"RCPT TO:<alice@example.net>\r\n");
        assert!(out.starts_with("250 2.1.5"), "recipient {n}: {out}");
    }
    assert!(sim.send(b"RCPT TO:<alice@example.net>\r\n").starts_with("452 4.5.3"));
    assert!(!sim.closed);
    assert!(sim.send(b"DATA\r\n").starts_with("354"));
    assert!(sim.send(b"x\r\n.\r\n").starts_with("250"));
    assert_eq!(sim.delivered[0].recipients.len(), 100);
}

// backend-neutral: pure state machine, no database
#[test]
fn tls_started_forgets_the_greeting_and_the_open_transaction_exactly() {
    // review: a leaked txn also answered 503 ("Nested MAIL"), so the reset was never really probed
    let mut sim = Sim::new();
    sim.greeted();
    sim.send(b"MAIL FROM:<a@b.c>\r\nRCPT TO:<alice@example.net>\r\n");
    sim.send(b"STARTTLS\r\n");
    let a = sim.s.tls_started();
    sim.apply(a);
    // the greeting is forgotten: the refusal is about EHLO, not about a nested transaction
    let out = sim.send(b"MAIL FROM:<a@b.c>\r\n");
    assert!(out.contains("Send EHLO or HELO first"), "{out}");
    sim.greeted();
    // the transaction is gone: RCPT has no MAIL to belong to
    let out = sim.send(b"RCPT TO:<alice@example.net>\r\n");
    assert!(out.contains("Need MAIL command first"), "{out}");
    // and a fresh MAIL is not "nested"
    assert!(sim.send(b"MAIL FROM:<a@b.c>\r\n").starts_with("250 2.1.0"));
}

// backend-neutral: pure state machine, no database
#[test]
fn a_message_may_reach_only_so_many_distinct_mailboxes() {
    let mut c = cfg();
    c.max_accounts = 2;
    let mut sim = Sim::with(c, false);
    sim.greeted();
    sim.send(b"MAIL FROM:<a@b.c>\r\n");
    assert!(sim.send(b"RCPT TO:<alice@example.net>\r\n").starts_with("250"));
    assert!(sim.send(b"RCPT TO:<alice@example.net>\r\n").starts_with("250"), "the same mailbox again is free");
    assert!(sim.send(b"RCPT TO:<bob@example.net>\r\n").starts_with("250"));
    assert!(sim.send(b"RCPT TO:<carol@example.net>\r\n").starts_with("452 4.5.3"));
    assert!(sim.send(b"DATA\r\n").starts_with("354"));
}
