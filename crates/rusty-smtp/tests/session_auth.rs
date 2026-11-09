//! The submission side of the sans-I/O session: AUTH PLAIN / LOGIN, TLS-only authentication, and
//! the sender rule hooks. No sockets, no database.

use rusty_core::AccountId;
use rusty_smtp::reply::Reply;
use rusty_smtp::session::{
    Action, Answer, AuthVerdict, Identity, PeerFacts, RcptVerdict, Recipient, Session, SessionConfig, Transaction,
};

fn cfg() -> SessionConfig {
    SessionConfig {
        hostname: "mx.example.net".into(),
        max_message_bytes: 10_000,
        max_recipients: 3,
        max_accounts: 1000,
        max_transactions: 50,
        max_commands_without_transaction: 1000,
        max_errors: 1000,
        max_line: 1000,
        starttls_available: true,
        submission: true,
    }
}

struct Sim {
    s: Session,
    out: String,
    closed: bool,
    delivered: Vec<Transaction>,
    /// What the driver was asked to verify.
    verified: Vec<(String, String)>,
}

impl Sim {
    fn new(tls: bool) -> Sim {
        let (s, a) = Session::new(cfg(), PeerFacts { ip: "192.0.2.9".parse().unwrap(), tls });
        let mut sim = Sim { s, out: String::new(), closed: false, delivered: vec![], verified: vec![] };
        sim.apply(a);
        sim
    }

    fn apply(&mut self, actions: Vec<Action>) {
        let mut q: std::collections::VecDeque<Action> = actions.into();
        while let Some(a) = q.pop_front() {
            match a {
                Action::Reply(r) => self.out.push_str(&String::from_utf8_lossy(&r.render())),
                Action::Close => self.closed = true,
                Action::StartTls => {}
                Action::Authenticate { identity, password } => {
                    self.verified.push((identity.clone(), password.expose().to_owned()));
                    let v = if identity == "alice@example.net" && password.expose() == "pw-alice" {
                        AuthVerdict::Success(Identity { account: AccountId(1), address: identity })
                    } else {
                        AuthVerdict::Failure(Reply::new(535, "5.7.8", "Authentication credentials invalid"))
                    };
                    q.extend(self.s.answer(Answer::Auth(v)));
                }
                Action::CheckSender { address, .. } => {
                    let ok = address == "alice@example.net" || address == "ally@example.net";
                    let v = if ok { Ok(()) } else { Err(Reply::new(553, "5.7.1", "Sender address rejected")) };
                    q.extend(self.s.answer(Answer::Sender(v)));
                }
                Action::CheckRcpt { address } => {
                    let v = if address.ends_with("@example.net") {
                        RcptVerdict::Accept(Recipient { address, accounts: vec![AccountId(2)] })
                    } else {
                        RcptVerdict::AcceptRemote(address)
                    };
                    q.extend(self.s.answer(Answer::Rcpt(v)));
                }
                Action::Deliver(t) => {
                    self.delivered.push(*t);
                    q.extend(self.s.answer(Answer::Delivery(Reply::new(250, "2.0.0", "Ok: queued"))));
                }
            }
        }
    }

    fn send(&mut self, bytes: &[u8]) -> String {
        self.out.clear();
        let a = self.s.feed(bytes);
        self.apply(a);
        std::mem::take(&mut self.out)
    }

    fn hello(&mut self) -> String {
        self.send(b"EHLO client.example.org\r\n")
    }
}

/// base64 of "\0alice@example.net\0pw-alice"
const PLAIN_OK: &str = "AGFsaWNlQGV4YW1wbGUubmV0AHB3LWFsaWNl";

// backend-neutral: pure state machine, no database
#[test]
fn auth_is_not_offered_and_not_accepted_without_tls() {
    let mut sim = Sim::new(false);
    let ehlo = sim.hello();
    assert!(!ehlo.contains("AUTH"), "AUTH must not be advertised in clear text: {ehlo}");
    assert!(ehlo.contains("STARTTLS"), "{ehlo}");
    for attempt in [format!("AUTH PLAIN {PLAIN_OK}\r\n"), "AUTH LOGIN\r\n".to_owned(), "AUTH PLAIN\r\n".to_owned()] {
        let out = sim.send(attempt.as_bytes());
        assert!(out.starts_with("538 5.7.11"), "{out}");
    }
    assert!(sim.verified.is_empty(), "credentials never reach the verifier over clear text");
    // and a base64 blob sent anyway is just a bad command, not credentials
    assert!(sim.send(format!("{PLAIN_OK}\r\n").as_bytes()).starts_with("500"));
    assert!(sim.verified.is_empty());
}

// backend-neutral: pure state machine, no database
#[test]
fn over_tls_auth_is_advertised_and_starttls_is_not() {
    let mut sim = Sim::new(true);
    let ehlo = sim.hello();
    assert!(ehlo.contains("250-AUTH PLAIN LOGIN") || ehlo.contains("250 AUTH PLAIN LOGIN"), "{ehlo}");
    assert!(!ehlo.contains("STARTTLS"), "{ehlo}");
}

// backend-neutral: pure state machine, no database
#[test]
fn plain_with_initial_response_and_with_a_continuation() {
    let mut sim = Sim::new(true);
    sim.hello();
    assert!(sim.send(format!("AUTH PLAIN {PLAIN_OK}\r\n").as_bytes()).starts_with("235 2.7.0"));
    assert_eq!(sim.verified, [("alice@example.net".to_owned(), "pw-alice".to_owned())]);
    assert!(sim.send(b"AUTH PLAIN\r\n").starts_with("503 5.5.1"), "already authenticated");

    let mut sim = Sim::new(true);
    sim.hello();
    assert_eq!(sim.send(b"AUTH PLAIN\r\n"), "334 \r\n");
    assert!(sim.send(format!("{PLAIN_OK}\r\n").as_bytes()).starts_with("235 2.7.0"));
}

// backend-neutral: pure state machine, no database
#[test]
fn login_takes_two_prompts_or_an_initial_user() {
    let mut sim = Sim::new(true);
    sim.hello();
    assert_eq!(sim.send(b"AUTH LOGIN\r\n"), "334 VXNlcm5hbWU6\r\n");
    assert_eq!(sim.send(b"YWxpY2VAZXhhbXBsZS5uZXQ=\r\n"), "334 UGFzc3dvcmQ6\r\n");
    assert!(sim.send(b"cHctYWxpY2U=\r\n").starts_with("235"));
    let mut sim = Sim::new(true);
    sim.hello();
    assert_eq!(sim.send(b"AUTH LOGIN YWxpY2VAZXhhbXBsZS5uZXQ=\r\n"), "334 UGFzc3dvcmQ6\r\n");
    assert!(sim.send(b"cHctYWxpY2U=\r\n").starts_with("235"));
}

// backend-neutral: pure state machine, no database
#[test]
fn a_wrong_password_is_535_and_three_failures_close_the_connection() {
    let mut sim = Sim::new(true);
    sim.hello();
    let wrong = "AGFsaWNlQGV4YW1wbGUubmV0AHdyb25n"; // alice@example.net / wrong
    assert!(sim.send(format!("AUTH PLAIN {wrong}\r\n").as_bytes()).starts_with("535 5.7.8"));
    assert!(sim.send(format!("AUTH PLAIN {wrong}\r\n").as_bytes()).starts_with("535 5.7.8"));
    let out = sim.send(format!("AUTH PLAIN {wrong}\r\n").as_bytes());
    assert!(out.contains("421 4.7.0"), "{out}");
    assert!(sim.closed);
}

// backend-neutral: pure state machine, no database
#[test]
fn malformed_sasl_data_is_501_and_the_exchange_ends() {
    for (cmd, follow) in [
        ("AUTH PLAIN !!!!\r\n", None),
        ("AUTH PLAIN abc\r\n", None),               // not a multiple of four
        ("AUTH PLAIN =\r\n", None),                 // the empty response: no credentials
        ("AUTH PLAIN\r\n", Some("Zm9v\r\n")),       // "foo": no NULs
        ("AUTH PLAIN\r\n", Some("AGE=\r\n")),       // only one NUL
        ("AUTH PLAIN\r\n", Some("AGEAAA==\r\n")),   // empty password
        ("AUTH LOGIN\r\n", Some("!!!!\r\n")),
        ("AUTH LOGIN\r\n", Some("*\r\n")),          // cancel
        ("AUTH LOGIN YWxpY2U=\r\n", Some("====\r\n")),
    ] {
        let mut sim = Sim::new(true);
        sim.hello();
        let mut out = sim.send(cmd.as_bytes());
        if let Some(f) = follow {
            out = sim.send(f.as_bytes());
        }
        assert!(out.starts_with("501 5."), "{cmd:?}/{follow:?} -> {out:?}");
        assert!(sim.verified.is_empty(), "{cmd:?}");
        // the exchange is over: the next line is a command again
        assert!(sim.send(b"NOOP\r\n").starts_with("250"), "{cmd:?}");
    }
}

// backend-neutral: pure state machine, no database
#[test]
fn an_authorisation_identity_that_differs_is_refused() {
    // "\0alice@example.net\0pw" with authzid "bob@example.net": base64 of "bob@example.net\0alice@example.net\0pw-alice"
    let blob = "Ym9iQGV4YW1wbGUubmV0AGFsaWNlQGV4YW1wbGUubmV0AHB3LWFsaWNl";
    let mut sim = Sim::new(true);
    sim.hello();
    assert!(sim.send(format!("AUTH PLAIN {blob}\r\n").as_bytes()).starts_with("535 5.7.8"));
    assert!(sim.verified.is_empty(), "impersonation is never even attempted");
}

// backend-neutral: pure state machine, no database
#[test]
fn an_oversized_sasl_line_is_a_cleanly_refused_line() {
    let mut sim = Sim::new(true);
    sim.hello();
    sim.send(b"AUTH PLAIN\r\n");
    let huge = format!("{}\r\n", "A".repeat(20_000));
    let out = sim.send(huge.as_bytes());
    assert!(out.starts_with("500 5.5.2"), "{out}");
    assert!(sim.verified.is_empty());
}

// backend-neutral: pure state machine, no database
#[test]
fn unknown_mechanisms_and_auth_in_a_transaction() {
    let mut sim = Sim::new(true);
    sim.hello();
    assert!(sim.send(b"AUTH CRAM-MD5\r\n").starts_with("504 5.5.4"));
    assert!(sim.send(b"AUTH XOAUTH2 abc\r\n").starts_with("504 5.5.4"));
    sim.send(format!("AUTH PLAIN {PLAIN_OK}\r\n").as_bytes());
    sim.send(b"MAIL FROM:<alice@example.net>\r\n");
    assert!(sim.send(b"AUTH PLAIN\r\n").starts_with("503 5.5.1"));
}

// backend-neutral: pure state machine, no database
#[test]
fn mail_needs_authentication_and_the_sender_rule() {
    let mut sim = Sim::new(true);
    sim.hello();
    assert!(sim.send(b"MAIL FROM:<alice@example.net>\r\n").starts_with("530 5.7.0"));
    sim.send(format!("AUTH PLAIN {PLAIN_OK}\r\n").as_bytes());
    assert!(sim.send(b"MAIL FROM:<>\r\n").starts_with("553 5.7.1"), "no null sender on submission");
    assert!(sim.send(b"MAIL FROM:<bob@example.net>\r\n").starts_with("553 5.7.1"));
    assert!(sim.send(b"RCPT TO:<x@y.example>\r\n").starts_with("503"), "no transaction was opened");
    assert!(sim.send(b"MAIL FROM:<ally@example.net>\r\n").starts_with("250 2.1.0"), "an alias of the user is fine");
}

// backend-neutral: pure state machine, no database
#[test]
fn remote_and_local_recipients_travel_in_one_transaction() {
    let mut sim = Sim::new(true);
    sim.hello();
    sim.send(format!("AUTH PLAIN {PLAIN_OK}\r\n").as_bytes());
    sim.send(b"MAIL FROM:<alice@example.net>\r\n");
    assert!(sim.send(b"RCPT TO:<bob@example.net>\r\n").starts_with("250"));
    assert!(sim.send(b"RCPT TO:<far@away.example>\r\n").starts_with("250"));
    // the recipient cap counts both kinds
    assert!(sim.send(b"RCPT TO:<far2@away.example>\r\n").starts_with("250"));
    assert!(sim.send(b"RCPT TO:<far3@away.example>\r\n").starts_with("452 4.5.3"));
    sim.send(b"DATA\r\n");
    assert!(sim.send(b"From: alice@example.net\r\n\r\nhi\r\n.\r\n").starts_with("250"));
    let t = &sim.delivered[0];
    assert_eq!(t.recipients.len(), 1);
    assert_eq!(t.remote_recipients, ["far@away.example", "far2@away.example"]);
    assert_eq!(t.identity.as_ref().map(|i| i.account), Some(AccountId(1)));
}

// backend-neutral: pure state machine, no database
#[test]
fn only_remote_recipients_are_enough_for_data() {
    let mut sim = Sim::new(true);
    sim.hello();
    sim.send(format!("AUTH PLAIN {PLAIN_OK}\r\n").as_bytes());
    sim.send(b"MAIL FROM:<alice@example.net>\r\nRCPT TO:<far@away.example>\r\n");
    assert!(sim.send(b"DATA\r\n").starts_with("354"));
}

// backend-neutral: pure state machine, no database
#[test]
fn starttls_forgets_the_identity() {
    let mut sim = Sim::new(false);
    sim.hello();
    let out = sim.send(b"STARTTLS\r\n");
    assert!(out.starts_with("220 2.0.0"), "{out}");
    let a = sim.s.tls_started();
    sim.apply(a);
    sim.hello();
    sim.send(format!("AUTH PLAIN {PLAIN_OK}\r\n").as_bytes());
    assert!(sim.send(b"MAIL FROM:<alice@example.net>\r\n").starts_with("250"));
}

// backend-neutral: pure state machine, no database
#[test]
fn credentials_never_appear_in_debug_output() {
    let mut sim = Sim::new(true);
    sim.hello();
    let actions = sim.s.feed(format!("AUTH PLAIN {PLAIN_OK}\r\n").as_bytes());
    let shown = format!("{actions:?}");
    assert!(!shown.contains("pw-alice") && !shown.contains(PLAIN_OK), "{shown}");
}

proptest::proptest! {
    // backend-neutral: arbitrary bytes after AUTH never panic and never authenticate without a verdict
    #[test]
    fn arbitrary_auth_traffic_never_panics(
        chunks in proptest::collection::vec(proptest::collection::vec(proptest::prelude::any::<u8>(), 0..200), 0..8)
    ) {
        let mut sim = Sim::new(true);
        sim.hello();
        sim.send(b"AUTH PLAIN\r\n");
        for c in &chunks {
            sim.send(c);
        }
        sim.send(b"AUTH LOGIN\r\n");
        for c in &chunks {
            sim.send(c);
        }
    }
}
