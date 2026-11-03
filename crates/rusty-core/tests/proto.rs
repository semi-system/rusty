use proptest::prelude::*;
use rusty_core::proto::{negotiate, VersionRange, PROTOCOL_VERSION, SUPPORTED};
use rusty_core::Error;

fn r(min: u16, max: u16) -> VersionRange {
    VersionRange::new(min, max).unwrap()
}

#[test]
fn negotiates_highest_common() {
    assert_eq!(negotiate(r(1, 5), r(3, 9)).unwrap(), 5);
    assert_eq!(negotiate(r(3, 9), r(1, 5)).unwrap(), 5);
    assert_eq!(negotiate(r(1, 1), r(1, 1)).unwrap(), 1);
    assert_eq!(negotiate(r(2, 4), r(4, 4)).unwrap(), 4);
    assert_eq!(negotiate(r(1, u16::MAX), r(1, u16::MAX)).unwrap(), u16::MAX);
    assert_eq!(negotiate(SUPPORTED, SUPPORTED).unwrap(), PROTOCOL_VERSION);
}

#[test]
fn disjoint_ranges_are_invalid() {
    assert!(matches!(negotiate(r(1, 2), r(3, 4)), Err(Error::Invalid(_))));
    assert!(matches!(negotiate(r(3, 4), r(1, 2)), Err(Error::Invalid(_))));
}

#[test]
fn zero_version_rejected() {
    assert!(matches!(VersionRange::new(0, 3), Err(Error::Invalid(_))));
    assert!(matches!(VersionRange::new(0, 0), Err(Error::Invalid(_))));
}

#[test]
fn min_greater_than_max_rejected() {
    assert!(matches!(VersionRange::new(5, 4), Err(Error::Invalid(_))));
}

#[test]
fn error_message_is_fixed() {
    let e = negotiate(r(11111, 22222), r(33333, 44444)).unwrap_err();
    assert_eq!(e.to_string(), negotiate(r(1, 2), r(3, 4)).unwrap_err().to_string());
    let s = format!("{e} {e:?}");
    for n in ["11111", "22222", "33333", "44444"] {
        assert!(!s.contains(n), "{s}");
    }
    let e = VersionRange::new(0, 61234).unwrap_err();
    assert!(!format!("{e} {e:?}").contains("61234"));
}

proptest! {
    #[test]
    fn negotiation_is_symmetric(a in 1u16.., b in 1u16.., c in 1u16.., d in 1u16..) {
        let (a, b) = (a.min(b), a.max(b));
        let (c, d) = (c.min(d), c.max(d));
        let (x, y) = (r(a, b), r(c, d));
        let (xy, yx) = (negotiate(x, y), negotiate(y, x));
        prop_assert_eq!(xy.is_ok(), yx.is_ok());
        if let (Ok(p), Ok(q)) = (xy, yx) {
            prop_assert_eq!(p, q);
            prop_assert!(p >= a.max(c) && p <= b.min(d));
        }
    }
}
