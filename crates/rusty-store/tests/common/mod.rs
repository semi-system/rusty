#![allow(dead_code)]
use rusty_store::SqliteStore;

/// Fresh private in-memory store with migrations applied.
pub async fn mem() -> SqliteStore {
    SqliteStore::open("sqlite::memory:").await.unwrap()
}
