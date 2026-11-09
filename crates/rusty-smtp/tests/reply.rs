//! Reply rendering and the one shared mapping table from store errors to SMTP statuses
//! (plan 1b rule 4): Busy -> 451 4.3.0, Quota -> 552, Invalid -> 5xx, NotFound -> 550 5.1.1,
//! storage -> 451 and never any internal text.

use rusty_core::Error;
use rusty_smtp::reply::{map_error, delivery_reply, ErrorContext, Reply};

fn rendered(r: &Reply) -> String {
    String::from_utf8(r.render()).unwrap()
}

// backend-neutral: pure rendering, no database
#[test]
fn single_and_multi_line_rendering() {
    assert_eq!(rendered(&Reply::new(250, "2.0.0", "Ok")), "250 2.0.0 Ok\r\n");
    assert_eq!(rendered(&Reply::new(220, "", "mx ESMTP")), "220 mx ESMTP\r\n");
    let multi = Reply::multi(250, "", vec!["mx.example.net".into(), "PIPELINING".into(), "8BITMIME".into()]);
    assert_eq!(rendered(&multi), "250-mx.example.net\r\n250-PIPELINING\r\n250 8BITMIME\r\n");
}

// backend-neutral: pure rendering, no database
#[test]
fn text_cannot_break_the_framing() {
    let r = Reply::new(550, "5.1.1", "bad\r\n250 forged");
    let out = rendered(&r);
    assert_eq!(out.matches("\r\n").count(), 1, "{out:?}");
    assert!(out.starts_with("550 5.1.1 "));
}

// backend-neutral: pure mapping table, no database
#[test]
fn the_mapping_table_for_recipients() {
    let cases: Vec<(Error, u16, &str)> = vec![
        (Error::Busy("db".into()), 451, "4.3.0"),
        (Error::storage("boom"), 451, "4.3.0"),
        (Error::NotFound("x".into()), 550, "5.1.1"),
        (Error::Invalid("x".into()), 501, "5.1.3"),
        (Error::Quota("x".into()), 550, "5.5.3"),
        (Error::Conflict("x".into()), 451, "4.3.0"),
    ];
    for (e, code, enh) in cases {
        let r = map_error(&e, ErrorContext::Rcpt);
        assert_eq!((r.code, r.enhanced), (code, enh), "{e:?}");
    }
}

// backend-neutral: pure mapping table, no database
#[test]
fn the_mapping_table_for_data() {
    let cases: Vec<(Error, u16, &str)> = vec![
        (Error::Busy("db".into()), 451, "4.3.0"),
        (Error::storage("boom"), 451, "4.3.0"),
        (Error::NotFound("x".into()), 550, "5.1.1"),
        (Error::Invalid("x".into()), 554, "5.6.0"),
        (Error::Quota("x".into()), 552, "5.2.2"),
        (Error::Conflict("x".into()), 451, "4.3.0"),
    ];
    for (e, code, enh) in cases {
        let r = map_error(&e, ErrorContext::Data);
        assert_eq!((r.code, r.enhanced), (code, enh), "{e:?}");
    }
}

// backend-neutral: pure mapping table, no database
#[test]
fn error_text_never_leaks_internals() {
    let leaky = Error::storage_with("sqlite: no such table accounts (/var/lib/rusty/meta.sqlite)", std::io::Error::other("secret path"));
    for ctx in [ErrorContext::Rcpt, ErrorContext::Data] {
        let out = rendered(&map_error(&leaky, ctx));
        assert!(!out.contains("sqlite") && !out.contains("/var") && !out.contains("secret"), "{out}");
        let out = rendered(&map_error(&Error::Invalid("user@secret.example".into()), ctx));
        assert!(!out.contains("secret"), "{out}");
    }
}

// backend-neutral: pure mapping rule, no database
#[test]
fn delivery_outcomes_are_all_or_451_unless_every_failure_is_permanent() {
    let ok = || -> Result<(), Error> { Ok(()) };
    // all stored
    assert_eq!(delivery_reply(vec![ok(), ok()]).code, 250);
    // a temporary failure anywhere: 451 (the retry is idempotent for the stored ones)
    assert_eq!(delivery_reply(vec![ok(), Err(Error::Busy("x".into()))]).code, 451);
    // a permanent failure next to a success: still 451, so nothing is silently dropped
    assert_eq!(delivery_reply(vec![ok(), Err(Error::Quota("x".into()))]).code, 451);
    // only permanent failures: permanent reply
    assert_eq!(delivery_reply(vec![Err(Error::Quota("x".into()))]).code, 552);
    assert_eq!(delivery_reply(vec![Err(Error::Quota("x".into())), Err(Error::NotFound("x".into()))]).code, 552);
    assert_eq!(delivery_reply(vec![Err(Error::Invalid("x".into()))]).code, 554);
    // mixed temporary and permanent: temporary wins
    assert_eq!(delivery_reply(vec![Err(Error::Quota("x".into())), Err(Error::storage("x"))]).code, 451);
    // nothing to deliver is a bug upstream: fail closed
    assert_eq!(delivery_reply(vec![]).code, 451);
}
