//! Entry points shared by the fuzz targets (`fuzz/fuzz_targets/fuzz_pop3_*.rs`) and their stable-toolchain stand-ins (`tests/fuzz_smoke.rs`): the same invariants whoever drives them.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use bytes::Bytes;

use crate::command::{parse, Command, MAX_CREDENTIAL, MAX_NUMBER};
use crate::session::{Backend, Entry, FetchError, Fetched, LoginError, Session, Transport};

/// Run a future that never waits (every backend below answers at once).
fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// The parser is total: it never panics, accepts only a clean line, and an accepted command carries values inside their bounds.
pub fn command(data: &[u8]) {
    match parse(data) {
        Err(_) => {}
        Ok(c) => {
            assert!(data.iter().all(|b| *b >= 0x20 && *b != 0x7f), "a control character was accepted");
            assert!(std::str::from_utf8(data).is_ok(), "a line that is not UTF-8 was accepted");
            let numbers: Vec<u32> = match &c {
                Command::List(Some(n)) | Command::Uidl(Some(n)) | Command::Retr(n) | Command::Dele(n) => vec![*n],
                Command::Top(a, b) => vec![*a, *b],
                _ => Vec::new(),
            };
            assert!(numbers.iter().all(|n| *n <= MAX_NUMBER));
            if let Command::User(s) | Command::Pass(s) = &c {
                assert!(!s.is_empty() && s.len() <= MAX_CREDENTIAL);
            }
        }
    }
}

const SECRET_MARKER: &str = "correct-horse";

struct Fixture {
    removed: usize,
}

#[async_trait]
impl Backend for Fixture {
    async fn login(&mut self, user: &str, password: &str) -> Result<(), LoginError> {
        if user == "alice" && password == SECRET_MARKER {
            Ok(())
        } else {
            Err(LoginError::Bad)
        }
    }
    async fn snapshot(&mut self) -> Result<Vec<Entry>, ()> {
        Ok((1..=3).map(|i| Entry { id: i, uid: format!("1.{i}"), size: 40 }).collect())
    }
    async fn retrieve(&mut self, id: i64, _size: u64) -> Result<Fetched, FetchError> {
        match id {
            1 => Ok(Fetched::plain(Bytes::from_static(b"Subject: a\r\n\r\n.dot\r\nbody\r\n"))),
            2 => Ok(Fetched::plain(Bytes::from_static(b"Subject: b\n\nno final newline"))),
            3 => Err(FetchError::Gone),
            _ => Err(FetchError::Unavailable),
        }
    }
    async fn expunge(&mut self, ids: &[i64]) -> Result<(), ()> {
        self.removed += ids.len();
        Ok(())
    }
}

/// A whole session driven by arbitrary bytes, split into lines at LF: it never panics, every reply line is CRLF-terminated ASCII, the output is bounded by what was sent, a secret the client sent is
/// never echoed, and nothing is removed unless a `QUIT` was understood after a sign-in.
pub fn session(data: &[u8]) {
    for transport in [Transport::Tls, Transport::Plain { stls: true }] {
        let mut s = Session::new(transport, false);
        let mut b = Fixture { removed: 0 };
        let mut out = Vec::new();
        let mut quit_after_login = false;
        let mut sent = 0usize;
        for line in data.split(|c| *c == b'\n').take(200) {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            sent += line.len() + 2;
            let was_in = s.is_authenticated();
            let step = block_on(s.line(line, &mut b, &mut out));
            if was_in && line.eq_ignore_ascii_case(b"QUIT") {
                quit_after_login = true;
            }
            if step != crate::session::Step::Continue {
                break;
            }
        }
        assert!(out.is_empty() || out.ends_with(b"\r\n"), "a reply does not end with CRLF");
        assert!(out.is_ascii(), "a reply holds a byte outside ASCII");
        assert!(!out.windows(2).any(|w| w.get(1) == Some(&b'\n') && w.first() != Some(&b'\r')), "a bare LF in a reply");
        // Each line sent can draw a reply of a few hundred bytes at most (a fixed text, or the three fixture messages once each).
        assert!(out.len() <= 400 * 200 + sent, "the output grew past what was asked: {} for {} bytes", out.len(), sent);
        assert!(b.removed == 0 || quit_after_login, "something was removed without a QUIT after signing in");
        // What the client sent as a password is never in a reply.
        let text = String::from_utf8_lossy(&out);
        for line in data.split(|c| *c == b'\n') {
            if let Some(rest) = line.get(..5).filter(|p| p.eq_ignore_ascii_case(b"PASS ")).and(line.get(5..)) {
                let secret = String::from_utf8_lossy(rest.strip_suffix(b"\r").unwrap_or(rest)).into_owned();
                if secret.len() >= 6 && !"Subject: a body no final newline dot".contains(&secret) && secret != SECRET_MARKER {
                    assert!(!text.contains(&secret), "a password was echoed");
                }
            }
        }
    }
}
