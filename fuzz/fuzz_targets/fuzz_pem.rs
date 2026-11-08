#![no_main]
use libfuzzer_sys::fuzz_target;
use rusty_svc::{certified_key, parse_cert_chain, parse_private_key, MAX_CERT_CHAIN};

fuzz_target!(|data: &[u8]| {
    // Neither loader may panic on any bytes; accepted input respects the documented bounds.
    if let Ok(chain) = parse_cert_chain(data) {
        assert!(!chain.is_empty() && chain.len() <= MAX_CERT_CHAIN);
    }
    let _ = parse_private_key(data);
    // The combined entry (what a reload runs): the two halves of the input as certificate and key.
    let (cert, key) = data.split_at(data.len() / 2);
    let _ = certified_key(cert, key);
    // And the same text in both roles.
    let _ = certified_key(data, data);
});
