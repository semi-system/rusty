//! Stable-toolchain stand-in for fuzz/fuzz_targets/fuzz_smtp_*.rs: the same entry points
//! (`rusty_smtp::fuzzing`), driven by proptest and the committed seeds. Run by scripts/ci.sh when
//! cargo-fuzz/nightly are missing.

use proptest::prelude::*;
use rusty_smtp::fuzzing::{
    check_auth, check_command_stream, check_data, check_dkim_key_record, check_dsn_headers, check_headers, check_path, check_queue_payload,
    check_reply,
};

const REPLY_SEEDS: [&[u8]; 4] = [
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_response/ehlo"),
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_response/reject"),
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_response/multi_and_trailing"),
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_response/bad"),
];
const KEY_SEEDS: [&[u8]; 3] = [
    include_bytes!("../../../fuzz/seeds/fuzz_dkim_key/spki_prefix"),
    include_bytes!("../../../fuzz/seeds/fuzz_dkim_key/tiny_der"),
    include_bytes!("../../../fuzz/seeds/fuzz_dkim_key/ed_zero"),
];
const DSN_SEEDS: [&[u8]; 2] = [
    include_bytes!("../../../fuzz/seeds/fuzz_dsn_headers/simple"),
    include_bytes!("../../../fuzz/seeds/fuzz_dsn_headers/no_headers"),
];
const PAYLOAD_SEEDS: [&[u8]; 2] = [
    include_bytes!("../../../fuzz/seeds/fuzz_queue_payload/ok"),
    include_bytes!("../../../fuzz/seeds/fuzz_queue_payload/injection"),
];

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
const AUTH_SEEDS: [&[u8]; 4] = [
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_auth/plain_ok"),
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_auth/login_ok"),
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_auth/cancel"),
    include_bytes!("../../../fuzz/seeds/fuzz_smtp_auth/bad_blobs"),
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
    AUTH_SEEDS.iter().for_each(|s| check_auth(s));
    REPLY_SEEDS.iter().for_each(|s| check_reply(s));
    DSN_SEEDS.iter().for_each(|s| check_dsn_headers(s));
    PAYLOAD_SEEDS.iter().for_each(|s| check_queue_payload(s));
    KEY_SEEDS.iter().for_each(|s| check_dkim_key_record(s));
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
        check_auth(&data);
        check_reply(&data);
        check_dsn_headers(&data);
        check_queue_payload(&data);
    }

    // backend-neutral: key-record parsing over bytes, no database
    #[test]
    fn hostile_key_records_never_panic_or_verify(data in proptest::collection::vec(any::<u8>(), 0..400)) {
        check_dkim_key_record(&data);
    }

    // backend-neutral: pure codecs over bytes, no database
    #[test]
    fn mutated_seeds_never_panic(which in 0usize..4, pos in 0usize..300, byte in any::<u8>(), cut in 0usize..300) {
        check_command_stream(&mutate(COMMAND_SEEDS.get(which % 3).copied().unwrap_or_default(), pos, byte, cut));
        check_data(&mutate(DATA_SEEDS.get(which).copied().unwrap_or_default(), pos, byte, cut));
        check_path(&mutate(PATH_SEEDS.get(which).copied().unwrap_or_default(), pos, byte, cut));
        check_headers(&mutate(HEADER_SEEDS.get(which % 3).copied().unwrap_or_default(), pos, byte, cut));
        check_auth(&mutate(AUTH_SEEDS.get(which).copied().unwrap_or_default(), pos, byte, cut));
        check_reply(&mutate(REPLY_SEEDS.get(which).copied().unwrap_or_default(), pos, byte, cut));
        check_dsn_headers(&mutate(DSN_SEEDS.get(which % 2).copied().unwrap_or_default(), pos, byte, cut));
        check_queue_payload(&mutate(PAYLOAD_SEEDS.get(which % 2).copied().unwrap_or_default(), pos, byte, cut));
    }
}
