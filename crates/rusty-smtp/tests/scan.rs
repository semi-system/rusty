//! The virus and spam scanners as inbound filters, against fake clamd and rspamd servers: what is sent, what each answer means, and what a scanner that is down, slow or hostile means.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusty_core::config::OnError;
use rusty_smtp::filter::{FilterContext, FilterVerdict, InboundFilter, QuarantineFolder};
use rusty_smtp::scan::{ClamdFilter, RspamdFilter};
use rusty_smtp::session::Recipient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const MSG: &[u8] = b"From: a@remote.example\r\nTo: bob@example.test\r\nSubject: hi\r\n\r\nbody of the message\r\n";

fn ctx<'a>(data: &'a [u8], rcpt: &'a [Recipient]) -> FilterContext<'a> {
    FilterContext { peer: "192.0.2.9".parse().unwrap(), tls: true, helo: "mx.remote.example", mail_from: "a@remote.example", recipients: rcpt, data }
}

fn rcpt() -> Vec<Recipient> {
    vec![Recipient { address: "bob@example.test".into(), accounts: vec![] }]
}

// ------------------------------------------------------------------------------------------------ fake clamd

#[derive(Clone, Copy)]
enum Clam {
    Ok,
    Found,
    Error,
    Garbage,
    Hang,
    Close,
}

/// A clamd that checks the INSTREAM framing and records the bytes it received.
async fn clamd(mode: Clam) -> (String, Arc<Mutex<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let got = Arc::new(Mutex::new(Vec::new()));
    let g = got.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            let g = g.clone();
            tokio::spawn(async move {
                if matches!(mode, Clam::Close) {
                    return;
                }
                let mut cmd = [0u8; 10];
                if s.read_exact(&mut cmd).await.is_err() || &cmd != b"zINSTREAM\0" {
                    return;
                }
                let mut body = Vec::new();
                loop {
                    let mut len = [0u8; 4];
                    if s.read_exact(&mut len).await.is_err() {
                        return;
                    }
                    let n = u32::from_be_bytes(len) as usize;
                    if n == 0 {
                        break;
                    }
                    let mut chunk = vec![0u8; n];
                    if s.read_exact(&mut chunk).await.is_err() {
                        return;
                    }
                    body.extend_from_slice(&chunk);
                }
                *g.lock().unwrap() = body;
                match mode {
                    Clam::Ok => {
                        let _ = s.write_all(b"stream: OK\0").await;
                    }
                    Clam::Found => {
                        let _ = s.write_all(b"stream: Win.Test.EICAR_HDB-1 FOUND\0").await;
                    }
                    Clam::Error => {
                        let _ = s.write_all(b"INSTREAM size limit exceeded. ERROR\0").await;
                    }
                    Clam::Garbage => {
                        let _ = s.write_all(&vec![b'x'; 100_000]).await;
                    }
                    Clam::Hang => tokio::time::sleep(Duration::from_secs(30)).await,
                    Clam::Close => {}
                }
            });
        }
    });
    (addr, got)
}

fn clam_filter(addr: &str, on_error: OnError) -> ClamdFilter {
    ClamdFilter::new(addr, Duration::from_millis(600), 1024 * 1024, on_error)
}

#[tokio::test]
async fn clamd_gets_the_whole_message_and_a_clean_one_is_marked_clean() {
    let (addr, got) = clamd(Clam::Ok).await;
    let v = clam_filter(&addr, OnError::Tempfail).check(&ctx(MSG, &rcpt())).await;
    assert_eq!(v, FilterVerdict::Tag(vec![("X-Virus-Scan".into(), "clean".into())]));
    assert_eq!(&*got.lock().unwrap(), MSG, "every byte reached the scanner");
    // A message of several chunks arrives whole too.
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let (addr, got) = clamd(Clam::Ok).await;
    clam_filter(&addr, OnError::Tempfail).check(&ctx(&big, &rcpt())).await;
    assert_eq!(&*got.lock().unwrap(), &big);
}

#[tokio::test]
async fn a_virus_is_refused_with_550_and_the_signature_is_not_told() {
    let (addr, _) = clamd(Clam::Found).await;
    match clam_filter(&addr, OnError::Accept).check(&ctx(MSG, &rcpt())).await {
        FilterVerdict::Reject { code, enhanced, text } => {
            assert_eq!((code, enhanced), (550, "5.7.1"));
            assert!(!text.contains("EICAR") && !text.contains("Win."), "{text}");
        }
        v => panic!("{v:?}"),
    }
}

#[tokio::test]
async fn a_scanner_that_is_down_slow_or_hostile_means_what_on_error_says() {
    // Nothing listens; the connection closes; the answer is an error; the reply is nonsense and too long; it never answers.
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().to_string()
    };
    let mut addrs = vec![dead];
    for mode in [Clam::Close, Clam::Error, Clam::Garbage, Clam::Hang] {
        addrs.push(clamd(mode).await.0);
    }
    for addr in &addrs {
        match clam_filter(addr, OnError::Tempfail).check(&ctx(MSG, &rcpt())).await {
            FilterVerdict::Reject { code, enhanced, .. } => assert_eq!((code, enhanced), (451, "4.7.1"), "{addr}"),
            v => panic!("{addr}: {v:?}"),
        }
        assert_eq!(clam_filter(addr, OnError::Accept).check(&ctx(MSG, &rcpt())).await, FilterVerdict::Tag(vec![("X-Virus-Scan".into(), "unavailable".into())]), "{addr}");
    }
}

#[tokio::test]
async fn a_message_over_the_limit_is_not_sent_and_is_marked_skipped() {
    let (addr, got) = clamd(Clam::Found).await;
    let f = ClamdFilter::new(&addr, Duration::from_millis(600), 16, OnError::Tempfail);
    assert_eq!(f.check(&ctx(MSG, &rcpt())).await, FilterVerdict::Tag(vec![("X-Scan-Status".into(), "skipped-size".into())]));
    assert!(got.lock().unwrap().is_empty(), "nothing was sent to the scanner");
}

// ------------------------------------------------------------------------------------------------ fake rspamd

#[derive(Clone)]
enum Rs {
    Json(&'static str),
    Status(u16),
    Raw(Vec<u8>),
    Hang,
}

/// An rspamd that records the request head and body it got.
async fn rspamd(mode: Rs) -> (String, Arc<Mutex<(String, Vec<u8>)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen = Arc::new(Mutex::new((String::new(), Vec::new())));
    let sn = seen.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let _ = stop.load(Ordering::SeqCst);
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            let (mode, sn) = (mode.clone(), sn.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let (head_end, len) = loop {
                    let Ok(n) = s.read(&mut tmp).await else { return };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..p]).into_owned();
                        let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                        break (p + 4, len);
                    }
                };
                while buf.len() < head_end + len {
                    let Ok(n) = s.read(&mut tmp).await else { return };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                *sn.lock().unwrap() = (String::from_utf8_lossy(&buf[..head_end]).into_owned(), buf[head_end..head_end + len].to_vec());
                let reply = match mode {
                    Rs::Json(j) => format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{j}", j.len()).into_bytes(),
                    Rs::Status(c) => format!("HTTP/1.1 {c} Nope\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes(),
                    Rs::Raw(b) => b,
                    Rs::Hang => {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        return;
                    }
                };
                let _ = s.write_all(&reply).await;
            });
        }
    });
    (format!("http://{addr}"), seen)
}

fn rs_filter(url: &str, on_error: OnError) -> RspamdFilter {
    RspamdFilter::new(url, Duration::from_millis(600), 1024 * 1024, on_error)
}

#[tokio::test]
async fn rspamd_gets_the_message_and_the_envelope_and_a_score_becomes_headers() {
    let (url, seen) = rspamd(Rs::Json(r#"{"action":"no action","score":1.5,"required_score":15.0,"symbols":{}}"#)).await;
    let v = rs_filter(&url, OnError::Accept).check(&ctx(MSG, &rcpt())).await;
    match v {
        FilterVerdict::Tag(t) => {
            let get = |n: &str| t.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
            assert_eq!(get("X-Spam-Score").as_deref(), Some("1.50"));
            assert!(get("X-Spam-Status").unwrap().starts_with("No, score=1.50"));
        }
        v => panic!("{v:?}"),
    }
    let (head, body) = seen.lock().unwrap().clone();
    assert!(head.starts_with("POST /checkv2 HTTP/1.1"), "{head}");
    for want in ["IP: 192.0.2.9", "Helo: mx.remote.example", "From: a@remote.example", "Rcpt: bob@example.test", "TLS: yes"] {
        assert!(head.contains(want), "{want}: {head}");
    }
    assert_eq!(body, MSG);
}

#[tokio::test]
async fn each_rspamd_action_has_its_meaning() {
    for (action, check) in [
        ("reject", Box::new(|v: FilterVerdict| matches!(v, FilterVerdict::Reject { code: 550, .. })) as Box<dyn Fn(FilterVerdict) -> bool>),
        ("soft reject", Box::new(|v| matches!(v, FilterVerdict::Reject { code: 451, .. }))),
        ("greylist", Box::new(|v| matches!(v, FilterVerdict::Reject { code: 451, .. }))),
        ("add header", Box::new(|v| matches!(v, FilterVerdict::QuarantineTagged(QuarantineFolder::Spam, ref t) if t.iter().any(|(k, v)| k == "X-Spam-Status" && v.starts_with("Yes"))))),
        ("rewrite subject", Box::new(|v| matches!(v, FilterVerdict::QuarantineTagged(QuarantineFolder::Spam, _)))),
        ("something new", Box::new(|v| matches!(v, FilterVerdict::Tag(ref t) if t.iter().any(|(k, v)| k == "X-Spam-Status" && v.starts_with("Unknown"))))),
    ] {
        let json: &'static str = Box::leak(format!(r#"{{"action":"{action}","score":9.0,"required_score":6.0}}"#).into_boxed_str());
        let (url, _) = rspamd(Rs::Json(json)).await;
        let v = rs_filter(&url, OnError::Accept).check(&ctx(MSG, &rcpt())).await;
        assert!(check(v), "{action}");
    }
}

#[tokio::test]
async fn an_rspamd_that_is_down_slow_or_hostile_means_what_on_error_says() {
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let mut urls = vec![dead];
    for mode in [
        Rs::Status(500),
        Rs::Json("not json"),
        Rs::Json(r#"{"action":"reject"}"#),
        Rs::Json(r#"{"action":"reject","score":"high"}"#),
        Rs::Raw(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n".to_vec()),
        Rs::Raw(vec![b'x'; 2_000_000]),
        Rs::Raw(Vec::new()),
        Rs::Hang,
    ] {
        urls.push(rspamd(mode).await.0);
    }
    for url in &urls {
        match rs_filter(url, OnError::Tempfail).check(&ctx(MSG, &rcpt())).await {
            FilterVerdict::Reject { code, .. } => assert_eq!(code, 451, "{url}"),
            v => panic!("{url}: {v:?}"),
        }
        assert_eq!(rs_filter(url, OnError::Accept).check(&ctx(MSG, &rcpt())).await, FilterVerdict::Tag(vec![("X-Spam-Status".into(), "unavailable".into())]), "{url}");
    }
}

#[tokio::test]
async fn a_header_value_cannot_carry_a_line_break_to_the_scanner() {
    let (url, seen) = rspamd(Rs::Json(r#"{"action":"no action","score":0.0,"required_score":6.0}"#)).await;
    let c = FilterContext { peer: "192.0.2.9".parse().unwrap(), tls: false, helo: "evil\r\nX-Injected: yes", mail_from: "a@x\r\nBcc: z@z", recipients: &rcpt(), data: MSG };
    rs_filter(&url, OnError::Accept).check(&c).await;
    let (head, _) = seen.lock().unwrap().clone();
    assert!(!head.lines().any(|l| l.starts_with("X-Injected") || l.starts_with("Bcc")), "{head}");
}

// ------------------------------------------------------------------------------------------------ what the scans came to (for the metrics)

#[tokio::test]
async fn every_scan_is_told_to_the_observer_as_a_scanner_and_a_fixed_result_word() {
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let obs: rusty_smtp::scan::Observer = {
        let seen = seen.clone();
        Arc::new(move |scanner, result| seen.lock().unwrap().push((scanner.to_owned(), result.to_owned())))
    };
    for (mode, want) in [(Clam::Ok, "clean"), (Clam::Found, "virus"), (Clam::Error, "error"), (Clam::Close, "error")] {
        let (addr, _) = clamd(mode).await;
        clam_filter(&addr, OnError::Accept).with_observer(obs.clone()).check(&ctx(MSG, &rcpt())).await;
        assert_eq!(seen.lock().unwrap().pop(), Some(("clamd".to_owned(), want.to_owned())));
    }
    for (action, want) in [("no action", "clean"), ("reject", "spam"), ("add header", "spam"), ("soft reject", "deferred"), ("something new", "unknown")] {
        let json: &'static str = Box::leak(format!(r#"{{"action":"{action}","score":9.0,"required_score":6.0}}"#).into_boxed_str());
        let (url, _) = rspamd(Rs::Json(json)).await;
        rs_filter(&url, OnError::Accept).with_observer(obs.clone()).check(&ctx(MSG, &rcpt())).await;
        assert_eq!(seen.lock().unwrap().pop(), Some(("rspamd".to_owned(), want.to_owned())), "{action}");
    }
    let (url, _) = rspamd(Rs::Status(500)).await;
    rs_filter(&url, OnError::Accept).with_observer(obs.clone()).check(&ctx(MSG, &rcpt())).await;
    assert_eq!(seen.lock().unwrap().pop(), Some(("rspamd".to_owned(), "error".to_owned())), "a scanner that fails is an error result");
    // A scan skipped for size is told too, and nothing is told when nobody observes.
    let big = vec![b'x'; 100];
    let (addr, _) = clamd(Clam::Ok).await;
    ClamdFilter::new(&addr, Duration::from_secs(2), 10, OnError::Accept).with_observer(obs).check(&ctx(&big, &rcpt())).await;
    assert_eq!(seen.lock().unwrap().pop(), Some(("clamd".to_owned(), "skipped".to_owned())));
}

// ------------------------------------------------------------------------------------------------ the lines of the recipients' plans

struct Lines(Option<f64>, Option<f64>);

#[async_trait::async_trait]
impl rusty_smtp::scan::ThresholdSource for Lines {
    async fn thresholds(&self, _: &[rusty_core::AccountId]) -> (Option<f64>, Option<f64>) {
        (self.0, self.1)
    }
}

#[tokio::test]
async fn the_recipients_own_spam_lines_replace_rspamds_actions_when_they_have_any() {
    // rspamd says "reject" at score 9; the plan's lines are junk from 10 and refuse from 15: the score is only junk-free mail.
    let (url, _) = rspamd(Rs::Json(r#"{"action":"reject","score":9.0,"required_score":6.0}"#)).await;
    let f = rs_filter(&url, OnError::Accept).with_thresholds(Arc::new(Lines(Some(10.0), Some(15.0))));
    assert!(matches!(f.check(&ctx(MSG, &rcpt())).await, FilterVerdict::Tag(ref t) if t.iter().any(|(k, v)| k == "X-Spam-Status" && v.starts_with("No"))));
    // The same score under stricter lines goes to the Spam folder, and above the second line is refused.
    let f = rs_filter(&url, OnError::Accept).with_thresholds(Arc::new(Lines(Some(5.0), Some(15.0))));
    assert!(matches!(f.check(&ctx(MSG, &rcpt())).await, FilterVerdict::QuarantineTagged(QuarantineFolder::Spam, _)));
    let f = rs_filter(&url, OnError::Accept).with_thresholds(Arc::new(Lines(Some(5.0), Some(9.0))));
    assert!(matches!(f.check(&ctx(MSG, &rcpt())).await, FilterVerdict::Reject { code: 550, .. }));
    // A temporary refusal of rspamd (greylisting) stays one.
    let (url, _) = rspamd(Rs::Json(r#"{"action":"greylist","score":1.0,"required_score":6.0}"#)).await;
    let f = rs_filter(&url, OnError::Accept).with_thresholds(Arc::new(Lines(Some(5.0), Some(15.0))));
    assert!(matches!(f.check(&ctx(MSG, &rcpt())).await, FilterVerdict::Reject { code: 451, .. }));
    // Without lines rspamd's own action decides, as before.
    let (url, _) = rspamd(Rs::Json(r#"{"action":"reject","score":9.0,"required_score":6.0}"#)).await;
    let f = rs_filter(&url, OnError::Accept).with_thresholds(Arc::new(Lines(None, None)));
    assert!(matches!(f.check(&ctx(MSG, &rcpt())).await, FilterVerdict::Reject { code: 550, .. }));
}

#[test]
fn the_score_lines_are_judged_in_order() {
    use rusty_smtp::scan::judge;
    assert_eq!(judge(3.0, Some(5.0), Some(10.0)), None);
    assert_eq!(judge(5.0, Some(5.0), Some(10.0)), Some(false));
    assert_eq!(judge(10.0, Some(5.0), Some(10.0)), Some(true));
    assert_eq!(judge(50.0, None, None), None);
    assert_eq!(judge(50.0, None, Some(10.0)), Some(true));
}
