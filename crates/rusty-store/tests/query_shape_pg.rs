//! Postgres statement-count budget and lock-first probe for `MailboxRepo`.
//!
//! The counts include the transaction-local `set_config('lock_timeout')` and COMMIT (BEGIN is not
//! logged by sqlx, add one round trip per write): every one is a network round trip (PG7 baseline: the
//! statement-merging optimisation starts from this table). The shapes are recorded by the sqlx
//! statement probe (`common::stmts`).
//!
//! `BUDGET` is a CEILING, not a golden value: a regression (more statements) fails the budget
//! test, an improvement does not. A separate snapshot test only complains when the measured
//! count is MORE than `SNAPSHOT_TOLERANCE` below the ceiling, so that the ceilings are tightened
//! after a real optimisation and cannot rot into a limit nobody would hit.
#![cfg(feature = "postgres")]

mod common;

use common::backends::pg_backend;
use common::shapes::{self, assert_lock_first};
use common::stmts;

/// Ceiling of statements per operation on Postgres as the sqlx probe sees them: the
/// transaction-local `set_config('lock_timeout')`, every statement and COMMIT. sqlx does not log
/// BEGIN as a query event, so a write is ONE more round trip than listed. Measured at the
/// repository boundary; a batch costs the same for 10 and 100 items by construction (asserted
/// below for every batched operation).
const BUDGET: &[(&str, usize)] = &[
    ("mailbox_for_account", 1),
    ("folder_exists", 1),
    ("create_folder", 9),
    ("rename_folder", 7),
    ("move_folder", 9),
    ("delete_folder (empty)", 11),
    ("delete_folder (3 expunged messages)", 12),
    ("append", 9),
    ("append (delivery, new)", 11),
    ("append (delivery, duplicate)", 5),
    ("append_many (1)", 9),
    ("append_many (10)", 9),
    ("append_many (100)", 9),
    ("deliver_many (10, new)", 11),
    ("deliver_many (100, new)", 11),
    ("deliver_many (10, all duplicates)", 5),
    ("set_flags", 6),
    ("set_flags_many (10)", 6),
    ("set_flags_many (100)", 6),
    ("add_keyword", 9),
    ("remove_keyword", 7),
    ("move_message", 8),
    ("move_many (10)", 9),
    ("move_many (100)", 9),
    ("expunge", 6),
    ("expunge_many (10)", 6),
    ("purge_deleted (11 rows)", 6),
    ("expunge_many (100)", 6),
    ("purge_deleted (100 rows)", 6),
    ("purge_deleted (nothing)", 4),
    ("list_folder", 2),
    ("changes_since", 1),
    ("list_folders", 1),
    ("get_message", 1),
    ("get_raw_ref", 1),
    ("keywords", 2),
    // JobRepo and GcRepo (PG5)
    ("db_now (jobs)", 1),
    ("enqueue (domain)", 5),
    ("enqueue (no domain)", 5),
    ("enqueue_batch (domain)", 6),
    ("claim (job available)", 5),
    ("claim (nothing)", 3),
    ("heartbeat", 3),
    ("fence_check", 1),
    ("fail", 3),
    ("reschedule", 3),
    ("complete", 3),
    ("purge_finished_batch", 3),
    ("db_now (gc)", 1),
    ("blob_candidates", 1),
    ("blob_rows_in_shard", 1),
    ("blob_row_exists", 1),
    ("delete_blob_rows (2 rows)", 4),
    ("purge_deleted_batch (1 message)", 7),
    ("purge_deliveries_batch", 3),
    ("purge_capabilities_batch", 3),
];

/// How far below its ceiling an operation may get before the ceiling counts as stale.
const SNAPSHOT_TOLERANCE: usize = 3;

// backend-neutral: Postgres statement budget and lock-first order; the SQLite statement shapes are pinned by query_shape.rs
#[tokio::test]
async fn statement_counts_stay_within_the_budget() {
    let p = stmts::probe();
    let Some(b) = pg_backend().await else { return };
    let all = shapes::collect(&b, &p).await;
    for s in &all {
        eprintln!("{:<42} {:>3}", s.label, s.stmts.len());
    }
    for (label, ceiling) in BUDGET {
        let s = all
            .iter()
            .find(|s| s.label == *label)
            .unwrap_or_else(|| panic!("no shape for {label}"));
        assert!(
            s.stmts.len() <= *ceiling,
            "{label}: {} statements exceed the budget of {ceiling}: {:#?}",
            s.stmts.len(),
            s.stmts
        );
    }
    assert_eq!(BUDGET.len(), all.len(), "every operation has a budget line");
}

/// The ceilings follow the code down: an improvement larger than `SNAPSHOT_TOLERANCE` statements
/// must tighten its ceiling (otherwise a later regression back to the old count would pass).
/// Smaller improvements need no edit.
// backend-neutral: Postgres statement budget and lock-first order; the SQLite statement shapes are pinned by query_shape.rs
#[tokio::test]
async fn budget_ceilings_are_not_stale() {
    let p = stmts::probe();
    let Some(b) = pg_backend().await else { return };
    let all = shapes::collect(&b, &p).await;
    for (label, ceiling) in BUDGET {
        let n = all.iter().find(|s| s.label == *label).unwrap().stmts.len();
        assert!(
            ceiling - n.min(*ceiling) <= SNAPSHOT_TOLERANCE,
            "{label}: measured {n}, ceiling {ceiling}: tighten the ceiling in BUDGET"
        );
    }
}

/// A batch is O(1) statements, not O(n): the same count for a small and a large batch, for
/// EVERY batched operation (the oracle that matters most).
// backend-neutral: Postgres statement budget and lock-first order; the SQLite statement shapes are pinned by query_shape.rs
#[tokio::test]
async fn batched_operations_cost_the_same_for_every_batch_size() {
    let p = stmts::probe();
    let Some(b) = pg_backend().await else { return };
    let all = shapes::collect(&b, &p).await;
    let n = |label: &str| {
        all.iter()
            .find(|s| s.label == label)
            .unwrap_or_else(|| panic!("no shape for {label}"))
            .stmts
            .len()
    };
    for (small, large) in [
        ("append_many (10)", "append_many (100)"),
        ("deliver_many (10, new)", "deliver_many (100, new)"),
        ("set_flags_many (10)", "set_flags_many (100)"),
        ("move_many (10)", "move_many (100)"),
        ("expunge_many (10)", "expunge_many (100)"),
        ("purge_deleted (11 rows)", "purge_deleted (100 rows)"),
    ] {
        assert_eq!(n(small), n(large), "{small} vs {large}");
    }
    assert!(n("append_many (1)") <= n("append_many (100)"));
}

/// Review risk 6.3.1: a write method that forgets the mailbox lock still passes every counter
/// test. Guard: the first data statement after BEGIN is the mailbox row lock, for EVERY write.
// backend-neutral: Postgres statement budget and lock-first order; the SQLite statement shapes are pinned by query_shape.rs
#[tokio::test]
async fn every_write_method_takes_the_mailbox_lock_first() {
    let p = stmts::probe();
    let Some(b) = pg_backend().await else { return };
    let all = shapes::collect(&b, &p).await;
    shapes::assert_every_trait_method_has_a_shape(&all);
    assert!(all.iter().filter(|s| s.write).count() >= 25);
    for s in &all {
        assert_lock_first("postgres", s);
    }
}

/// No session state outside a transaction (PgBouncer transaction pooling, decision D6): no
/// session-level SET, no temp tables, no advisory locks, no LISTEN, no cursors WITH HOLD, no SQL
/// PREPARE in anything a repository operation executes.
// backend-neutral: Postgres statement budget and lock-first order; the SQLite statement shapes are pinned by query_shape.rs
#[tokio::test]
async fn repository_statements_use_no_session_state() {
    let p = stmts::probe();
    let Some(b) = pg_backend().await else { return };
    let all = shapes::collect(&b, &p).await;
    for s in &all {
        for t in &s.stmts {
            let u = t.to_ascii_uppercase();
            for banned in [
                "SET SESSION",
                "SET ROLE",
                "TEMP TABLE",
                "TEMPORARY TABLE",
                // Session-level advisory locks outlive a transaction; the transaction-scoped
                // `pg_advisory_xact_lock` (job cap serialisation) is released at COMMIT and is fine.
                "PG_ADVISORY_LOCK",
                "PG_TRY_ADVISORY_LOCK",
                "PG_ADVISORY_UNLOCK",
                "LISTEN ",
                "NOTIFY ",
                "WITH HOLD",
                "PREPARE ",
                "DECLARE ",
                "DISCARD",
            ] {
                assert!(!u.contains(banned), "{}: {banned} in {t:?}", s.label);
            }
            if u.starts_with("SET ") || u.contains(" SET LOCAL") {
                assert!(u.starts_with("SET LOCAL"), "{}: session SET in {t:?}", s.label);
            }
            if u.contains("SET_CONFIG") {
                // Only the transaction-local form (`is_local = true`).
                assert!(t.contains(", true)"), "{}: session-level set_config in {t:?}", s.label);
            }
        }
    }
}
