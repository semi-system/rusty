//! D2 clock policy (`docs/superpowers/specs/2026-11-03-rusty-clock-policy.md`): the DATABASE
//! clock decides every correctness-relevant timestamp; the Rust clock is display-only; values
//! the protocol defines (client INTERNALDATE, `Date:` header) and the `_at(now)` test twins are
//! caller data and are stored verbatim.
//!
//! In a test process and its database share a host clock, so a behavioural test cannot tell the
//! two clocks apart by itself. Two things make the policy testable anyway: the `_at(now)` twins
//! with an absurd `now` (the caller value must win, verbatim), and a source scan that fails on
//! any `SystemTime::now` in the correctness code paths outside an explicit allow-list.

mod common;

use common::backends::*;
use common::{for_each_env, raw, INBOX};
use rusty_core::{DeliveryId, Flags};

/// The database's own notion of now.
async fn db_now(b: &Backend) -> i64 {
    if b.sqlite.is_some() {
        exec(b, "SELECT unixepoch()", &[]).await
    } else {
        exec(b, "SELECT rs_now()", &[]).await
    }
}

fn within(value: i64, lo: i64, hi: i64, what: &str) {
    assert!(
        (lo - 2..=hi + 2).contains(&value),
        "{what}: {value} not within [{lo}, {hi}] +-2s"
    );
}

#[tokio::test]
async fn directory_created_at_and_uid_validity_follow_the_database_clock() {
    for_each_backend(|b| async move {
        let lo = db_now(&b).await;
        let dom = b.dir.create_domain("clk.test").await.unwrap();
        let acc = b
            .dir
            .create_account("alice", dom, None, None)
            .await
            .unwrap();
        let hi = db_now(&b).await;
        let d = exec(
            &b,
            "SELECT created_at FROM domains WHERE id = ?1",
            &[Bind::I(dom.0)],
        )
        .await;
        within(d, lo, hi, "domains.created_at");
        let a = exec(
            &b,
            "SELECT created_at FROM accounts WHERE id = ?1",
            &[Bind::I(acc.0)],
        )
        .await;
        within(a, lo, hi, "accounts.created_at");
        // The mailbox UIDVALIDITY high-water mark is the database clock plus the 5 system
        // folders; folder i has base + i.
        let uv = exec(
            &b,
            "SELECT uid_validity FROM mailboxes WHERE account_id = ?1",
            &[Bind::I(acc.0)],
        )
        .await;
        within(uv, lo + 5, hi + 5, "mailboxes.uid_validity");
        let f1 = exec(
            &b,
            "SELECT f.uid_validity FROM folders f JOIN mailboxes m ON m.id = f.mailbox_id \
             WHERE m.account_id = ?1 AND f.id = 1",
            &[Bind::I(acc.0)],
        )
        .await;
        within(f1, lo + 1, hi + 1, "folders.uid_validity");
    })
    .await;
}

#[tokio::test]
async fn spike_append_internal_date_and_delivery_created_at_follow_the_database_clock() {
    for_each_backend(|b| async move {
        let (_, _, mb) = acct(&b, "clkapp").await;
        let lo = db_now(&b).await;
        let r = mb
            .append(INBOX, raw("a", Some("a@x")), Flags::empty())
            .await
            .unwrap();
        let did = DeliveryId([7u8; 32]);
        let r2 = match mb
            .deliver(INBOX, raw("b", Some("b@x")), Flags::empty(), did)
            .await
            .unwrap()
        {
            rusty_store::DeliverOutcome::Created(r) => r,
            other => panic!("{other:?}"),
        };
        let hi = db_now(&b).await;
        for (id, what) in [(r.id, "append"), (r2.id, "deliver")] {
            let d = exec(
                &b,
                "SELECT internal_date FROM messages WHERE mailbox_id = ?1 AND id = ?2",
                &[Bind::I(mb.id().0), Bind::I(id.0)],
            )
            .await;
            within(d, lo, hi, what);
        }
        let c = exec(
            &b,
            "SELECT created_at FROM deliveries WHERE mailbox_id = ?1",
            &[Bind::I(mb.id().0)],
        )
        .await;
        within(c, lo, hi, "deliveries.created_at");
    })
    .await;
}

#[tokio::test]
async fn blob_zero_since_is_set_from_the_database_clock() {
    for_each_backend(|b| async move {
        let lo = db_now(&b).await;
        // A blob row nobody references starts at refcount 0 and gets zero_since at insert.
        insert_blob_row(&b, &"aa".repeat(32), 1).await;
        let hi = db_now(&b).await;
        let z = exec(&b, "SELECT zero_since FROM blobs", &[]).await;
        within(z, lo, hi, "blobs.zero_since");
    })
    .await;
}

// ---------------------------------------------------------------- SQLite Mailbox facade

#[tokio::test]
async fn mailbox_append_internal_date_is_database_clock_and_at_twin_is_verbatim() {
    for_each_env(|e| async move {
        let lo = e.db_clock().await;
        let r =
            e.mb.append(INBOX, raw("a", Some("a@x")), Flags::empty())
                .await
                .unwrap();
        let hi = e.db_clock().await;
        let d = e
            .scalar(&format!(
                "SELECT internal_date FROM messages WHERE id = {}",
                r.id.0
            ))
            .await;
        within(d, lo, hi, "Mailbox::append internal_date");

        // The `_at(now)` twin: the caller's value is stored verbatim, also for internal_date
        // (client-supplied INTERNALDATE class), never replaced by the Rust or database clock.
        let did = DeliveryId([9u8; 32]);
        let out =
            e.mb.deliver_at(INBOX, raw("b", Some("b@x")), Flags::empty(), did, 1_000)
                .await
                .unwrap();
        let id = match out {
            rusty_store::DeliverOutcome::Created(r) => r.id,
            other => panic!("{other:?}"),
        };
        let d = e
            .scalar(&format!(
                "SELECT internal_date FROM messages WHERE id = {}",
                id.0
            ))
            .await;
        assert_eq!(
            d, 1_000,
            "internal_date must be the caller value of deliver_at"
        );
        let c = e.scalar("SELECT created_at FROM deliveries").await;
        assert_eq!(c, 1_000);
    })
    .await;
}

#[tokio::test]
async fn mailbox_expunge_and_folder_uid_validity_use_the_database_clock() {
    for_each_env(|e| async move {
        let r =
            e.mb.append(INBOX, raw("a", Some("a@x")), Flags::empty())
                .await
                .unwrap();
        let lo = e.db_clock().await;
        e.mb.expunge(r.id).await.unwrap();
        let f = e.mb.create_folder(None, "Later").await.unwrap();
        let hi = e.db_clock().await;
        let del = e
            .scalar(&format!(
                "SELECT deleted_at FROM messages WHERE id = {}",
                r.id.0
            ))
            .await;
        within(del, lo, hi, "messages.deleted_at");
        let uv = e
            .scalar(&format!(
                "SELECT uid_validity FROM folders WHERE id = {}",
                f.0
            ))
            .await;
        assert!(
            uv >= lo - 2,
            "folder uid_validity {uv} below the database clock {lo}"
        );
    })
    .await;
}

#[tokio::test]
async fn gc_grace_cutoff_comes_from_the_database_clock() {
    use std::time::Duration;
    for_each_env(|e| async move {
        let now = e.db_clock().await;
        for (name, age) in [("old", 3 * 3600), ("young", 1800)] {
            let h = format!(
                "{:0>64}",
                name.bytes().map(|b| format!("{b:02x}")).collect::<String>()
            );
            insert_blob_row(&e.b, &h, 1).await;
            // The insert trigger stamps zero_since with the database clock; age it afterwards.
            set_zero_since(&e.b, &h, now - age).await;
        }
        let rep =
            e.b.gc()
                .collect_blobs(e.blobs.as_ref(), Duration::from_secs(3600))
                .await
                .unwrap();
        assert_eq!(rep.rows_deleted, 1, "{rep:?}");
        let left = e.scalar("SELECT COUNT(*) FROM blobs").await;
        assert_eq!(left, 1);
    })
    .await;
}

// ---------------------------------------------------------------- source scan

/// No correctness path may read the Rust wall clock. The allow-list names the files where the
/// Rust clock is legitimate and says why.
// backend-neutral: source scan, no database
#[test]
fn no_rust_clock_in_correctness_paths() {
    // File modification times are stamped by the operating system, so the blob store compares
    // them against the OS clock (temp-file sweep, quarantine healing).
    const ALLOWED: &[(&str, &str)] = &[
        ("rusty-store/src/blob.rs", "file mtimes use the OS clock"),
        ("rusty-store/src/fixtures/pg_db.rs", "test-only (feature test-fixtures): names and reaps stale test databases"),
    ];
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut offenders = Vec::new();
    for krate in ["rusty-store", "rusty-directory"] {
        let mut stack = vec![crates.join(krate).join("src")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|x| x.to_str()) != Some("rs") {
                    continue;
                }
                let rel = path
                    .strip_prefix(&crates)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if ALLOWED.iter().any(|(f, _)| *f == rel) {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                for (i, line) in text.lines().enumerate() {
                    let code = line.split("//").next().unwrap_or("");
                    const CLOCK_READS: &[&str] = &[
                        "SystemTime::now",
                        "Instant::now",
                        ".elapsed()",
                        "Utc::now",
                        "Local::now",
                        "OffsetDateTime::now",
                    ];
                    if CLOCK_READS.iter().any(|p| code.contains(p)) {
                        offenders.push(format!("{rel}:{}: {}", i + 1, line.trim()));
                    }
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "Rust clock in correctness paths:\n{}",
        offenders.join("\n")
    );
}

// ---------------------------------------------------------------- jobs (SQLite until PG5)

/// Lease and schedule columns are decided by the database clock; the PG5 repository repeats
/// this test for Postgres through the same body.
#[tokio::test]
async fn job_lease_and_schedule_use_the_database_clock() {
    for_each_backend(|b| async move {
        let jq = b.jobs();
        let lo = db_now(&b).await;
        jq.enqueue("clock.test", None, &serde_json::json!({}), None)
            .await
            .unwrap();
        let job = jq
            .claim("w1", &["clock.test"], 60)
            .await
            .unwrap()
            .expect("job");
        let hi = db_now(&b).await;
        let col = |name: &'static str, id: i64| {
            let b = &b;
            async move {
                exec(
                    b,
                    &format!("SELECT {name} FROM jobs WHERE id = ?1"),
                    &[Bind::I(id)],
                )
                .await
            }
        };
        within(col("created_at", job.id.0).await, lo, hi, "jobs.created_at");
        within(col("run_after", job.id.0).await, lo, hi, "jobs.run_after");
        within(
            col("lease_expires_at", job.id.0).await,
            lo + 60,
            hi + 60,
            "jobs.lease_expires_at",
        );
        // A heartbeat extends from the database clock too.
        jq.heartbeat(job.id, job.epoch, 120).await.unwrap();
        within(
            col("lease_expires_at", job.id.0).await,
            lo + 120,
            hi + 120 + 2,
            "jobs.lease_expires_at after heartbeat",
        );
    })
    .await;
}

// ---------------------------------------------------------------- supplied timestamps are bounded

/// The `_at(now)` twins store caller data verbatim, so they must reject absurd values instead of
/// writing them (negative, or beyond `MAX_SUPPLIED_TIME`, the year 2100).
#[tokio::test]
async fn supplied_timestamps_are_bounded() {
    for_each_env(|e| async move {
        use rusty_store::{DeliverItem, MAX_SUPPLIED_TIME};
        for (i, bad) in [-1i64, i64::MIN, MAX_SUPPLIED_TIME + 1, i64::MAX]
            .into_iter()
            .enumerate()
        {
            let did = DeliveryId([i as u8 + 40; 32]);
            let r =
                e.mb.deliver_at(INBOX, raw("x", Some("x@x")), Flags::empty(), did, bad)
                    .await;
            assert!(
                matches!(r, Err(rusty_core::Error::Invalid(_))),
                "deliver_at({bad}): {r:?}"
            );
            let item = DeliverItem {
                raw: raw("y", Some("y@x")),
                flags: Flags::empty(),
                delivery_id: DeliveryId([i as u8 + 60; 32]),
            };
            let r = e.mb.deliver_many_at(INBOX, vec![item], bad).await;
            assert!(
                matches!(r, Err(rusty_core::Error::Invalid(_))),
                "deliver_many_at({bad}): {r:?}"
            );
            let r = e.mb.expunge_many_at(&[rusty_core::MessageId(1)], bad).await;
            assert!(
                matches!(r, Err(rusty_core::Error::Invalid(_))),
                "expunge_many_at({bad}): {r:?}"
            );
        }
        // Nothing was written by the rejected calls, and the bounds themselves are accepted.
        assert_eq!(e.scalar("SELECT COUNT(*) FROM messages").await, 0);
        for (i, ok) in [0i64, MAX_SUPPLIED_TIME].into_iter().enumerate() {
            e.mb.deliver_at(
                INBOX,
                raw("z", Some("z@x")),
                Flags::empty(),
                DeliveryId([i as u8 + 80; 32]),
                ok,
            )
            .await
            .unwrap();
        }
    })
    .await;
}

/// Behavioural proof that the GC cutoff follows the DATABASE clock: the database clock is skewed
/// (by a query that returns a different "now" than the process sees) while rows are aged relative
/// to the real time. A cutoff taken from the process clock would give the opposite answers.
#[tokio::test]
async fn gc_cutoff_follows_a_skewed_database_clock() {
    use std::time::{Duration, UNIX_EPOCH};
    for (skew, expect_deleted, label) in [
        // DB clock a year ahead: even a 30-minute-old zero_since is past the 1h grace -> deleted.
        (31_536_000i64, 2u64, "database clock ahead"),
        // DB clock a year behind: a 3h-old row is not older than the grace window -> kept.
        (-31_536_000i64, 0u64, "database clock behind"),
    ] {
        for_each_env(|e| async move {
            let now = e.db_clock().await;
            for (name, age) in [("old", 3 * 3600), ("young", 1800)] {
                let h = format!(
                    "{:0>64}",
                    name.bytes().map(|b| format!("{b:02x}")).collect::<String>()
                );
                insert_blob_row(&e.b, &h, 1).await;
                set_zero_since(&e.b, &h, now - age).await;
            }
            // "now" as the skewed database clock reports it, judged by the shared GC body.
            let skewed = UNIX_EPOCH + Duration::from_secs(u64::try_from(now + skew).unwrap());
            let rep =
                e.b.gc()
                    .collect_blobs_at(e.blobs.as_ref(), Duration::from_secs(3600), skewed)
                    .await
                    .unwrap();
            assert_eq!(rep.rows_deleted, expect_deleted, "{label}: {rep:?}");
        })
        .await;
    }
}
