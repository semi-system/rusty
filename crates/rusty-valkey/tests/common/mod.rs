#![allow(dead_code)]
//! The Valkey of the tests: `RUSTY_TEST_VALKEY_URL` (`valkey://user:password@host:port`, written by
//! scripts/kv-up.sh). Missing: skipped, unless `RUSTY_REQUIRE_VALKEY=1`, then it is a failure (CI).

use std::time::Duration;

use rusty_valkey::{ValkeyKv, ValkeyOptions};

pub fn url() -> Option<String> {
    match std::env::var("RUSTY_TEST_VALKEY_URL") {
        Ok(u) if !u.is_empty() => Some(u),
        _ => {
            assert!(
                std::env::var("RUSTY_REQUIRE_VALKEY").as_deref() != Ok("1"),
                "RUSTY_REQUIRE_VALKEY=1 but RUSTY_TEST_VALKEY_URL is not set (scripts/kv-up.sh starts one)"
            );
            None
        }
    }
}

/// `(host, port, user, password)` out of the URL of the test server.
pub fn parts(url: &str) -> (String, u16, String, String) {
    let rest = url.strip_prefix("valkey://").expect("valkey:// URL");
    let (cred, hostport) = rest.split_once('@').expect("credentials");
    let (user, password) = cred.split_once(':').expect("user:password");
    let (host, port) = hostport.rsplit_once(':').expect("host:port");
    (host.to_owned(), port.parse().expect("port"), user.to_owned(), password.to_owned())
}

pub fn options(prefix: &str) -> Option<ValkeyOptions> {
    let (host, port, user, password) = parts(&url()?);
    Some(ValkeyOptions {
        host,
        port,
        tls: None,
        username: Some(user),
        password: Some(password),
        connect_timeout: Duration::from_secs(2),
        command_timeout: Duration::from_secs(2),
        key_prefix: format!("{prefix}:"),
    })
}

pub async fn valkey_kv(prefix: &str) -> Option<ValkeyKv> {
    let o = options(prefix)?;
    Some(ValkeyKv::connect(o).await.expect("connect to the test Valkey"))
}
