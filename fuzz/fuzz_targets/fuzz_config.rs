#![no_main]
use libfuzzer_sys::fuzz_target;
use rusty_core::config::{
    is_loopback_host, parse_pg_url, valid_hostname, Profile, SslMode, StartupConfig, StoreSection, MAX_POOL_SIZE,
};

const MAX_RECIPIENTS: u32 = 10_000;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    // Must never panic; an accepted configuration must satisfy every documented bound.
    let Ok(c) = StartupConfig::from_toml_str(text) else {
        return;
    };
    assert!(c.blob_dir.is_absolute());
    assert!((1024..=1024 * 1024 * 1024).contains(&c.max_message_bytes));
    assert!(c.smtp_max_message_bytes() >= 1024 && c.smtp_max_message_bytes() <= c.max_message_bytes);
    assert!(c.gc_grace_seconds >= 3600);
    assert!(!c.smtp.proxy_protocol && !c.imap.proxy_protocol);
    assert!((1..=MAX_RECIPIENTS).contains(&c.smtp.max_recipients));
    assert!(c.smtp.max_connections >= 1 && c.imap.max_connections >= 1);
    assert!((1..=600).contains(&c.limits.shutdown_grace_secs));
    assert_eq!(c.tls.cert_file.is_some(), c.tls.key_file.is_some());
    if c.tls.has_cert() {
        assert!(!c.tls.dev_self_signed);
        assert!(c.tls.cert_file.as_deref().is_some_and(|p| p.is_absolute()));
        assert!(c.tls.key_file.as_deref().is_some_and(|p| p.is_absolute()));
    }
    if let Some(h) = &c.hostname {
        assert!(valid_hostname(h));
    }
    // The start-up rules are total, and fail closed: authenticated listeners without any TLS source
    // never pass.
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
});
