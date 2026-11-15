//! The setup token gate: its limits, its window, and that a spent token never matches again.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use rusty_admin::{ManualNow, SetupGate, SetupVerdict};

fn ip(n: u8) -> IpAddr {
    format!("203.0.113.{n}").parse().unwrap()
}

fn gate() -> (SetupGate, String, Arc<ManualNow>) {
    let now = Arc::new(ManualNow::new(5_000_000));
    let (g, t) = SetupGate::new(now.clone());
    (g, t, now)
}

#[test]
fn the_token_is_256_random_bits_and_new_at_every_start() {
    let (_, a, _) = gate();
    let (_, b, _) = gate();
    assert_eq!(a.len(), 64);
    assert!(a.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
    assert_ne!(a, b);
}

#[test]
fn only_the_right_token_is_accepted_whatever_its_spelling_around_it() {
    let (g, t, _) = gate();
    assert_eq!(g.check(ip(1), &t), SetupVerdict::Accepted);
    assert_eq!(g.check(ip(1), &format!("  {t}\n")), SetupVerdict::Accepted, "surrounding white space is ignored");
    assert_eq!(g.check(ip(1), &t.to_uppercase()), SetupVerdict::Accepted, "hex is case-insensitive");
    for bad in ["", "x", &t[..63], &format!("{t}0"), &"0".repeat(64)] {
        assert_eq!(g.check(ip(2), bad), SetupVerdict::Wrong, "{bad:?}");
    }
}

#[test]
fn an_address_is_refused_after_five_wrong_tokens_and_the_others_are_not() {
    let (g, t, now) = gate();
    for _ in 0..5 {
        assert_eq!(g.check(ip(1), "nope"), SetupVerdict::Wrong);
    }
    assert!(matches!(g.check(ip(1), &t), SetupVerdict::Locked { retry_after_secs } if retry_after_secs > 3000), "locked even for the right token");
    assert_eq!(g.check(ip(2), &t), SetupVerdict::Accepted, "another address is unaffected");
    now.advance(Duration::from_secs(3601));
    assert_eq!(g.check(ip(1), &t), SetupVerdict::Accepted, "its hour is over");
}

#[test]
fn twenty_wrong_tokens_from_anywhere_lock_everyone_for_an_hour() {
    let (g, t, now) = gate();
    for i in 0..20u8 {
        // Four per address: never five from one.
        assert_eq!(g.check(ip(i / 4 + 10), "nope"), SetupVerdict::Wrong, "{i}");
    }
    for a in [1u8, 99, 200] {
        assert!(matches!(g.check(ip(a), &t), SetupVerdict::Locked { .. }), "{a}");
    }
    now.advance(Duration::from_secs(3601));
    assert_eq!(g.check(ip(99), &t), SetupVerdict::Accepted);
}

#[test]
fn a_spent_token_never_matches_again() {
    let (g, t, _) = gate();
    assert!(g.is_live());
    g.consume();
    assert!(!g.is_live());
    assert_eq!(g.check(ip(1), &t), SetupVerdict::Wrong);
    // An all-zero guess does not match the cleared state either.
    assert_eq!(g.check(ip(1), &"0".repeat(64)), SetupVerdict::Wrong);
}
