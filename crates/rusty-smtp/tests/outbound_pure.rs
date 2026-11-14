//! The pure parts of the outbound path, no sockets and no database: the remote reply parser, retry
//! policy and classification, the job payload decoder, the bounce builder, MX planning, data encoding.

use rusty_core::config::OutboundConfig;
use rusty_core::BlobHash;
use rusty_smtp::outbound::client::encode_data;
use rusty_smtp::outbound::dial::forbidden_address;
use rusty_smtp::outbound::dsn::{self, DsnInput, Failure};
use rusty_smtp::outbound::payload::OutboundPayload;
use rusty_smtp::outbound::policy::{classify, expired, next_step, Next, Outcome};
use rusty_smtp::outbound::smtp_reply::{parse_reply, Class, ReplyError};
use rusty_smtp::resolve::{plan_for, MxAnswer, MxHost, Plan};
use serde_json::json;


#[test]
fn replies_parse_single_multi_line_and_enhanced_codes() {
    let (r, used) = parse_reply(b"250-mx hello\r\n250-SIZE 100\r\n250 STARTTLS\r\nrest").unwrap().unwrap();
    assert_eq!((r.code, used), (250, 42));
    assert_eq!(r.lines, vec!["mx hello", "SIZE 100", "STARTTLS"]);
    let (r, _) = parse_reply(b"550 5.1.1 <a@b>: User unknown\r\n").unwrap().unwrap();
    assert_eq!((r.code, r.enhanced.as_deref(), r.class()), (550, Some("5.1.1"), Class::Permanent));
    assert_eq!(r.text, "<a@b>: User unknown");
    // No enhanced code, bare LF accepted, text-less line.
    let (r, _) = parse_reply(b"354\n").unwrap().unwrap();
    assert_eq!((r.code, r.enhanced.clone(), r.class()), (354, None, Class::Intermediate));
    // Incomplete: more bytes needed, also in the middle of a multi-line reply.
    assert_eq!(parse_reply(b"250-a\r\n250 b"), Ok(None));
    assert_eq!(parse_reply(b""), Ok(None));
}

#[test]
fn hostile_replies_are_refused_before_anything_grows() {
    assert_eq!(parse_reply(b"2x0 hi\r\n"), Err(ReplyError::Malformed));
    assert_eq!(parse_reply(b"250xfoo\r\n"), Err(ReplyError::Malformed));
    assert_eq!(parse_reply(b"250-a\r\n251 b\r\n"), Err(ReplyError::CodeChanged));
    assert_eq!(parse_reply(b"999 no\r\n"), Err(ReplyError::Malformed));
    assert_eq!(parse_reply(&vec![b'x'; 5000]), Err(ReplyError::LineTooLong));
    let mut many = Vec::new();
    for _ in 0..200 {
        many.extend_from_slice(b"250-line\r\n");
    }
    assert_eq!(parse_reply(&many), Err(ReplyError::TooManyLines));
    let mut long = Vec::new();
    for _ in 0..20 {
        long.extend_from_slice(format!("250-{}\r\n", "y".repeat(1000)).as_bytes());
    }
    assert!(matches!(parse_reply(&long), Err(ReplyError::TooLong)), "reply total is bounded");
}

#[test]
fn classification_follows_the_first_digit_and_never_bounces_for_a_broken_server() {
    let reply = |s: &str| parse_reply(s.as_bytes()).unwrap().unwrap().0;
    assert_eq!(classify(&reply("250 ok\r\n")), Outcome::Delivered);
    assert!(matches!(classify(&reply("450 4.2.0 later\r\n")), Outcome::Temporary { .. }));
    assert!(matches!(classify(&reply("421 4.3.2 closing\r\n")), Outcome::Temporary { .. }));
    assert!(matches!(classify(&reply("550 5.1.1 no\r\n")), Outcome::Permanent { status, .. } if status == "5.1.1"));
    assert!(matches!(classify(&reply("554 plain\r\n")), Outcome::Permanent { status, .. } if status == "5.0.0"));
    assert!(matches!(classify(&reply("354 go\r\n")), Outcome::Temporary { .. }), "a 3xx where an answer is due is a server bug: defer");
    assert!(matches!(classify(&reply("150 odd\r\n")), Outcome::Temporary { .. }));
}

#[test]
fn the_backoff_schedule_is_5m_15m_30m_1h_2h_4h_then_every_4h_and_stops_at_the_deadline() {
    let cfg = OutboundConfig::default();
    let delays: Vec<u32> = (0..9).map(|i| cfg.backoff_for(i)).collect();
    assert_eq!(delays, [300, 900, 1800, 3600, 7200, 14_400, 14_400, 14_400, 14_400]);
    let (created, max) = (1000i64, cfg.max_age_secs as i64);
    assert_eq!(next_step(&cfg, created, created + 10, 0), Next::Retry { after_secs: 300 });
    assert_eq!(next_step(&cfg, created, created + 10, 40), Next::Retry { after_secs: 14_400 });
    // The last retry is pulled in to land exactly on the deadline, so the bounce is on time.
    assert_eq!(next_step(&cfg, created, created + max - 100, 6), Next::Retry { after_secs: 100 });
    assert_eq!(next_step(&cfg, created, created + max, 6), Next::GiveUp);
    assert!(!expired(&cfg, created, created + max - 1) && expired(&cfg, created, created + max));
    // Five days by default.
    assert_eq!(cfg.max_age_secs, 5 * 86_400);
}

fn good_payload() -> serde_json::Value {
    json!({"blob": "a".repeat(64), "envelope_from": "alice@example.net", "recipients": ["Bob@Remote.Test", "carol@remote.test"],
           "destination": "Remote.Test", "sender_domain": "example.net"})
}

#[test]
fn the_payload_round_trips_and_keeps_the_try_counter() {
    let p = OutboundPayload::decode(&good_payload()).unwrap();
    assert_eq!((p.tries, p.destination.as_str()), (0, "remote.test"));
    assert_eq!(p.recipients.len(), 2);
    let mut again = p.clone();
    again.tries = 3;
    assert_eq!(OutboundPayload::decode(&again.encode()).unwrap(), again);
    let null_sender = {
        let mut v = good_payload();
        v["envelope_from"] = json!("");
        v
    };
    assert_eq!(OutboundPayload::decode(&null_sender).unwrap().envelope_from, "");
}

#[test]
fn a_payload_that_could_inject_smtp_commands_is_refused() {
    for (field, value) in [
        ("envelope_from", json!("a@b>\r\nRCPT TO:<x@y")),
        ("envelope_from", json!("a b@c")),
        ("recipients", json!(["b@remote.test\r\nDATA"])),
        ("recipients", json!(["<b@remote.test>"])),
        ("recipients", json!(["b@other.test"])), // outside the destination
        ("recipients", json!([])),
        ("recipients", json!("b@remote.test")),
        ("blob", json!("../../etc/passwd")),
        ("destination", json!("bad domain")),
        ("tries", json!(-1)),
    ] {
        let mut v = good_payload();
        v[field] = value.clone();
        assert!(OutboundPayload::decode(&v).is_err(), "{field} = {value}");
    }
    assert!(OutboundPayload::decode(&json!([1, 2])).is_err());
    assert!(OutboundPayload::decode(&json!({})).is_err());
}

#[test]
fn data_is_dot_stuffed_crlf_normalised_and_terminated() {
    assert_eq!(encode_data(b"a\r\n.b\r\n..c\r\n"), b"a\r\n..b\r\n...c\r\n.\r\n");
    assert_eq!(encode_data(b"no newline at end"), b"no newline at end\r\n.\r\n");
    assert_eq!(encode_data(b"bare\nlf\rcr\r\n"), b"bare\r\nlf\r\ncr\r\n.\r\n");
    assert_eq!(encode_data(b""), b"\r\n.\r\n");
    assert_eq!(encode_data(b".\r\n"), b"..\r\n.\r\n");
}

#[test]
fn mx_planning_orders_by_preference_rotates_equals_and_honours_null_mx_and_implicit_mx() {
    let hosts = |v: &[(u16, &str)]| MxAnswer::Hosts(v.iter().map(|(p, h)| MxHost { preference: *p, host: (*h).into() }).collect());
    let order = |p: Plan| match p {
        Plan::Try(t) => t.into_iter().map(|t| t.host).collect::<Vec<_>>(),
        Plan::Permanent(m) => panic!("{m}"),
    };
    let ans = hosts(&[(20, "c"), (10, "b"), (10, "a"), (5, "z")]);
    assert_eq!(order(plan_for("d.test", ans.clone(), 0)), ["z", "a", "b", "c"]);
    assert_eq!(order(plan_for("d.test", ans, 1)), ["z", "b", "a", "c"], "equal preferences rotate with the seed");
    assert_eq!(order(plan_for("d.test", MxAnswer::NoRecords, 0)), ["d.test"], "implicit MX");
    assert!(matches!(plan_for("d.test", MxAnswer::NullMx, 0), Plan::Permanent(m) if m.contains("null MX")));
    assert!(matches!(plan_for("d.test", MxAnswer::NoSuchDomain, 0), Plan::Permanent(_)));
    assert!(matches!(plan_for("d.test", hosts(&[(0, ".")]), 0), Plan::Permanent(_)), "a lone root target is a null MX");
    let many: Vec<(u16, String)> = (0..20).map(|i| (i, format!("h{i}"))).collect();
    let many = MxAnswer::Hosts(many.iter().map(|(p, h)| MxHost { preference: *p, host: h.clone() }).collect());
    assert_eq!(order(plan_for("d.test", many, 0)).len(), 8, "bounded number of exchangers");
}

#[test]
fn addresses_that_can_only_be_this_host_are_never_dialed() {
    for ip in ["127.0.0.1", "0.0.0.0", "169.254.1.1", "224.0.0.1", "255.255.255.255", "::1", "::", "fe80::1", "ff02::1", "::ffff:127.0.0.1"] {
        assert!(forbidden_address(ip.parse().unwrap()), "{ip}");
    }
    for ip in ["10.77.0.12", "192.0.2.1", "172.16.5.4", "2001:db8::1"] {
        assert!(!forbidden_address(ip.parse().unwrap()), "{ip}: private networks are legitimate mail destinations");
    }
}

fn failure(r: &str) -> Failure {
    Failure { recipient: r.into(), status: "5.1.1".into(), diagnostic: "550 5.1.1 <x>: User unknown".into(), remote_mta: Some("mx.remote.test".into()) }
}

#[test]
fn the_bounce_is_an_rfc3464_report_with_the_original_headers_only() {
    let original = b"From: alice@example.net\r\nTo: x@remote.test\r\nSubject: secret plan\r\n\r\nTHE BODY MUST NOT BE RETURNED\r\n";
    let f = [failure("x@remote.test")];
    let bytes = dsn::build(&DsnInput { hostname: "mx.example.net", to: "alice@example.net", failures: &f, original, arrival: 1_794_139_200, now: 1_794_139_300, job_key: "7" });
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.starts_with("Return-Path: <>\r\nFrom: Mail Delivery System <MAILER-DAEMON@mx.example.net>\r\nTo: <alice@example.net>\r\n"), "{text}");
    assert!(text.contains("Auto-Submitted: auto-replied") && text.contains("Content-Type: multipart/report; report-type=delivery-status; boundary="));
    assert!(text.contains("Content-Type: message/delivery-status\r\n\r\nReporting-MTA: dns; mx.example.net\r\n"));
    assert!(text.contains("Final-Recipient: rfc822; x@remote.test\r\nAction: failed\r\nStatus: 5.1.1\r\nRemote-MTA: dns; mx.remote.test\r\nDiagnostic-Code: smtp; 550 5.1.1 <x>: User unknown\r\n"));
    assert!(text.contains("Content-Type: text/rfc822-headers\r\n\r\nFrom: alice@example.net\r\nTo: x@remote.test\r\nSubject: secret plan\r\n"));
    assert!(!text.contains("THE BODY"), "the body is never returned");
    assert!(text.trim_end().ends_with("--"), "closed multipart");
    // Same input, same bytes: a re-run builds the identical bounce.
    let again = dsn::build(&DsnInput { hostname: "mx.example.net", to: "alice@example.net", failures: &f, original, arrival: 1_794_139_200, now: 1_794_139_300, job_key: "7" });
    assert_eq!(text.as_bytes(), again.as_slice());
}

#[test]
fn remote_text_cannot_inject_headers_or_boundaries_into_a_bounce() {
    let mut f = failure("x@remote.test");
    f.diagnostic = "550 no\r\nBcc: attacker@evil.test\r\n\r\n--rusty-dsn-x--".into();
    let original = b"Subject: a\r\n--rusty-dsn-fake\r\nX: y\r\n\r\nbody";
    let text = String::from_utf8(dsn::build(&DsnInput { hostname: "mx.example.net", to: "a@example.net", failures: &[f], original, arrival: 0, now: 0, job_key: "1" })).unwrap();
    assert!(!text.contains("\r\nBcc:"), "no new header line from remote text: {text}");
    assert!(text.lines().filter(|l| l.starts_with("--rusty-dsn-")).all(|l| l.len() > 30), "only our boundary lines exist");
    let hdrs = dsn::header_section(original, 8192);
    assert_eq!(hdrs, b"Subject: a\r\nX: y\r\n", "boundary-looking lines are dropped from the copied headers");
}

#[test]
fn control_characters_cannot_hide_a_boundary_line_in_the_copied_headers() {
    // The boundary check has to look at what is copied, not at the raw line: stripping the control
    // character from `\x01--x` would otherwise leave a line that starts with `--` (found by the
    // `arbitrary_bytes_never_panic` property test of the fuzz smoke run). A line that is empty once
    // cleaned must not become a blank line either, which would end the copied header section early.
    let original = b"Subject: a\r\n\x01--rusty-dsn-fake\r\n\x7f--also\r\n\x1b\r\nX: y\r\n\r\nbody";
    let hdrs = dsn::header_section(original, 8192);
    assert_eq!(hdrs, b"Subject: a\r\nX: y\r\n", "{:?}", String::from_utf8_lossy(&hdrs));
}

#[test]
fn the_bounce_id_is_stable_per_job_and_failure_set() {
    let a = dsn::delivery_id("7", &[failure("a@r.test"), failure("b@r.test")]);
    assert_eq!(a, dsn::delivery_id("7", &[failure("b@r.test"), failure("a@r.test")]), "order does not matter");
    assert_ne!(a, dsn::delivery_id("8", &[failure("a@r.test"), failure("b@r.test")]));
    assert_ne!(a, dsn::delivery_id("7", &[failure("a@r.test")]));
    let _ = BlobHash::of(b"x");
}
