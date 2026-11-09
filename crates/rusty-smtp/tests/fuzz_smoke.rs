//! Stable-toolchain stand-in for fuzz/fuzz_targets/fuzz_smtp_*.rs: the same entry points
//! (`rusty_smtp::fuzzing`), driven by proptest and the committed seeds. Run by scripts/ci.sh when
//! cargo-fuzz/nightly are missing.

use proptest::prelude::*;
use rusty_smtp::fuzzing::{check_command_stream, check_data, check_headers, check_path};

const COMMAND_SEEDS: [&[u8]; 3] = [
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_command/swaks_session"),
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_command/misc_verbs"),
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_command/edge_cases"),
];
const DATA_SEEDS: [&[u8]; 4] = [
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_data/simple"),
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_data/smuggling"),
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_data/empty"),
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_data/nul_byte"),
];
const PATH_SEEDS: [&[u8]; 4] = [
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_path/mail_full"),
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_path/rcpt"),
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_path/route_and_param"),
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_path/null"),
];
const HEADER_SEEDS: [&[u8]; 3] = [
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_headers/forged"),
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_headers/no_headers"),
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_headers/headers_only"),
];

// backend-neutral: pure codecs over bytes, no database
#[test]
fn seeds_pass() {
    COMMAND_SEEDS.iter().for_each(|s| check_command_stream(s));
    DATA_SEEDS.iter().for_each(|s| check_data(s));
    PATH_SEEDS.iter().for_each(|s| check_path(s));
    HEADER_SEEDS.iter().for_each(|s| check_headers(s));
}

fn mutate(seed: &[u8], pos: usize, byte: u8, cut: usize) -> Vec<u8> {
    let mut b = seed.to_vec();
    if !b.is_empty() {
        let at = pos % b.len();
        if let Some(slot) = b.get_mut(at) {
            *slot = byte;
        }
        let keep = b.len().saturating_sub(cut % b.len()).max(1);
        b.truncate(keep);
    }
    b
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1500))]

    // backend-neutral: pure codecs over bytes, no database
    #[test]
    fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..600)) {
        check_command_stream(&data);
        check_data(&data);
        check_path(&data);
        check_headers(&data);
    }

    // backend-neutral: pure codecs over bytes, no database
    #[test]
    fn mutated_seeds_never_panic(which in 0usize..4, pos in 0usize..300, byte in any::<u8>(), cut in 0usize..300) {
        check_command_stream(&mutate(COMMAND_SEEDS.get(which % 3).copied().unwrap_or_default(), pos, byte, cut));
        check_data(&mutate(DATA_SEEDS.get(which).copied().unwrap_or_default(), pos, byte, cut));
        check_path(&mutate(PATH_SEEDS.get(which).copied().unwrap_or_default(), pos, byte, cut));
        check_headers(&mutate(HEADER_SEEDS.get(which % 3).copied().unwrap_or_default(), pos, byte, cut));
    }
}
