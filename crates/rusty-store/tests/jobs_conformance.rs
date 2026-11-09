//! JobRepo conformance: every job state transition, on both backends, one body.
mod common;

use common::backends::*;
use rusty_core::{DeliveryId, Error, Flags, JobId};
use rusty_store::JobFence;
use serde_json::{json, Value};

const STALE: &str = "stale job epoch";

fn is_stale<T: std::fmt::Debug>(r: &rusty_core::Result<T>) -> bool {
    matches!(r, Err(Error::Conflict(m)) if m == STALE)
}

async fn state(b: &Backend, id: JobId) -> String {
    text(b, &format!("SELECT state FROM jobs WHERE id = {}", id.0)).await
}

async fn col(b: &Backend, c: &str, id: JobId) -> i64 {
    scalar(b, &format!("SELECT {c} FROM jobs WHERE id = {}", id.0)).await
}

async fn last_error(b: &Backend, id: JobId) -> String {
    text(b, &format!("SELECT last_error FROM jobs WHERE id = {}", id.0)).await
}

fn nested(depth: usize) -> Value {
    let mut v = json!(1);
    for _ in 0..depth {
        v = Value::Array(vec![v]);
    }
    v
}

async fn enq(b: &Backend, kind: &str, run_after: Option<i64>, now: i64) -> JobId {
    b.jobs()
        .enqueue_at(kind, None, &json!({"k": kind}), run_after, now)
        .await
        .unwrap()
}

#[tokio::test]
async fn enqueue_validation_and_limits() {
    for_each_backend(|b| async move {
        let jq = b.jobs();
        let n = b.name;
        for bad in ["", "UPPER", "has space", &"a".repeat(65)] {
            assert!(
                matches!(jq.enqueue(bad, None, &json!({}), None).await, Err(Error::Invalid(_))),
                "{n} {bad}"
            );
        }
        jq.enqueue(&"a".repeat(64), None, &json!({}), None).await.unwrap();
        for bad in [0u32, 21] {
            assert!(
                matches!(
                    jq.enqueue_with_max_attempts("t", None, &json!({}), None, bad).await,
                    Err(Error::Invalid(_))
                ),
                "{n}"
            );
        }
        // Depth 64 is accepted and reads back through claim; 65 is refused.
        let id = jq.enqueue("deep", None, &nested(64), None).await.unwrap();
        let got = jq.claim("w", &["deep"], 30).await.unwrap().unwrap();
        assert_eq!(got.id, id, "{n}");
        assert_eq!(got.payload, nested(64), "{n}");
        assert!(
            matches!(jq.enqueue("deep", None, &nested(65), None).await, Err(Error::Invalid(_))),
            "{n}"
        );
        // 64 KiB boundary on the serialized text (a JSON string adds two quote bytes).
        let fill = |len: usize| json!("x".repeat(len));
        jq.enqueue("big", None, &fill(65536 - 2), None).await.unwrap();
        assert!(
            matches!(jq.enqueue("big", None, &fill(65536 - 1), None).await, Err(Error::Invalid(_))),
            "{n}"
        );
        // Unknown domain: foreign key.
        assert!(
            matches!(
                jq.enqueue("t", Some(rusty_core::DomainId(9999)), &json!({}), None).await,
                Err(Error::Conflict(_))
            ),
            "{n}"
        );
    })
    .await;
}

#[tokio::test]
async fn claim_orders_by_run_after_then_id_filters_kind_and_respects_schedule() {
    for_each_backend(|b| async move {
        let jq = b.jobs();
        let n = b.name;
        let a = enq(&b, "t", Some(100), 1000).await;
        let c = enq(&b, "t", Some(50), 1000).await;
        let d = enq(&b, "t", Some(50), 1000).await;
        let other = enq(&b, "other", Some(1), 1000).await;
        let future = enq(&b, "t", Some(5000), 1000).await;
        let mut order = vec![];
        while let Some(j) = jq.claim_at("w", &["t"], 30, 1000).await.unwrap() {
            order.push(j.id);
            jq.complete_at(j.id, j.epoch, 1000).await.unwrap();
        }
        assert_eq!(order, vec![c, d, a], "{n}");
        assert_eq!(state(&b, other).await, "queued", "{n} kind filter");
        assert_eq!(state(&b, future).await, "queued", "{n}");
        let j = jq.claim_at("w", &["t"], 30, 5000).await.unwrap().unwrap();
        assert_eq!(j.id, future, "{n}");
        for bad in [&[][..], &["UPPER"][..], &["a"; 17][..]] {
            assert!(matches!(jq.claim_at("w", bad, 30, 1).await, Err(Error::Invalid(_))), "{n}");
        }
        assert!(
            matches!(jq.claim_at("bad worker", &["t"], 30, 1).await, Err(Error::Invalid(_))),
            "{n}"
        );
        for lease in [0u32, 86_401, u32::MAX] {
            assert!(
                matches!(jq.claim_at("w", &["t"], lease, 1).await, Err(Error::Invalid(_))),
                "{n}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn lifecycle_complete_fail_backoff_and_dead_letter() {
    for_each_backend(|b| async move {
        let jq = b.jobs();
        let n = b.name;
        let id = enq(&b, "t", None, 1000).await;
        let j = jq.claim_at("w", &["t"], 30, 1000).await.unwrap().unwrap();
        assert_eq!((j.epoch, j.attempts), (1, 1), "{n}");
        assert_eq!(state(&b, id).await, "running", "{n}");
        jq.complete_at(id, j.epoch, 1001).await.unwrap();
        assert_eq!(state(&b, id).await, "done", "{n}");
        assert!(is_stale(&jq.complete_at(id, j.epoch, 1002).await), "{n}");

        // fail: requeue with backoff arithmetic from `now`, then dead at max_attempts.
        let id = jq
            .enqueue_with_max_attempts_at("f", None, &json!({}), None, 2, 2000)
            .await
            .unwrap();
        let j1 = jq.claim_at("w", &["f"], 30, 2000).await.unwrap().unwrap();
        jq.fail_at(id, j1.epoch, "first\u{202e}err", 60, 2001).await.unwrap();
        assert_eq!(state(&b, id).await, "queued", "{n}");
        assert_eq!(col(&b, "run_after", id).await, 2061, "{n}");
        assert_eq!(last_error(&b, id).await, "firsterr", "{n}");
        assert!(jq.claim_at("w", &["f"], 30, 2060).await.unwrap().is_none(), "{n}");
        let j2 = jq.claim_at("w", &["f"], 30, 2061).await.unwrap().unwrap();
        assert_eq!((j2.epoch, j2.attempts), (2, 2), "{n}");
        jq.fail_at(id, j2.epoch, "second", 60, 2062).await.unwrap();
        assert_eq!(state(&b, id).await, "dead", "{n}");
        assert!(jq.claim_at("w", &["f"], 30, 99_999).await.unwrap().is_none(), "{n}");

        // Backoff is clamped to seven days; a long error is cut to 512 bytes.
        let id = enq(&b, "c", None, 3000).await;
        let j = jq.claim_at("w", &["c"], 30, 3000).await.unwrap().unwrap();
        jq.fail_at(id, j.epoch, &"\u{e9}".repeat(400), u32::MAX, 3001).await.unwrap();
        assert_eq!(col(&b, "run_after", id).await, 3001 + 7 * 86400, "{n}");
        assert_eq!(last_error(&b, id).await.len(), 512, "{n}");
    })
    .await;
}

#[tokio::test]
async fn lease_expiry_epoch_fencing_and_zombies() {
    for_each_backend(|b| async move {
        let jq = b.jobs();
        let n = b.name;
        let id = enq(&b, "t", None, 1000).await;
        let zombie = jq.claim_at("w1", &["t"], 30, 1000).await.unwrap().unwrap();
        // Lease still valid: nobody else can claim it.
        assert!(jq.claim_at("w2", &["t"], 30, 1029).await.unwrap().is_none(), "{n}");
        // Heartbeat extends; an expired lease is refused even before a re-claim.
        jq.heartbeat_at(id, zombie.epoch, 30, 1020).await.unwrap();
        assert_eq!(col(&b, "lease_expires_at", id).await, 1050, "{n}");
        assert!(is_stale(&jq.heartbeat_at(id, zombie.epoch, 30, 1050).await), "{n} expired lease");
        assert!(is_stale(&jq.complete_at(id, zombie.epoch, 1050).await), "{n}");
        assert!(is_stale(&jq.fail_at(id, zombie.epoch, "x", 1, 1050).await), "{n}");
        let fz = JobFence { id, epoch: zombie.epoch };
        assert!(is_stale(&jq.fence_check_at(fz, 1050).await), "{n}");
        // Re-claim: higher epoch; the zombie is stale for everything.
        let owner = jq.claim_at("w2", &["t"], 30, 1050).await.unwrap().unwrap();
        assert_eq!((owner.id, owner.epoch, owner.attempts), (id, 2, 2), "{n}");
        assert!(is_stale(&jq.heartbeat_at(id, zombie.epoch, 30, 1051).await), "{n}");
        assert!(is_stale(&jq.complete_at(id, zombie.epoch, 1051).await), "{n}");
        assert!(is_stale(&jq.fail_at(id, zombie.epoch, "x", 1, 1051).await), "{n}");
        assert!(is_stale(&jq.fence_check_at(fz, 1051).await), "{n}");
        jq.fence_check_at(JobFence { id, epoch: owner.epoch }, 1051).await.unwrap();
        let unknown = JobFence { id: JobId(4242), epoch: 1 };
        assert!(is_stale(&jq.fence_check_at(unknown, 1051).await), "{n}");
        jq.complete_at(id, owner.epoch, 1052).await.unwrap();
        let fo = JobFence { id, epoch: owner.epoch };
        assert!(is_stale(&jq.fence_check_at(fo, 1053).await), "{n} finished");
        // Last attempt with a lapsed lease: the next claim parks it dead and returns None.
        let id = jq
            .enqueue_with_max_attempts_at("l", None, &json!({}), None, 1, 2000)
            .await
            .unwrap();
        jq.claim_at("w", &["l"], 30, 2000).await.unwrap().unwrap();
        assert!(jq.claim_at("w", &["l"], 30, 2031).await.unwrap().is_none(), "{n}");
        assert_eq!(state(&b, id).await, "dead", "{n}");
        assert_eq!(last_error(&b, id).await, "lease expired on the last attempt", "{n}");
    })
    .await;
}

#[tokio::test]
async fn lease_and_schedule_follow_the_database_clock() {
    for_each_backend(|b| async move {
        let jq = b.jobs();
        let n = b.name;
        let lo = db_clock(&b).await;
        let id = jq.enqueue("t", None, &json!({}), None).await.unwrap();
        let j = jq.claim("w", &["t"], 60).await.unwrap().unwrap();
        let hi = db_clock(&b).await;
        for (c, base) in [("created_at", 0), ("run_after", 0), ("lease_expires_at", 60)] {
            let v = col(&b, c, id).await;
            assert!(
                (lo + base - 2..=hi + base + 2).contains(&v),
                "{n} {c} {v} not near the DB clock {lo}..{hi}"
            );
        }
        jq.heartbeat(id, j.epoch, 120).await.unwrap();
        let v = col(&b, "lease_expires_at", id).await;
        assert!((lo + 118..=hi + 124).contains(&v), "{n} heartbeat {v}");
        jq.complete(id, j.epoch).await.unwrap();
        assert_eq!(state(&b, id).await, "done", "{n}");
    })
    .await;
}

#[tokio::test]
async fn poisoned_and_exhausted_rows_are_parked_not_claimed() {
    for_each_backend(|b| async move {
        let jq = b.jobs();
        let n = b.name;
        let deep = format!("{}1{}", "[".repeat(70), "]".repeat(70));
        run(
            &b,
            &format!(
                "INSERT INTO jobs (kind, payload, run_after, created_at, updated_at) \
                 VALUES ('t', '{deep}', 0, 0, 0)"
            ),
        )
        .await;
        run(
            &b,
            "INSERT INTO jobs (kind, payload, attempts, max_attempts, run_after, created_at, updated_at) \
             VALUES ('t', '{}', 5, 5, 0, 0, 0)",
        )
        .await;
        let good = enq(&b, "t", Some(10), 1000).await;
        let j = jq.claim_at("w", &["t"], 30, 1000).await.unwrap().unwrap();
        assert_eq!(j.id, good, "{n}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM jobs WHERE state = 'dead'").await, 2, "{n}");
        assert_eq!(
            text(&b, "SELECT last_error FROM jobs WHERE payload LIKE '[[[%'").await,
            "unreadable payload",
            "{n}"
        );
        assert_eq!(
            text(&b, "SELECT last_error FROM jobs WHERE payload = '{}' AND state = 'dead'").await,
            "no attempts left",
            "{n}"
        );
    })
    .await;
}

#[tokio::test]
async fn claim_settles_at_most_100_exhausted_rows_per_call() {
    for_each_backend(|b| async move {
        let jq = b.jobs();
        let n = b.name;
        for _ in 0..150 {
            run(
                &b,
                "INSERT INTO jobs (kind, payload, attempts, max_attempts, run_after, created_at, updated_at) \
                 VALUES ('t', '{}', 5, 5, 0, 0, 0)",
            )
            .await;
        }
        let good = enq(&b, "t", Some(10), 1000).await;
        assert!(jq.claim_at("w", &["t"], 30, 1000).await.unwrap().is_none(), "{n}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM jobs WHERE state = 'dead'").await, 100, "{n}");
        let j = jq.claim_at("w", &["t"], 30, 1000).await.unwrap().unwrap();
        assert_eq!(j.id, good, "{n}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM jobs WHERE state = 'dead'").await, 150, "{n}");
    })
    .await;
}

#[tokio::test]
async fn pending_job_caps_are_ten_thousand_per_scope_and_free_up() {
    for_each_backend(|b| async move {
        let n = b.name;
        assert_eq!(rusty_store::MAX_PENDING_JOBS_PER_DOMAIN, 10_000);
        assert_eq!(rusty_store::MAX_PENDING_JOBS_GLOBAL_NO_DOMAIN, 10_000);
        let jq = b.jobs();
        // Production constants: fill a domain, then the no-domain scope, to exactly 10,000.
        let dom = b.dir.create_domain("caps.test").await.unwrap();
        for d in [format!("{}", dom.0), "NULL".to_owned()] {
            run(
                &b,
                &format!(
                    "WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM s WHERE i < 10000) \
                     INSERT INTO jobs (kind, domain_id, payload, run_after, created_at, updated_at) \
                     SELECT 't', {d}, '{{}}', 0, 0, 0 FROM s"
                ),
            )
            .await;
        }
        for d in [Some(dom), None] {
            let r = jq.enqueue("t", d, &json!({}), None).await;
            assert!(matches!(r, Err(Error::Quota(ref m)) if m == "job queue full"), "{n} {d:?}");
        }
        // A finished job frees a slot (small caps).
        let small = b.jobs_with_limits(2, 2);
        let dom2 = b.dir.create_domain("caps2.test").await.unwrap();
        let a = small.enqueue_at("s", Some(dom2), &json!({}), None, 10).await.unwrap();
        small.enqueue_at("s", Some(dom2), &json!({}), None, 10).await.unwrap();
        let full = small.enqueue_at("s", Some(dom2), &json!({}), None, 10).await;
        assert!(matches!(full, Err(Error::Quota(_))), "{n}");
        let j = small.claim_at("w", &["s"], 30, 10).await.unwrap().unwrap();
        assert_eq!(j.id, a, "{n}");
        let full = small.enqueue_at("s", Some(dom2), &json!({}), None, 10).await;
        assert!(matches!(full, Err(Error::Quota(_))), "running still counts {n}");
        small.complete_at(a, j.epoch, 11).await.unwrap();
        small.enqueue_at("s", Some(dom2), &json!({}), None, 12).await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn purge_finished_removes_only_old_finished_rows() {
    for_each_backend(|b| async move {
        let jq = b.jobs();
        let n = b.name;
        let old = enq(&b, "t", None, 100).await;
        let j = jq.claim_at("w", &["t"], 30, 100).await.unwrap().unwrap();
        jq.complete_at(old, j.epoch, 100).await.unwrap();
        let fresh = enq(&b, "t", None, 100_000).await;
        let j = jq.claim_at("w", &["t"], 30, 100_000).await.unwrap().unwrap();
        jq.complete_at(fresh, j.epoch, 100_000).await.unwrap();
        let queued = enq(&b, "q", None, 50).await;
        assert_eq!(jq.purge_finished(1000).await.unwrap(), 1, "{n}");
        assert_eq!(state(&b, fresh).await, "done", "{n}");
        assert_eq!(state(&b, queued).await, "queued", "{n}");
        // More than one batch.
        for _ in 0..1500 {
            run(
                &b,
                "INSERT INTO jobs (kind, state, payload, run_after, created_at, updated_at) \
                 VALUES ('t', 'dead', '{}', 0, 0, 5)",
            )
            .await;
        }
        assert_eq!(jq.purge_finished(1000).await.unwrap(), 1500, "{n}");
        // The DB-clock default with the seven-day retention keeps a fresh row.
        jq.purge_finished(i64::MAX).await.unwrap();
        let f = enq(&b, "t", None, 100).await;
        let now = db_clock(&b).await;
        run(&b, &format!("UPDATE jobs SET state='done', updated_at = {now} WHERE id = {}", f.0)).await;
        assert_eq!(jq.purge_finished_default().await.unwrap(), 0, "{n}");
        run(&b, &format!("UPDATE jobs SET updated_at = {} WHERE id = {}", now - 8 * 86400, f.0)).await;
        assert_eq!(jq.purge_finished_default().await.unwrap(), 1, "{n}");
    })
    .await;
}

/// Fenced mailbox effects: the effect and the completion commit together or not at all, and a
/// lease that lapsed by the DATABASE clock refuses the effect even before a re-claim.
#[tokio::test]
async fn fenced_delivery_commits_with_completion_and_rolls_back_for_zombies() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "fence").await;
        let jq = b.jobs();
        let id = jq.enqueue("deliver", None, &json!({}), None).await.unwrap();
        let first = jq.claim("w1", &["deliver"], 30).await.unwrap().unwrap();
        run(&b, &format!("UPDATE jobs SET lease_expires_at = 1 WHERE id = {}", id.0)).await;
        let msg = common::raw("fenced", None);
        let did = DeliveryId([7; 32]);
        let r = mb
            .deliver_fenced(INBOX, msg.clone(), Flags::default(), did, JobFence { id, epoch: first.epoch })
            .await;
        assert!(is_stale(&r), "{n} lapsed lease: {r:?}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, 0, "{n}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM deliveries").await, 0, "{n}");
        // Re-claim: the old epoch is stale, the new one commits effect and completion.
        let second = jq.claim("w2", &["deliver"], 30).await.unwrap().unwrap();
        assert_eq!(second.epoch, first.epoch + 1, "{n}");
        let r = mb
            .deliver_fenced(INBOX, msg.clone(), Flags::default(), did, JobFence { id, epoch: first.epoch })
            .await;
        assert!(is_stale(&r), "{n}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, 0, "{n}");
        mb.deliver_fenced(INBOX, msg, Flags::default(), did, JobFence { id, epoch: second.epoch })
            .await
            .unwrap();
        assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, 1, "{n}");
        assert_eq!(state(&b, id).await, "done", "{n}");
    })
    .await;
}

/// Review PG5 M-c: a fence is bound to the job's domain. A worker holding the fence of a job of
/// domain A cannot deliver into a mailbox of domain B (defence in depth: claim is global by
/// kind). The refusal is `Invalid`, rolls everything back, leaves the job running, and a job
/// without a domain (a system job) is not restricted.
#[tokio::test]
async fn a_fence_cannot_deliver_into_another_domains_mailbox() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (dom_a, _, mb_a) = acct(&b, "fencea").await;
        let (_, _, mb_b) = acct(&b, "fenceb").await;
        let jq = b.jobs();
        let id = jq.enqueue("deliver", Some(dom_a), &json!({}), None).await.unwrap();
        let job = jq.claim("w", &["deliver"], 60).await.unwrap().unwrap();
        let fence = JobFence { id, epoch: job.epoch };
        let did = DeliveryId([5; 32]);
        let msg = common::raw("cross-domain", None);
        let r = mb_b.deliver_fenced(INBOX, msg.clone(), Flags::default(), did, fence).await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{n}: wrong domain: {r:?}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, 0, "{n}: rolled back");
        assert_eq!(scalar(&b, "SELECT count(*) FROM deliveries").await, 0, "{n}");
        assert_eq!(state(&b, id).await, "running", "{n}: the job is untouched");
        // The same fence delivers into its own domain's mailbox.
        mb_a.deliver_fenced(INBOX, msg, Flags::default(), did, fence).await.unwrap();
        assert_eq!(state(&b, id).await, "done", "{n}");
        // A system job (no domain) may deliver anywhere.
        let sys = jq.enqueue("deliver", None, &json!({}), None).await.unwrap();
        let sj = jq.claim("w", &["deliver"], 60).await.unwrap().unwrap();
        assert_eq!(sj.id, sys);
        mb_b.deliver_fenced(INBOX, common::raw("system", None), Flags::default(), DeliveryId([6; 32]),
            JobFence { id: sys, epoch: sj.epoch })
            .await
            .unwrap();
        assert_eq!(state(&b, sys).await, "done", "{n}");
    })
    .await;
}

/// Review PG5 M-b: the pending-job cap is a property of the repository, not of whatever the
/// caller passes: a holder of `Repos.jobs` that asks for `cap = u32::MAX` still hits the
/// documented 10,000 per scope (a domain, or no domain).
#[tokio::test]
async fn the_repository_bounds_a_caller_supplied_cap() {
    for_each_backend(|b| async move {
        let n = b.name;
        let dom = b.dir.create_domain("capdom.test").await.unwrap();
        for domain in [None, Some(dom)] {
            let dcol = domain.map_or("NULL".to_owned(), |d| d.0.to_string());
            let sql = if b.name == "postgres" {
                format!(
                    "INSERT INTO jobs (kind, domain_id, state, payload, attempts, max_attempts, run_after, \
                     created_at, updated_at) SELECT 'cap', {dcol}, 'queued', '{{}}', 0, 5, 0, 0, 0 \
                     FROM generate_series(1, 10000)"
                )
            } else {
                format!(
                    "WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM s WHERE i < 10000) \
                     INSERT INTO jobs (kind, domain_id, state, payload, attempts, max_attempts, run_after, \
                     created_at, updated_at) SELECT 'cap', {dcol}, 'queued', '{{}}', 0, 5, 0, 0, 0 FROM s"
                )
            };
            run(&b, &sql).await;
            let job = rusty_store::repo::NewJob {
                kind: "cap",
                domain,
                payload: "{}",
                run_after: None,
                max_attempts: 5,
                cap: u32::MAX,
                now: None,
            };
            let r = b.jrepo.enqueue(&job).await;
            assert!(
                matches!(r, Err(Error::Quota(_))),
                "{n} {domain:?}: 10,000 pending is the ceiling whatever cap is asked for: {r:?}"
            );
        }
    })
    .await;
}

/// Mutation R03 (claim with the DATABASE clock treats a live lease as expired): the existing
/// lease tests use the `_at(now)` twins, which bind the clock and never reach the clock default.
#[tokio::test]
async fn a_live_lease_is_not_reclaimed_by_the_database_clock_claim() {
    for_each_backend(|b| async move {
        let n = b.name;
        let jq = b.jobs();
        let id = jq.enqueue("live", None, &json!({}), None).await.unwrap();
        let first = jq.claim("w1", &["live"], 3600).await.unwrap().expect("first claim");
        assert_eq!(first.id, id);
        let second = jq.claim("w2", &["live"], 3600).await.unwrap();
        assert!(second.is_none(), "{n}: a running job with a live lease must not be claimed again");
        assert_eq!(col(&b, "epoch", id).await, 1, "{n}: the epoch did not move");
    })
    .await;
}

/// U5: a long-lived job (an outbound message retried for days) needs a retry that does not spend
/// the attempt budget, carries an updated payload, and a way to know its age by the database clock.
#[tokio::test]
async fn reschedule_resets_attempts_replaces_payload_and_is_fenced() {
    for_each_backend(|b| async move {
        let jq = b.jobs();
        let n = b.name;
        let id = jq
            .enqueue_with_max_attempts_at("r", None, &json!({"tries": 0}), None, 3, 5000)
            .await
            .unwrap();
        let mut epoch = 0;
        for round in 0..6i64 {
            let now = 5000 + round * 100;
            let j = jq.claim_at("w", &["r"], 30, now).await.unwrap().unwrap();
            assert_eq!(j.created_at, 5000, "{n}: created_at is the enqueue time, stable across retries");
            assert_eq!(j.attempts, 1, "{n}: attempts restart after a reschedule");
            assert_eq!(j.payload, json!({"tries": round}), "{n}");
            assert!(j.epoch > epoch, "{n}: every claim raises the epoch");
            epoch = j.epoch;
            jq.reschedule_at(id, j.epoch, &json!({"tries": round + 1}), "try later", 50, now + 1)
                .await
                .unwrap();
            assert_eq!(state(&b, id).await, "queued", "{n}");
            assert_eq!(col(&b, "run_after", id).await, now + 51, "{n}");
            assert_eq!(last_error(&b, id).await, "try later", "{n}");
            // The old epoch is a zombie now.
            assert!(is_stale(&jq.reschedule_at(id, j.epoch, &json!({}), "x", 1, now + 2).await), "{n}");
        }
        // A lapsed lease cannot reschedule either.
        let j = jq.claim_at("w", &["r"], 30, 9000).await.unwrap().unwrap();
        assert!(is_stale(&jq.reschedule_at(id, j.epoch, &json!({}), "x", 1, 9031).await), "{n}");
        // Payload limits still apply.
        let j = jq.claim_at("w", &["r"], 30, 9100).await.unwrap().unwrap();
        assert!(jq.reschedule_at(id, j.epoch, &nested(65), "x", 1, 9101).await.is_err(), "{n}");
    })
    .await;
}
