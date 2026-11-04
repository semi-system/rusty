//! Real-Postgres test support lives in rusty-store fixtures module (feature test-fixtures) so that
//! other crates tests can use it without a database driver of their own.
pub use rusty_store::fixtures::pg_db::*;
