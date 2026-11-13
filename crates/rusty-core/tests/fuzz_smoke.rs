//! Stable-toolchain stand-in for fuzz/fuzz_targets/fuzz_config.rs: same entry point and
//! invariants, driven by proptest. Run by scripts/ci.sh when cargo-fuzz/nightly are missing.

use proptest::prelude::*;
use rusty_core::config::{
    is_loopback_host, parse_pg_url, valid_hostname, Profile, SslMode, StartupConfig, StoreSection, MAX_POOL_SIZE,
};

const SEED_FULL: &str = include_str!("../../../fuzz/seeds/fuzz_config/full");
const SEED_PG: &str = include_str!("../../../fuzz/seeds/fuzz_config/postgres");
const SEED_EXAMPLE: &str = include_str!("../../../rusty.example.toml");

fn check(text: &str) {
    let Ok(c) = StartupConfig::from_toml_str(text) else {
        return;
    };
    assert!(c.blob_dir.is_absolute());
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
    {
        let ep = parse_pg_url(c.store.url.as_deref().unwrap_or_default()).expect("validated url");
        assert!(!ep.user.is_empty() && !ep.host.is_empty() && ep.port != 0);
        assert!((1..=MAX_POOL_SIZE).contains(&c.store.pool_size));
        assert!(c.store.lock_timeout_ms >= 1 && c.store.statement_timeout_ms >= 1);
        // Never starts with an unverified remote database unless the explicit override is set.
        if c.check_startup(Profile::Production).is_ok()
            && !is_loopback_host(&ep.host)
            && c.store.sslmode != SslMode::VerifyFull
        {
            assert!(c.store.insecure_allow_unverified_tls);
        }
        if c.check_startup(Profile::Production).is_ok() {
            assert!(c.store.pool_size >= StoreSection::pool_floor(c.service_count()));
        }
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
    check(SEED_PG);
    assert!(StartupConfig::from_toml_str(SEED_PG).is_ok());
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

mod pg_url {
    use proptest::prelude::*;
    use rusty_core::config::parse_pg_url;

    fn check(text: &str) {
        if let Ok(e) = parse_pg_url(text) {
            assert!(!e.user.is_empty() && !e.host.is_empty() && !e.database.is_empty() && e.port != 0);
            assert!(!text.contains(['?', '#', '%']));
            assert!(!e.user.contains([':', '@', '/']));
        }
    }

    #[test]
    fn seeds_pass() {
        check(include_str!("../../../fuzz/seeds/fuzz_pg_url/plain"));
        check(include_str!("../../../fuzz/seeds/fuzz_pg_url/rejected"));
        check(include_str!("../../../fuzz/seeds/fuzz_pg_url/ipv6"));
        assert!(parse_pg_url(include_str!("../../../fuzz/seeds/fuzz_pg_url/plain")).is_ok());
        assert!(parse_pg_url(include_str!("../../../fuzz/seeds/fuzz_pg_url/rejected")).is_err());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(3000))]
        #[test]
        fn arbitrary_text_never_panics(s in "\\PC{0,120}") {
            check(&s);
        }
        #[test]
        fn url_shaped_text_never_panics(s in "postgres(ql)?://[a-z0-9:@/_%?#.-]{0,60}") {
            check(&s);
        }
    }
}
