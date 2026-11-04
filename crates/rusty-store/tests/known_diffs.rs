//! Documented divergences between the SQLite and Postgres backends (task PG6).
//!
//! A behaviour that legitimately differs is never "ported" by weakening an assertion. It gets a
//! row in [`KNOWN_DIFFS`] (what differs, why, and which test pins each side) and one test per
//! side. `known_diffs_are_pinned` fails when a listed pin does not exist, or when a test file
//! mentions a `KNOWN_DIFFS Dn` id that is not in the table.
mod common;

use common::backends::{pg_backend_or_skip, sqlite_backend};
use common::*;

pub struct KnownDiff {
    pub id: &'static str,
    pub what: &'static str,
    pub sqlite: &'static str,
    pub postgres: &'static str,
    pub reason: &'static str,
    /// `(file, test name)` of the test pinning the SQLite side and of the one pinning Postgres.
    pub sqlite_pin: (&'static str, &'static str),
    pub postgres_pin: (&'static str, &'static str),
}

pub const KNOWN_DIFFS: &[KnownDiff] = &[KnownDiff {
    id: "D1",
    what: "blobs.zero_since on a referenced blob (refcount > 0)",
    sqlite: "accepted: the column is plain data; GC must not trust it (gc.rs never_deletes_referenced_blob_even_if_zero_since_is_old)",
    postgres: "refused by CHECK blobs_zero_since_consistent ((refcount = 0) = (zero_since IS NOT NULL))",
    reason: "PG1/PG5: Postgres stamps zero_since in a BEFORE trigger and binds it with a CHECK; the SQLite schema is frozen (migrations are never edited) and cannot express the check on existing databases",
    sqlite_pin: ("known_diffs.rs", "sqlite_accepts_zero_since_on_a_referenced_blob"),
    postgres_pin: ("known_diffs.rs", "postgres_refuses_zero_since_on_a_referenced_blob"),
}];

async fn referenced_blob(e: &Env) -> String {
    let msg = raw("referenced", Some("r@x"));
    let h = rusty_core::BlobHash::of(&msg);
    e.mb.append(INBOX, msg, rusty_core::Flags::default())
        .await
        .unwrap();
    h.as_str().to_owned()
}

// backend-neutral: SQLite side of KNOWN_DIFFS D1
#[tokio::test]
async fn sqlite_accepts_zero_since_on_a_referenced_blob() {
    let e = Env::new(sqlite_backend().await).await;
    let h = referenced_blob(&e).await;
    let n = e
        .try_exec(
            &format!(
                "UPDATE blobs SET zero_since = 0 WHERE hash = {}",
                hash_lit(&e.b, &h)
            ),
            &[],
        )
        .await
        .expect("SQLite stores the inconsistent row");
    assert_eq!(n, 1);
}

// backend-neutral: Postgres side of KNOWN_DIFFS D1
#[tokio::test]
async fn postgres_refuses_zero_since_on_a_referenced_blob() {
    let Some(b) = pg_backend_or_skip().await else {
        return;
    };
    let e = Env::new(b).await;
    let h = referenced_blob(&e).await;
    let err = e
        .try_exec(
            &format!(
                "UPDATE blobs SET zero_since = 0 WHERE hash = {}",
                hash_lit(&e.b, &h)
            ),
            &[],
        )
        .await
        .expect_err("Postgres refuses the inconsistent row");
    assert_eq!(err.sqlstate.as_deref(), Some("23514"));
    assert_eq!(
        err.constraint.as_deref(),
        Some("blobs_zero_since_consistent")
    );
}

// backend-neutral: reads the test sources, no database
#[test]
fn known_diffs_are_pinned() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut seen = std::collections::HashSet::new();
    for d in KNOWN_DIFFS {
        assert!(seen.insert(d.id), "duplicate id {}", d.id);
        for (side, (file, name)) in [("sqlite", d.sqlite_pin), ("postgres", d.postgres_pin)] {
            let src = std::fs::read_to_string(dir.join(file))
                .unwrap_or_else(|_| panic!("{}: {side} pin file {file} is missing", d.id));
            assert!(
                src.contains(&format!("async fn {name}(")) || src.contains(&format!("fn {name}(")),
                "{}: {side} pin {file}::{name} does not exist",
                d.id
            );
        }
        assert!(!d.what.is_empty() && !d.sqlite.is_empty() && !d.postgres.is_empty());
        assert!(!d.reason.is_empty());
    }
    // every `KNOWN_DIFFS Dn` mention in a test file refers to a listed id
    for entry in std::fs::read_dir(&dir).unwrap() {
        let p = entry.unwrap().path();
        if p.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&p).unwrap();
        for part in src.split("KNOWN_DIFFS ").skip(1) {
            let id: String = part
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if id.starts_with('D') && id.len() > 1 && id[1..].chars().all(|c| c.is_ascii_digit()) {
                assert!(
                    seen.contains(id.as_str()),
                    "{}: unknown KNOWN_DIFFS id {id}",
                    p.display()
                );
            }
        }
    }
}
