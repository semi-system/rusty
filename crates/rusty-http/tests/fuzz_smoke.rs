//! Stable-toolchain stand-in for fuzz/fuzz_targets/fuzz_http_*.rs: the same entry points (`rusty_http::fuzzing`),
//! driven by proptest and the committed seeds. Run by scripts/ci.sh when cargo-fuzz/nightly are missing.

use proptest::prelude::*;
use rusty_http::fuzzing::{check_cookies, check_guards, check_request_path};

const PATH_SEEDS: [&[u8]; 3] = [
    include_bytes!("../../../fuzz/seeds/fuzz_http_path/api"),
    include_bytes!("../../../fuzz/seeds/fuzz_http_path/traversal"),
    include_bytes!("../../../fuzz/seeds/fuzz_http_path/bad_utf8"),
];
const COOKIE_SEEDS: [&[u8]; 2] = [
    include_bytes!("../../../fuzz/seeds/fuzz_http_cookies/session"),
    include_bytes!("../../../fuzz/seeds/fuzz_http_cookies/dup_and_bad"),
];
const GUARD_SEEDS: [&[u8]; 2] = [
    include_bytes!("../../../fuzz/seeds/fuzz_http_guards/good"),
    include_bytes!("../../../fuzz/seeds/fuzz_http_guards/bad"),
];

#[test]
fn the_seeds_pass() {
    PATH_SEEDS.iter().for_each(|s| check_request_path(s));
    COOKIE_SEEDS.iter().for_each(|s| check_cookies(s));
    GUARD_SEEDS.iter().for_each(|s| check_guards(s));
}

/// Mostly path-shaped text with the characters that matter, so the interesting branches are reached.
fn path_like() -> impl Strategy<Value = String> {
    proptest::collection::vec(prop_oneof![Just("/".to_owned()), Just("%".to_owned()), Just("..".to_owned()), Just(".".to_owned()), Just("%2f".to_owned()), Just("%00".to_owned()), "[a-zA-Z0-9%._~:@-]{0,8}", any::<char>().prop_map(|c| c.to_string())], 0..14)
        .prop_map(|v| v.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn paths_never_panic_and_keep_their_invariants(s in path_like()) {
        check_request_path(s.as_bytes());
    }

    #[test]
    fn arbitrary_bytes_as_a_path(data in proptest::collection::vec(any::<u8>(), 0..300)) {
        check_request_path(&data);
    }

    #[test]
    fn cookies_never_panic_and_a_built_cookie_reads_back(s in "[ -~]{0,120}") {
        check_cookies(s.as_bytes());
    }

    #[test]
    fn arbitrary_bytes_as_a_cookie_header(data in proptest::collection::vec(any::<u8>(), 0..300)) {
        check_cookies(&data);
    }

    #[test]
    fn guards_never_panic_and_never_let_through_what_one_of_them_refuses(s in "([A-Za-z-]{1,16}: [ -~]{0,60}\n){0,8}") {
        check_guards(s.as_bytes());
    }
}
