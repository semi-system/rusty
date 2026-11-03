//! PG2 spike conformance: ONE test body per behaviour, run against every available backend
//! (SQLite always; Postgres when `RUSTY_TEST_PG_URL` is set, otherwise a loud SKIPPED notice
//! from `pg_admin_url`). The bodies only use the backend-neutral facades, never SQL, except
//! the few raw fixtures (alias/list rows) that the slice has no write API for.

mod common;

use bytes::Bytes;
use common::backends::*;
use common::{raw, reply};
use rusty_core::{DeliveryId, DomainId, Error, Flags, FolderId};
use rusty_store::repo::{JobFence, ListMember, NewAccount, Resolution};
use rusty_store::{ChangeItem, ChangeOp, DeliverOutcome};

#[tokio::test]
async fn create_domain_and_account_duplicates_and_invalid() {
    for_each_backend(|b| async move {
        let n = b.name;
        let d = b.dir.create_domain("example.org").await.unwrap();
        assert!(is_conflict(b.dir.create_domain("example.org").await), "{n}");
        // Bad domain: rejected by the facade (stand-in for normalize_domain) ...
        for bad in ["", "Example.org", "a b.org", "x@y.org"] {
            assert!(
                matches!(b.dir.create_domain(bad).await, Err(Error::Invalid(_))),
                "{n}: {bad:?}"
            );
        }
        // ... and, if the facade is bypassed, by the database CHECK through the SQLSTATE /
        // error-kind mapping (23514 on Postgres, CHECK on SQLite) -> Invalid on both.
        assert!(
            matches!(b.drepo.create_domain("UPPER.org").await, Err(Error::Invalid(_))),
            "{n}: db check"
        );
        let a = b.dir.create_account("bob", d, Some("hash".into()), None).await;
        assert!(a.is_ok(), "{n}: {a:?}");
        // Duplicate address: RS001 (Postgres trigger) / `conflict:` (SQLite trigger).
        assert!(is_conflict(b.dir.create_account("bob", d, None, None).await), "{n}");
        // Unknown domain: FK violation -> Conflict on both.
        assert!(
            is_conflict(b.dir.create_account("bob", DomainId(9999), None, None).await),
            "{n}"
        );
        // Uppercase local part bypassing the facade: CHECK -> Invalid.
        let direct = b
            .drepo
            .create_account(&NewAccount {
                local: "Bob2".into(),
                domain: d,
                plan: None,
                password_hash: None,
            })
            .await;
        assert!(matches!(direct, Err(Error::Invalid(_))), "{n}: {direct:?}");
    })
    .await;
}

#[tokio::test]
async fn system_folders_are_1_to_5_and_custom_folders_follow() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "sys").await;
        for id in 1..=5 {
            b.mrepo.folder_exists(mb.id(), FolderId(id)).await.unwrap();
        }
        assert!(matches!(
            b.mrepo.folder_exists(mb.id(), FolderId(6)).await,
            Err(Error::NotFound(_))
        ));
        let f = mb.create_folder(None, "Projects").await.unwrap();
        assert_eq!(f, FolderId(6), "{n}");
        let child = mb.create_folder(Some(f), "2026").await.unwrap();
        assert_eq!(child, FolderId(7), "{n}");
        // Duplicate sibling name -> Conflict; unknown parent -> NotFound; bad name -> Invalid.
        assert!(is_conflict(mb.create_folder(None, "Projects").await), "{n}");
        assert!(matches!(
            mb.create_folder(Some(FolderId(99)), "x").await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            mb.create_folder(None, "a/b").await,
            Err(Error::Invalid(_))
        ));
        // Depth limit 64.
        let mut p = child;
        let mut depth = 2;
        let err = loop {
            match mb.create_folder(Some(p), "d").await {
                Ok(f) => {
                    p = f;
                    depth += 1;
                }
                Err(e) => break e,
            }
        };
        assert!(matches!(err, Error::Invalid(_)), "{n}: {err:?}");
        assert_eq!(depth, 64, "{n}");
    })
    .await;
}

#[tokio::test]
async fn resolve_account_alias_list_and_unknown() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (dom, acc, _) = acct(&b, "res").await;
        assert_eq!(
            b.dir.resolve("alice", "res.test").await.unwrap(),
            Resolution::Account(acc),
            "{n}"
        );
        exec(
            &b,
            "INSERT INTO aliases (local_part, domain_id, account_id) VALUES (?1, ?2, ?3) \
             RETURNING id",
            &[Bind::S("al"), Bind::I(dom.0), Bind::I(acc.0)],
        )
        .await;
        assert_eq!(
            b.dir.resolve("al", "res.test").await.unwrap(),
            Resolution::Account(acc),
            "{n}"
        );
        let list = exec(
            &b,
            "INSERT INTO lists (local_part, domain_id) VALUES (?1, ?2) RETURNING id",
            &[Bind::S("team"), Bind::I(dom.0)],
        )
        .await;
        exec(
            &b,
            "INSERT INTO list_members (list_id, external) VALUES (?1, ?2) RETURNING list_id",
            &[Bind::I(list), Bind::S("z@out.test")],
        )
        .await;
        exec(
            &b,
            "INSERT INTO list_members (list_id, account_id) VALUES (?1, ?2) RETURNING list_id",
            &[Bind::I(list), Bind::I(acc.0)],
        )
        .await;
        assert_eq!(
            b.dir.resolve("team", "res.test").await.unwrap(),
            Resolution::List(vec![
                ListMember::External("z@out.test".into()),
                ListMember::Account(acc)
            ]),
            "{n}: insertion order via seq"
        );
        for (l, d) in [("nobody", "res.test"), ("alice", "other.test")] {
            assert!(
                matches!(b.dir.resolve(l, d).await, Err(Error::NotFound(_))),
                "{n}"
            );
        }
    })
    .await;
}

// ------------------------------------------------------------------ mailbox

#[tokio::test]
async fn append_assigns_increasing_ids_uids_and_reads_back() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "app").await;
        let mut refs = vec![];
        for i in 0..5 {
            refs.push(
                mb.append(INBOX, raw(&format!("s{i}"), Some(&format!("m{i}@x"))), Flags::SEEN)
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(
            refs.iter().map(|r| (r.id.0, r.uid)).collect::<Vec<_>>(),
            (1..=5).map(|i| (i as i64, i as u32)).collect::<Vec<_>>(),
            "{n}"
        );
        // Another folder has its own uid space but shares the message-id space.
        let sent = mb.append(FolderId(2), raw("sent", None), Flags::default()).await.unwrap();
        assert_eq!((sent.id.0, sent.uid), (6, 1), "{n}");

        let all = mb.list_folder(INBOX, 0, 100).await.unwrap();
        assert_eq!(all.len(), 5, "{n}");
        assert_eq!(all[0].subject, "s0", "{n}");
        assert_eq!(all[0].from_addr, "bob@example.org", "{n}");
        assert_eq!(all[0].flags, Flags::SEEN, "{n}");
        assert_eq!(all[0].thread_id.0, 1, "{n}");
        assert!(all[0].size > 0 && all[0].internal_date > 1_700_000_000, "{n}");
        // Paging by uid.
        let p1 = mb.list_folder(INBOX, 0, 2).await.unwrap();
        let p2 = mb.list_folder(INBOX, p1[1].uid, 2).await.unwrap();
        let p3 = mb.list_folder(INBOX, p2[1].uid, 2).await.unwrap();
        let p4 = mb.list_folder(INBOX, p3[0].uid, 2).await.unwrap();
        assert_eq!(
            [p1.len(), p2.len(), p3.len(), p4.len()],
            [2, 2, 1, 0],
            "{n}"
        );
        assert_eq!(p3[0].uid, 5, "{n}");
        assert!(matches!(mb.list_folder(INBOX, 0, 0).await, Err(Error::Invalid(_))));
        // Bad input.
        assert!(matches!(
            mb.append(INBOX, Bytes::new(), Flags::default()).await,
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            mb.append(FolderId(77), raw("x", None), Flags::default()).await,
            Err(Error::NotFound(_))
        ));
    })
    .await;
}

#[tokio::test]
async fn changes_since_is_contiguous_and_paged() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "chg").await;
        let f = mb.create_folder(None, "F").await.unwrap();
        mb.append(INBOX, raw("a", None), Flags::default()).await.unwrap();
        mb.append_many(INBOX, vec![raw("b", None), raw("c", None)], Flags::default())
            .await
            .unwrap();
        let page = mb.changes_since(0, 100).await.unwrap();
        assert!(!page.more);
        let seqs: Vec<i64> = page.changes.iter().map(|c| c.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4], "{n}");
        assert_eq!(page.changes[0].item, ChangeItem::Folder(f), "{n}");
        assert_eq!(page.changes[0].op, ChangeOp::Create, "{n}");
        assert_eq!(page.changes[3].item, ChangeItem::Message(rusty_core::MessageId(3)), "{n}");
        let p = mb.changes_since(1, 2).await.unwrap();
        assert!(p.more, "{n}");
        assert_eq!(p.changes.iter().map(|c| c.seq).collect::<Vec<_>>(), vec![2, 3], "{n}");
        let p = mb.changes_since(3, 2).await.unwrap();
        assert!(!p.more && p.changes.len() == 1, "{n}");
        assert!(mb.changes_since(4, 2).await.unwrap().changes.is_empty());
    })
    .await;
}

#[tokio::test]
async fn ids_of_another_mailbox_are_not_found() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, a) = acct(&b, "xa").await;
        let (_, _, other) = acct(&b, "xb").await;
        let secret = a.create_folder(None, "Secret").await.unwrap();
        a.append(secret, raw("private", None), Flags::default()).await.unwrap();
        assert!(matches!(
            other.list_folder(secret, 0, 10).await,
            Err(Error::NotFound(_))
        ), "{n}");
        assert!(matches!(
            other.append(secret, raw("x", None), Flags::default()).await,
            Err(Error::NotFound(_))
        ), "{n}");
        assert!(matches!(
            other.create_folder(Some(secret), "child").await,
            Err(Error::NotFound(_))
        ), "{n}");
        assert!(other.changes_since(0, 10).await.unwrap().changes.is_empty(), "{n}");
        assert_eq!(a.changes_since(0, 10).await.unwrap().changes.len(), 2, "{n}");
    })
    .await;
}

#[tokio::test]
async fn deliver_is_idempotent_by_delivery_id() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "dlv").await;
        let did = DeliveryId([7u8; 32]);
        let first = mb.deliver(INBOX, raw("once", Some("o@x")), Flags::default(), did).await.unwrap();
        let DeliverOutcome::Created(r1) = first else { panic!("{n}: {first:?}") };
        // Retry (even with other folder/flags): Duplicate with the ORIGINAL ref, nothing written.
        let again = mb
            .deliver(FolderId(2), raw("once", Some("o@x")), Flags::SEEN, did)
            .await
            .unwrap();
        assert_eq!(again, DeliverOutcome::Duplicate(r1), "{n}");
        assert_eq!(mb.list_folder(INBOX, 0, 10).await.unwrap().len(), 1, "{n}");
        assert!(mb.list_folder(FolderId(2), 0, 10).await.unwrap().is_empty(), "{n}");
        assert_eq!(mb.changes_since(0, 10).await.unwrap().changes.len(), 1, "{n}");
        // Same id, different content: Conflict, still nothing written.
        assert!(is_conflict(
            mb.deliver(INBOX, raw("other", None), Flags::default(), did).await
        ), "{n}");
        assert_eq!(mb.changes_since(0, 10).await.unwrap().changes.len(), 1, "{n}");
        // A new id creates the next message; counters continue without a gap.
        let next = mb
            .deliver(INBOX, raw("two", None), Flags::default(), DeliveryId([8u8; 32]))
            .await
            .unwrap();
        let DeliverOutcome::Created(r2) = next else { panic!("{n}") };
        assert_eq!((r2.id.0, r2.uid), (2, 2), "{n}");
    })
    .await;
}

#[tokio::test]
async fn threading_joins_parent_also_inside_one_batch() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "thr").await;
        mb.append(INBOX, raw("root", Some("root@x")), Flags::default()).await.unwrap();
        mb.append(INBOX, reply("Re: root", Some("r1@x"), Some("root@x"), &["root@x"]), Flags::default())
            .await
            .unwrap();
        // Batch: a new root, then a reply to the previous message of the SAME batch.
        mb.append_many(
            INBOX,
            vec![
                raw("other", Some("o@x")),
                reply("Re: other", Some("o2@x"), Some("o@x"), &["o@x"]),
                raw("lonely", None),
            ],
            Flags::default(),
        )
        .await
        .unwrap();
        let t: Vec<(i64, i64)> = mb
            .list_folder(INBOX, 0, 10)
            .await
            .unwrap()
            .iter()
            .map(|m| (m.id.0, m.thread_id.0))
            .collect();
        assert_eq!(t, vec![(1, 1), (2, 1), (3, 3), (4, 3), (5, 5)], "{n}");
    })
    .await;
}

#[tokio::test]
async fn append_many_is_consecutive_and_validates() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "many").await;
        mb.append(INBOX, raw("pre", None), Flags::default()).await.unwrap();
        let refs = mb
            .append_many(INBOX, (0..10).map(|i| raw(&format!("m{i}"), None)).collect(), Flags::default())
            .await
            .unwrap();
        assert_eq!(
            refs.iter().map(|r| (r.id.0, r.uid)).collect::<Vec<_>>(),
            (2..=11).map(|i| (i as i64, i as u32)).collect::<Vec<_>>(),
            "{n}"
        );
        let seqs: Vec<i64> = mb.changes_since(0, 100).await.unwrap().changes.iter().map(|c| c.seq).collect();
        assert_eq!(seqs, (1..=11).collect::<Vec<i64>>(), "{n}");
        assert!(matches!(
            mb.append_many(INBOX, vec![], Flags::default()).await,
            Err(Error::Invalid(_))
        ));
        // A batch with one bad item writes nothing.
        let bad = mb.append_many(INBOX, vec![raw("ok", None), Bytes::new()], Flags::default()).await;
        assert!(matches!(bad, Err(Error::Invalid(_))), "{n}");
        assert_eq!(mb.changes_since(0, 100).await.unwrap().changes.len(), 11, "{n}");
    })
    .await;
}

// ------------------------------------------------------------------ concurrency

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_appends_are_gapless() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "conc").await;
        let mut tasks = vec![];
        for i in 0..50 {
            let mb = mb.clone();
            tasks.push(tokio::spawn(async move {
                mb.append(INBOX, raw(&format!("c{i}"), Some(&format!("c{i}@x"))), Flags::default())
                    .await
            }));
        }
        let mut refs = vec![];
        for t in tasks {
            refs.push(t.await.unwrap().unwrap());
        }
        let mut uids: Vec<u32> = refs.iter().map(|r| r.uid).collect();
        let mut ids: Vec<i64> = refs.iter().map(|r| r.id.0).collect();
        uids.sort_unstable();
        ids.sort_unstable();
        assert_eq!(uids, (1..=50).collect::<Vec<u32>>(), "{n}");
        assert_eq!(ids, (1..=50).collect::<Vec<i64>>(), "{n}");
        let seqs: Vec<i64> = mb.changes_since(0, 1000).await.unwrap().changes.iter().map(|c| c.seq).collect();
        assert_eq!(seqs, (1..=50).collect::<Vec<i64>>(), "{n}");
        assert_eq!(mb.list_folder(INBOX, 0, 1000).await.unwrap().len(), 50, "{n}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_batches_stay_consecutive_per_batch() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "cb").await;
        let mut tasks = vec![];
        for t in 0..12 {
            let mb = mb.clone();
            tasks.push(tokio::spawn(async move {
                mb.append_many(
                    INBOX,
                    (0..5).map(|i| raw(&format!("b{t}-{i}"), None)).collect(),
                    Flags::default(),
                )
                .await
            }));
        }
        let mut all = vec![];
        for t in tasks {
            let refs = t.await.unwrap().unwrap();
            // Within one batch: consecutive ids and uids (the mailbox lock makes it atomic).
            for w in refs.windows(2) {
                assert_eq!(w[1].id.0, w[0].id.0 + 1, "{n}");
                assert_eq!(w[1].uid, w[0].uid + 1, "{n}");
            }
            all.extend(refs);
        }
        let mut ids: Vec<i64> = all.iter().map(|r| r.id.0).collect();
        ids.sort_unstable();
        assert_eq!(ids, (1..=60).collect::<Vec<i64>>(), "{n}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_delivery_id_creates_one_message() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "idem").await;
        let did = DeliveryId([9u8; 32]);
        let mut tasks = vec![];
        for _ in 0..20 {
            let mb = mb.clone();
            tasks.push(tokio::spawn(async move {
                mb.deliver(INBOX, raw("same", Some("s@x")), Flags::default(), did).await
            }));
        }
        let (mut created, mut dup) = (0, 0);
        let mut refs = std::collections::HashSet::new();
        for t in tasks {
            match t.await.unwrap().unwrap() {
                DeliverOutcome::Created(r) => {
                    created += 1;
                    refs.insert(r);
                }
                DeliverOutcome::Duplicate(r) => {
                    dup += 1;
                    refs.insert(r);
                }
            }
        }
        assert_eq!((created, dup, refs.len()), (1, 19, 1), "{n}");
        assert_eq!(mb.list_folder(INBOX, 0, 100).await.unwrap().len(), 1, "{n}");
        assert_eq!(mb.changes_since(0, 100).await.unwrap().changes.len(), 1, "{n}");
    })
    .await;
}

/// A reader polling `changes_since` while writers run never sees a hole: the mailbox lock is
/// held from the seq allocation to the commit, so seq N+1 is never visible before N.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reader_never_sees_a_seq_hole() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "rd").await;
        let total = 4 * 15;
        let mut writers = vec![];
        for t in 0..4 {
            let mb = mb.clone();
            writers.push(tokio::spawn(async move {
                for i in 0..15 {
                    mb.append(INBOX, raw(&format!("w{t}-{i}"), None), Flags::default())
                        .await
                        .unwrap();
                }
            }));
        }
        let reader = {
            let mb = mb.clone();
            tokio::spawn(async move {
                let mut cursor = 0i64;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                while cursor < total {
                    assert!(std::time::Instant::now() < deadline, "reader timed out at {cursor}");
                    let page = mb.changes_since(cursor, 1000).await.unwrap();
                    for c in &page.changes {
                        assert_eq!(c.seq, cursor + 1, "hole in change log");
                        cursor = c.seq;
                    }
                    tokio::task::yield_now().await;
                }
            })
        };
        for w in writers {
            w.await.unwrap();
        }
        reader.await.unwrap_or_else(|e| panic!("{n}: {e}"));
    })
    .await;
}

// ------------------------------------------------------------------ dialect tripwire

/// The shared statements are written with `$N` placeholders. That is only safe because
/// sqlx-sqlite binds `$N` by NUMBER (plain SQLite would number `$2 - $1` by first appearance
/// and swap the values). If a sqlx upgrade changes this, the shared SQL must go back to `?N`
/// for SQLite (and be rewritten for Postgres).
#[tokio::test]
async fn sqlite_dollar_placeholders_follow_their_number() {
    let store = common::mem().await;
    let v: i64 = sqlx::query_scalar("SELECT $2 - $1")
        .bind(10i64)
        .bind(3i64)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(v, -7, "$1 = 10, $2 = 3, as on Postgres");
    let v: i64 = sqlx::query_scalar("SELECT $1 * 10 + $2 + $1")
        .bind(2i64)
        .bind(5i64)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(v, 27);
}

// ------------------------------------------------------------------ job fence

/// A `running` job leased for an hour at epoch `epoch`.
async fn running_job(b: &Backend, epoch: i64) -> i64 {
    exec(
        b,
        "INSERT INTO jobs (kind, payload, state, attempts, run_after, lease_owner, \
         lease_expires_at, epoch, created_at, updated_at) \
         VALUES ('deliver', '{}', 'running', 1, 0, 'w1', 4102444800, ?1, 0, 0) RETURNING id",
        &[Bind::I(epoch)],
    )
    .await
}

async fn job_state(b: &Backend, id: i64) -> String {
    let sql = "SELECT state FROM jobs WHERE id = ";
    if let Some(s) = &b.sqlite {
        return sqlx::query_scalar(&format!("{sql}{id}")).fetch_one(s.pool()).await.unwrap();
    }
    #[cfg(feature = "postgres")]
    if let Some((p, _)) = &b.pg {
        return sqlx::query_scalar(&format!("{sql}{id}")).fetch_one(p.pool()).await.unwrap();
    }
    unreachable!()
}

/// The `JobQueue::complete_in` pattern without a transaction handle: the repository effect and
/// the job completion commit together; a stale epoch rolls the effect back.
#[tokio::test]
async fn deliver_completes_the_job_atomically_and_a_stale_fence_rolls_back() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "fence").await;
        let job = running_job(&b, 3).await;
        let did = DeliveryId([5u8; 32]);
        let fence = |id, epoch| JobFence { id: rusty_core::JobId(id), epoch };
        // Stale epoch: Conflict, NOTHING written, job still running.
        let e = mb
            .deliver_fenced(INBOX, raw("x", None), Flags::default(), did, fence(job, 2))
            .await;
        assert!(is_conflict(e), "{n}");
        assert!(mb.list_folder(INBOX, 0, 10).await.unwrap().is_empty(), "{n}");
        assert!(mb.changes_since(0, 10).await.unwrap().changes.is_empty(), "{n}");
        assert_eq!(job_state(&b, job).await, "running", "{n}");
        // Right epoch: message and completion together.
        let out = mb
            .deliver_fenced(INBOX, raw("x", None), Flags::default(), did, fence(job, 3))
            .await
            .unwrap();
        assert!(matches!(out, DeliverOutcome::Created(_)), "{n}");
        assert_eq!(job_state(&b, job).await, "done", "{n}");
        // A retried job (new job row, same delivery id): Duplicate AND the job is completed.
        let job2 = running_job(&b, 1).await;
        let out = mb
            .deliver_fenced(INBOX, raw("x", None), Flags::default(), did, fence(job2, 1))
            .await
            .unwrap();
        assert!(matches!(out, DeliverOutcome::Duplicate(_)), "{n}");
        assert_eq!(job_state(&b, job2).await, "done", "{n}");
        assert_eq!(mb.list_folder(INBOX, 0, 10).await.unwrap().len(), 1, "{n}");
    })
    .await;
}
