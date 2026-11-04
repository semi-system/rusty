//! Debug output of configuration and credential-carrying types must never contain secrets.
//! backend-neutral: pure unit test of Debug output, no database

use rusty_store::repo::{AuthRow, NewAccount};
use rusty_core::DomainId;

#[cfg(feature = "postgres")]
#[test]
fn postgres_store_config_debug_has_no_password() {
    use rusty_store::StoreConfig;
    let cfg = StoreConfig::postgres("postgres://alice:hunter2-secret@db.internal:5432/rusty");
    for text in [format!("{cfg:?}"), format!("{cfg:#?}")] {
        assert!(!text.contains("hunter2-secret"), "password leaked: {text}");
        assert!(!text.contains("alice"), "user leaked: {text}");
        assert!(text.contains("Postgres"), "backend should still be named: {text}");
    }
}

#[test]
fn sqlite_store_config_debug_is_still_useful() {
    let cfg = rusty_store::StoreConfig::sqlite("sqlite::memory:");
    assert!(format!("{cfg:?}").contains("sqlite::memory:"));
}

#[test]
fn password_hashes_are_not_printed() {
    let phc = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2hoYXNo";
    let row = AuthRow { status: "active".into(), password_hash: Some(phc.into()) };
    assert!(!format!("{row:?}").contains("argon2id"));
    let new = NewAccount {
        local: "bob".into(),
        domain: DomainId(1),
        plan: None,
        password_hash: Some(phc.into()),
    };
    assert!(!format!("{new:?}").contains("argon2id"));
}
