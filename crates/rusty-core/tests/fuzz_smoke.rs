//! Stable-toolchain stand-in for fuzz/fuzz_targets/fuzz_config.rs: same entry point and
//! invariants, driven by proptest. Run by scripts/ci.sh when cargo-fuzz/nightly are missing.

use proptest::prelude::*;
use rusty_core::config::{valid_hostname, Profile, StartupConfig};

const SEED_FULL: &str = include_str!("../../../fuzz/seeds/fuzz_config/full");
const SEED_EXAMPLE: &str = include_str!("../../../rusty.example.toml");

fn check(text: &str) {
    let Ok(c) = StartupConfig::from_toml_str(text) else {
        return;
    };
    assert!(c.database_path.is_absolute() && c.blob_dir.is_absolute());
    assert!((1024..=1024 * 1024 * 1024).contains(&c.max_message_bytes));
    assert!(c.smtp_max_message_bytes() >= 1024 && c.smtp_max_message_bytes() <= c.max_message_bytes);
    assert!(c.gc_grace_seconds >= 3600);
    assert!(!c.smtp.proxy_protocol && !c.imap.proxy_protocol);
    assert!((1..=10_000).contains(&c.smtp.max_recipients));
    assert!((1..=600).contains(&c.limits.shutdown_grace_secs));
    assert_eq!(c.tls.cert_file.is_some(), c.tls.key_file.is_some());
    if c.tls.has_cert() {
        assert!(!c.tls.dev_self_signed);
    }
    if let Some(h) = &c.hostname {
        assert!(valid_hostname(h));
    }
    let prod = c.check_startup(Profile::Production);
    let dev = c.check_startup(Profile::Dev);
    if c.authenticated_listeners() > 0 && !c.tls.has_cert() && !c.tls.dev_self_signed {
        assert!(prod.is_err() && dev.is_err());
    }
    if c.hostname.is_none() {
        assert!(prod.is_err());
    }
}

#[test]
fn seeds_pass() {
    check(SEED_FULL);
    check(SEED_EXAMPLE);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn arbitrary_text_never_panics(s in "\\PC{0,400}") {
        check(&s);
    }

    #[test]
    fn mutated_seed_never_panics(pos in 0usize..400, byte in any::<u8>(), cut in 0usize..400) {
        let mut b = SEED_FULL.as_bytes().to_vec();
        if !b.is_empty() {
            let at = pos % b.len();
            if let Some(slot) = b.get_mut(at) {
                *slot = byte;
            }
            b.truncate(b.len().saturating_sub(cut % b.len()).max(1));
        }
        if let Ok(s) = std::str::from_utf8(&b) {
            check(s);
        }
    }
}
