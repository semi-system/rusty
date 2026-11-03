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

#[test]
fn hand_built_invalid_ranges_are_rejected() {
    let bad = [
        VersionRange { min: 0, max: 0 },
        VersionRange { min: 0, max: 3 },
        VersionRange { min: 5, max: 4 },
        VersionRange { min: u16::MAX, max: 1 },
    ];
    for b in bad {
        assert!(matches!(negotiate(b, r(1, 9)), Err(Error::Invalid(_))));
        assert!(matches!(negotiate(r(1, 9), b), Err(Error::Invalid(_))));
        assert!(matches!(negotiate(b, b), Err(Error::Invalid(_))));
    }
}

#[test]
fn supported_is_exactly_one() {
    assert_eq!(SUPPORTED.min, 1);
    assert_eq!(SUPPORTED.max, 1);
    assert_eq!(SUPPORTED, r(1, 1));
    assert_eq!(PROTOCOL_VERSION, 1);
}

proptest! {
    // Endpoints are arbitrary (including 0 and min > max), so invalid ranges are generated.
    #[test]
    fn negotiation_is_symmetric(a in any::<u16>(), b in any::<u16>(), c in any::<u16>(), d in any::<u16>()) {
        let x = VersionRange { min: a, max: b };
        let y = VersionRange { min: c, max: d };
        let (xy, yx) = (negotiate(x, y), negotiate(y, x));
        prop_assert_eq!(xy.is_ok(), yx.is_ok());
        if let (Ok(p), Ok(q)) = (xy, yx) {
            prop_assert_eq!(p, q);
            prop_assert!(a >= 1 && a <= b && c >= 1 && c <= d);
            prop_assert!(p >= a.max(c) && p <= b.min(d));
        }
    }
}
