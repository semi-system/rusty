#![no_main]
use libfuzzer_sys::fuzz_target;
use rusty_core::config::parse_pg_url;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    // Must never panic; an accepted URL has no password, no query and a plain endpoint.
    if let Ok(e) = parse_pg_url(text) {
        assert!(!e.user.is_empty() && !e.host.is_empty() && !e.database.is_empty() && e.port != 0);
        assert!(!text.contains(['?', '#', '%']));
        assert!(!e.user.contains([':', '@', '/']));
        assert!(!e.host.starts_with('/'));
    }
});
