//! Stable-toolchain stand-in for fuzz/fuzz_targets/fuzz_pem.rs: same entry points and invariants,
//! driven by proptest. Run by scripts/ci.sh when cargo-fuzz/nightly are missing.

use proptest::prelude::*;
use rusty_svc::{certified_key, parse_cert_chain, parse_private_key, MAX_CERT_CHAIN};

fn check(data: &[u8]) {
    if let Ok(chain) = parse_cert_chain(data) {
        assert!(!chain.is_empty() && chain.len() <= MAX_CERT_CHAIN);
    }
    let _ = parse_private_key(data);
    let (cert, key) = data.split_at(data.len() / 2);
    let _ = certified_key(cert, key);
    let _ = certified_key(data, data);
}

fn valid_pair() -> (String, String) {
    let ck = rcgen::generate_simple_self_signed(vec!["mail.example.test".to_string()]).unwrap();
    (ck.cert.pem(), ck.signing_key.serialize_pem())
}

#[test]
fn seeds_pass() {
    for s in ["cert.pem", "two-certs.pem", "bad-key.pem", "encrypted.pem", "cert-then-bad-key.pem"] {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/seeds/fuzz_pem").join(s);
        check(&std::fs::read(p).unwrap());
    }
    let (c, k) = valid_pair();
    check(c.as_bytes());
    check(k.as_bytes());
    assert!(certified_key(c.as_bytes(), k.as_bytes()).is_ok());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    #[test]
    fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..600)) {
        check(&data);
    }

    #[test]
    fn mutated_valid_pem_never_panics(pos in 0usize..2000, byte in any::<u8>(), cut in 0usize..2000) {
        let (c, k) = valid_pair();
        let mut both = format!("{c}{k}").into_bytes();
        let at = pos % both.len();
        if let Some(slot) = both.get_mut(at) {
            *slot = byte;
        }
        both.truncate(both.len() - cut % both.len());
        check(&both);
        // and the halves in their proper roles
        let mut cert = c.into_bytes();
        let mut key = k.into_bytes();
        let ci = pos % cert.len();
        let ki = pos % key.len();
        if let Some(s) = cert.get_mut(ci) { *s = byte; }
        if let Some(s) = key.get_mut(ki) { *s = byte; }
        let _ = certified_key(&cert, &key);
    }
}
