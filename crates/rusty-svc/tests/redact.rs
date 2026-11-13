//! Log hygiene: a session that logs a line containing a password or an AUTH blob does so through
//! the redaction helper, and the captured log (text and JSON) never contains the secret.

use std::io::Write;
use std::sync::{Arc, Mutex};

use rusty_core::config::{LogFormat, LogLevel};
use rusty_svc::{redact_command, subscriber, Redacted};
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// The fake session: every client line is logged the way a real session must log it. The line
/// after a bare `AUTH LOGIN` is part of the exchange and is opaque.
fn fake_session(lines: &[&str]) {
    let mut in_auth = false;
    for l in lines {
        let shown = redact_command(l, in_auth);
        tracing::debug!(client_line = %shown, "command");
        in_auth = l.eq_ignore_ascii_case("AUTH LOGIN");
    }
}

const SECRETS: &[&str] = &["hunter2-s3cret", "dGVzdAB0ZXN0AGh1bnRlcjItczNjcmV0", "c2VjcmV0LXBhc3M="];

fn run(format: LogFormat) -> String {
    let cap = Capture::default();
    let sub = subscriber(LogLevel::Debug, format, cap.clone());
    tracing::subscriber::with_default(sub, || {
        fake_session(&[
            "EHLO client.example.org",
            "AUTH PLAIN dGVzdAB0ZXN0AGh1bnRlcjItczNjcmV0",
            "AUTH LOGIN",
            "c2VjcmV0LXBhc3M=",
            "a001 LOGIN alice hunter2-s3cret",
            "a002 AUTHENTICATE PLAIN dGVzdAB0ZXN0AGh1bnRlcjItczNjcmV0",
            "login \"alice\" \"hunter2-s3cret\"",
            "MAIL FROM:<a@example.org>",
        ]);
        tracing::info!(password = ?Redacted::new("hunter2-s3cret"), "debug of a wrapped secret");
    });
    cap.text()
}

#[test]
fn secrets_never_reach_the_log_text() {
    let out = run(LogFormat::Text);
    for s in SECRETS {
        assert!(!out.contains(s), "secret {s} leaked into: {out}");
    }
    // ordinary commands stay readable, the verb of a redacted one is kept for diagnosis
    assert!(out.contains("EHLO client.example.org"), "{out}");
    assert!(out.contains("MAIL FROM:<a@example.org>"), "{out}");
    assert!(out.contains("AUTH PLAIN"), "{out}");
    assert!(out.contains("LOGIN"), "{out}");
}

#[test]
fn secrets_never_reach_the_log_json() {
    let out = run(LogFormat::Json);
    for s in SECRETS {
        assert!(!out.contains(s), "secret {s} leaked into: {out}");
    }
    assert!(out.lines().all(|l| l.starts_with('{') && l.ends_with('}')), "not JSON lines: {out}");
}

#[test]
fn redaction_rules() {
    assert_eq!(redact_command("EHLO x.example", false), "EHLO x.example");
    assert_eq!(redact_command("AUTH PLAIN abc", false), "AUTH PLAIN <redacted>");
    assert_eq!(redact_command("auth login", false), "auth login");
    assert_eq!(redact_command("a1 LOGIN u p", false), "a1 LOGIN <redacted>");
    assert_eq!(redact_command("a1 login u p", false), "a1 login <redacted>");
    assert_eq!(
        redact_command("a1 AUTHENTICATE PLAIN xyz", false),
        "a1 AUTHENTICATE PLAIN <redacted>"
    );
    assert_eq!(redact_command("PASS secret", false), "PASS <redacted>");
    // Anything inside an authentication exchange is opaque, whatever it looks like.
    assert_eq!(redact_command("anything at all", true), "<redacted>");
    // Control characters cannot forge log lines.
    assert!(!redact_command("EHLO a\r\nFAKE LOG LINE", false).contains('\n'));
    // Very long lines are cut.
    assert!(redact_command(&"x".repeat(100_000), false).len() < 1_100);
}

#[test]
fn redacted_wrapper_hides_the_value_in_debug_and_display() {
    let r = Redacted::new("hunter2-s3cret");
    assert!(!format!("{r:?}").contains("hunter2"));
    assert!(!format!("{r}").contains("hunter2"));
}
