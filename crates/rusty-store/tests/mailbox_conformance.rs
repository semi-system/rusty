//! PG2 spike conformance: ONE test body per behaviour, run on Postgres
//! (`RUSTY_TEST_PG_URL`; a missing server fails the test, see `pg_admin_url`). The bodies only use the backend-neutral facades, never SQL, except
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
    if let Some((p, _)) = &b.pg {
        return sqlx::query_scalar(sqlx::AssertSqlSafe(format!("{sql}{id}"))).fetch_one(p.pool()).await.unwrap();
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

// ====================================================================== PG4: all operations
//
// ONE body per behaviour, every operation of `MailboxRepo` through the `Mailbox` facade (and, where
// the facade hides a detail such as `committed_seq`, through the repository). Expected values are
// the SQLite behaviour that existed before the repository extraction.

use rusty_core::{BlobHash, MessageId};
use rusty_store::repo::{Committed, DeliverEntry, Delivery, NewMessage};
use rusty_store::{DeliverItem, Mailbox, MessageRef, MAX_BATCH, MAX_SUPPLIED_TIME};

fn did(n: u8) -> DeliveryId {
    DeliveryId([n; 32])
}

fn item(subject: &str, n: u8) -> DeliverItem {
    DeliverItem {
        raw: raw(subject, None),
        flags: Flags::default(),
        delivery_id: did(n),
    }
}

async fn change_count(mb: &Mailbox) -> usize {
    mb.changes_since(0, 1000).await.unwrap().changes.len()
}

async fn last_change(mb: &Mailbox) -> rusty_store::Change {
    *mb.changes_since(0, 1000).await.unwrap().changes.last().unwrap()
}

/// A repository-level message (the blob is stored first, as the facade does).
async fn new_msg(b: &Backend, folder: FolderId, subject: &str) -> NewMessage {
    let bytes = raw(subject, None);
    let hash = b.blobs.put(bytes.clone()).await.unwrap();
    assert_eq!(hash, BlobHash::of(&bytes));
    NewMessage {
        folder,
        hash,
        size: bytes.len() as i64,
        flags: Flags::default(),
        env: rusty_mime::parse_envelope(&bytes).unwrap(),
        internal_date: None,
    }
}

#[tokio::test]
async fn rename_folder_conformance() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (dom, _, mb) = acct(&b, "ren").await;
        let other = acct_in(&b, dom, "bob").await;
        let f = mb.create_folder(None, "Old").await.unwrap();
        mb.create_folder(None, "Taken").await.unwrap();
        let before = change_count(&mb).await;
        mb.rename_folder(f, "New").await.unwrap();
        let folders = mb.list_folders().await.unwrap();
        assert!(folders.iter().any(|x| x.id == f && x.name == "New" && x.role == "custom"), "{n}");
        assert_eq!(change_count(&mb).await, before + 1, "{n}");
        let last = last_change(&mb).await;
        assert_eq!((last.item, last.op), (ChangeItem::Folder(f), ChangeOp::Update), "{n}");
        // Failures write nothing.
        assert!(matches!(mb.rename_folder(FolderId(1), "X").await, Err(Error::Invalid(_))), "{n}");
        assert!(matches!(mb.rename_folder(FolderId(99), "X").await, Err(Error::NotFound(_))), "{n}");
        assert!(is_conflict(mb.rename_folder(f, "Taken").await), "{n}");
        assert!(matches!(mb.rename_folder(f, "a/b").await, Err(Error::Invalid(_))), "{n}");
        assert!(matches!(other.rename_folder(f, "Z").await, Err(Error::NotFound(_))), "{n}");
        assert_eq!(change_count(&mb).await, before + 1, "{n}");
        assert_eq!(change_count(&other).await, 0, "{n}");
    })
    .await;
}

#[tokio::test]
async fn move_folder_conformance() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "mvf").await;
        let a = mb.create_folder(None, "A").await.unwrap();
        let bb = mb.create_folder(Some(a), "B").await.unwrap();
        let c = mb.create_folder(None, "C").await.unwrap();
        let parent_of = |folders: &[rusty_store::FolderInfo], id: FolderId| {
            folders.iter().find(|f| f.id == id).unwrap().parent
        };
        mb.move_folder(bb, Some(c)).await.unwrap();
        assert_eq!(parent_of(&mb.list_folders().await.unwrap(), bb), Some(c), "{n}");
        mb.move_folder(bb, None).await.unwrap();
        assert_eq!(parent_of(&mb.list_folders().await.unwrap(), bb), None, "{n}");
        // Same parent: nothing changes and nothing is logged.
        let before = change_count(&mb).await;
        mb.move_folder(bb, None).await.unwrap();
        assert_eq!(change_count(&mb).await, before, "{n}");
        // Cycles are Conflict.
        mb.move_folder(c, Some(a)).await.unwrap();
        assert!(is_conflict(mb.move_folder(a, Some(c)).await), "{n}: descendant");
        assert!(is_conflict(mb.move_folder(a, Some(a)).await), "{n}: itself");
        // System folders, unknown folders and unknown parents.
        assert!(matches!(mb.move_folder(FolderId(1), None).await, Err(Error::Invalid(_))), "{n}");
        assert!(matches!(mb.move_folder(FolderId(99), None).await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(mb.move_folder(bb, Some(FolderId(99))).await, Err(Error::NotFound(_))), "{n}");
        // A sibling with the same name under the new parent is Conflict.
        mb.create_folder(Some(a), "B").await.unwrap();
        assert!(is_conflict(mb.move_folder(bb, Some(a)).await), "{n}: sibling name");
        // Depth limit: a 63-level chain; a leaf fits under it, a two-level subtree does not.
        let mut deepest = mb.create_folder(None, "d0").await.unwrap();
        for i in 1..63 {
            deepest = mb.create_folder(Some(deepest), &format!("d{i}")).await.unwrap();
        }
        let leaf = mb.create_folder(None, "leaf").await.unwrap();
        let tree = mb.create_folder(None, "tree").await.unwrap();
        mb.create_folder(Some(tree), "tree-child").await.unwrap();
        assert!(matches!(mb.move_folder(tree, Some(deepest)).await, Err(Error::Invalid(_))), "{n}");
        mb.move_folder(leaf, Some(deepest)).await.unwrap();
        assert_eq!(parent_of(&mb.list_folders().await.unwrap(), tree), None, "{n}");
    })
    .await;
}

#[tokio::test]
async fn delete_folder_conformance() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (dom, _, mb) = acct(&b, "delf").await;
        let other = acct_in(&b, dom, "bob").await;
        let f = mb.create_folder(None, "Tmp").await.unwrap();
        let child = mb.create_folder(Some(f), "Child").await.unwrap();
        // Subfolders and live messages block the delete; system folders are Invalid.
        assert!(is_conflict(mb.delete_folder(f).await), "{n}: children");
        let m = mb.append(child, raw("live", None), Flags::default()).await.unwrap();
        assert!(
            matches!(mb.delete_folder(child).await, Err(Error::Conflict(m)) if m == "folder still contains messages"),
            "{n}"
        );
        assert!(matches!(mb.delete_folder(FolderId(1)).await, Err(Error::Invalid(_))), "{n}");
        assert!(matches!(mb.delete_folder(FolderId(99)).await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(other.delete_folder(child).await, Err(Error::NotFound(_))), "{n}");
        // Expunged messages do not block it: they are purged with the folder, the blob's
        // refcount drops to zero and zero_since is set (database clock).
        mb.expunge(m.id).await.unwrap();
        assert_eq!(scalar(&b, "SELECT refcount FROM blobs").await, 1, "{n}");
        let before = change_count(&mb).await;
        mb.delete_folder(child).await.unwrap();
        assert_eq!(scalar(&b, "SELECT refcount FROM blobs").await, 0, "{n}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM blobs WHERE zero_since IS NOT NULL").await, 1, "{n}");
        assert!(matches!(mb.get_message(m.id).await, Err(Error::NotFound(_))), "{n}");
        assert_eq!(change_count(&mb).await, before + 1, "{n}");
        let last = last_change(&mb).await;
        assert_eq!((last.item, last.op), (ChangeItem::Folder(child), ChangeOp::Destroy), "{n}");
        mb.delete_folder(f).await.unwrap();
        // Folder ids are never reused.
        let again = mb.create_folder(None, "Tmp").await.unwrap();
        assert_eq!(again, FolderId(child.0 + 1), "{n}");
        assert!(mb.list_folders().await.unwrap().iter().all(|x| x.id != f && x.id != child), "{n}");
    })
    .await;
}

#[tokio::test]
async fn set_flags_and_expunge_conformance() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (dom, _, mb) = acct(&b, "flg").await;
        let other = acct_in(&b, dom, "bob").await;
        let m = mb.append(INBOX, raw("one", None), Flags::default()).await.unwrap();
        mb.set_flags(m.id, Flags::SEEN).await.unwrap();
        mb.set_flags(m.id, Flags::FLAGGED).await.unwrap();
        let list = mb.list_folder(INBOX, 0, 10).await.unwrap();
        assert_eq!(list[0].flags, Flags::FLAGGED, "flags are REPLACED: {n}");
        let last = last_change(&mb).await;
        assert_eq!((last.item, last.op), (ChangeItem::Message(m.id), ChangeOp::Update), "{n}");
        assert!(matches!(mb.set_flags(MessageId(99), Flags::SEEN).await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(other.set_flags(m.id, Flags::SEEN).await, Err(Error::NotFound(_))), "{n}");
        // Expunge: soft delete, visible only through get_message.
        mb.expunge(m.id).await.unwrap();
        assert!(mb.list_folder(INBOX, 0, 10).await.unwrap().is_empty(), "{n}");
        let d = mb.get_message(m.id).await.unwrap();
        assert!(d.deleted && d.folder == INBOX && d.summary.id == m.id, "{n}");
        let last = last_change(&mb).await;
        assert_eq!(last.op, ChangeOp::Destroy, "{n}");
        assert!(matches!(mb.expunge(m.id).await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(mb.set_flags(m.id, Flags::SEEN).await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(mb.get_raw_ref(m.id).await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(other.expunge(m.id).await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(other.get_message(m.id).await, Err(Error::NotFound(_))), "{n}");
        // The soft-deleted row keeps its blob referenced.
        assert_eq!(scalar(&b, "SELECT refcount FROM blobs").await, 1, "{n}");
    })
    .await;
}

#[tokio::test]
async fn keyword_conformance() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "kw").await;
        let m = mb.append(INBOX, raw("kw", None), Flags::default()).await.unwrap();
        let base = change_count(&mb).await;
        mb.add_keyword(m.id, "Foo").await.unwrap();
        mb.add_keyword(m.id, "FOO").await.unwrap(); // idempotent, case-insensitive
        assert_eq!(mb.keywords(m.id).await.unwrap(), vec!["foo".to_owned()], "{n}");
        assert_eq!(change_count(&mb).await, base + 1, "{n}: the repeat logs nothing");
        for i in 0..31 {
            mb.add_keyword(m.id, &format!("k{i:02}")).await.unwrap();
        }
        assert_eq!(mb.keywords(m.id).await.unwrap().len(), 32, "{n}");
        assert!(matches!(mb.add_keyword(m.id, "one-too-many").await, Err(Error::Quota(_))), "{n}");
        assert_eq!(mb.keywords(m.id).await.unwrap().len(), 32, "{n}");
        assert!(matches!(mb.add_keyword(m.id, "a b").await, Err(Error::Invalid(_))), "{n}");
        let sorted = mb.keywords(m.id).await.unwrap();
        let mut expect = sorted.clone();
        expect.sort();
        assert_eq!(sorted, expect, "{n}: sorted");
        let before = change_count(&mb).await;
        mb.remove_keyword(m.id, "FOO").await.unwrap();
        assert_eq!(change_count(&mb).await, before + 1, "{n}");
        mb.remove_keyword(m.id, "foo").await.unwrap(); // absent: no-op
        assert_eq!(change_count(&mb).await, before + 1, "{n}");
        mb.add_keyword(m.id, "one-too-many").await.unwrap(); // room again
        // Expunged and unknown messages.
        mb.expunge(m.id).await.unwrap();
        assert!(matches!(mb.add_keyword(m.id, "x").await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(mb.remove_keyword(m.id, "x").await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(mb.keywords(m.id).await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(mb.add_keyword(MessageId(99), "x").await, Err(Error::NotFound(_))), "{n}");
    })
    .await;
}

#[tokio::test]
async fn move_message_conformance() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (dom, _, mb) = acct(&b, "mvm").await;
        let other = acct_in(&b, dom, "bob").await;
        let m1 = mb.append(INBOX, raw("a", None), Flags::default()).await.unwrap();
        let m2 = mb.append(INBOX, raw("b", None), Flags::default()).await.unwrap();
        let sent = FolderId(2);
        assert_eq!(mb.move_message(m2.id, sent).await.unwrap(), MessageRef { id: m2.id, uid: 1 }, "{n}");
        assert_eq!(mb.move_message(m1.id, sent).await.unwrap().uid, 2, "{n}");
        // Already there: the current ref, nothing logged.
        let before = change_count(&mb).await;
        assert_eq!(mb.move_message(m1.id, sent).await.unwrap().uid, 2, "{n}");
        assert_eq!(change_count(&mb).await, before, "{n}");
        assert_eq!(mb.list_folder(sent, 0, 10).await.unwrap().len(), 2, "{n}");
        assert!(mb.list_folder(INBOX, 0, 10).await.unwrap().is_empty(), "{n}");
        // Back: a FRESH uid in the inbox's own sequence.
        assert_eq!(mb.move_message(m1.id, INBOX).await.unwrap().uid, 3, "{n}");
        assert!(matches!(mb.move_message(m1.id, FolderId(77)).await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(mb.move_message(MessageId(99), sent).await, Err(Error::NotFound(_))), "{n}");
        mb.expunge(m2.id).await.unwrap();
        assert!(matches!(mb.move_message(m2.id, INBOX).await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(other.move_message(m1.id, INBOX).await, Err(Error::NotFound(_))), "{n}");
    })
    .await;
}

#[tokio::test]
async fn batch_flag_expunge_and_move_conformance() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (dom, _, mb) = acct(&b, "bat").await;
        let other = acct_in(&b, dom, "bob").await;
        let mut ids = vec![];
        for i in 0..5 {
            ids.push(mb.append(INBOX, raw(&format!("m{i}"), None), Flags::default()).await.unwrap().id);
        }
        let base = change_count(&mb).await;
        // set_flags_many: duplicates apply once, unknown ids are reported in input order.
        let out = mb
            .set_flags_many(&[ids[0], ids[1], ids[1], MessageId(99), ids[2], MessageId(98)], Flags::SEEN)
            .await
            .unwrap();
        assert_eq!((out.applied, out.missing), (3, vec![MessageId(99), MessageId(98)]), "{n}");
        assert_eq!(change_count(&mb).await, base + 3, "{n}");
        let list = mb.list_folder(INBOX, 0, 10).await.unwrap();
        assert_eq!(
            list.iter().map(|m| m.flags == Flags::SEEN).collect::<Vec<_>>(),
            [true, true, true, false, false],
            "{n}"
        );
        // Foreign ids are missing, never touched.
        let out = other.set_flags_many(&[ids[0]], Flags::DRAFT).await.unwrap();
        assert_eq!((out.applied, out.missing.len()), (0, 1), "{n}");
        assert_eq!(change_count(&other).await, 0, "{n}");
        // expunge_many.
        let out = mb.expunge_many(&[ids[3], ids[4], ids[3], MessageId(77)]).await.unwrap();
        assert_eq!((out.applied, out.missing), (2, vec![MessageId(77)]), "{n}");
        assert_eq!(mb.list_folder(INBOX, 0, 10).await.unwrap().len(), 3, "{n}");
        let out = mb.expunge_many(&[ids[3]]).await.unwrap();
        assert_eq!((out.applied, out.missing), (0, vec![ids[3]]), "{n}: already expunged is missing");
        // move_many: fresh contiguous uids in input order; already-there keeps its uid.
        let sent = FolderId(2);
        let mv = mb.move_many(&[ids[2], ids[0], ids[1], MessageId(9)], sent).await.unwrap();
        assert_eq!(
            mv.moved.iter().map(|(id, r)| (id.0, r.uid)).collect::<Vec<_>>(),
            vec![(ids[2].0, 1), (ids[0].0, 2), (ids[1].0, 3)],
            "{n}"
        );
        assert_eq!(mv.missing, vec![MessageId(9)], "{n}");
        let before = change_count(&mb).await;
        let mv = mb.move_many(&[ids[0], ids[3]], sent).await.unwrap();
        assert_eq!(mv.moved, vec![(ids[0], MessageRef { id: ids[0], uid: 2 })], "{n}");
        assert_eq!(mv.missing, vec![ids[3]], "{n}");
        assert_eq!(change_count(&mb).await, before, "{n}: nothing moved, nothing logged");
        assert!(
            matches!(mb.move_many(&[MessageId(99)], FolderId(77)).await, Err(Error::NotFound(_))),
            "{n}"
        );
        // Bounds.
        assert!(matches!(mb.set_flags_many(&[], Flags::SEEN).await, Err(Error::Invalid(_))), "{n}");
        let too_many: Vec<MessageId> = (1..=(MAX_BATCH as i64 + 1)).map(MessageId).collect();
        assert!(matches!(mb.expunge_many(&too_many).await, Err(Error::Invalid(_))), "{n}");
        assert!(matches!(mb.move_many(&too_many, sent).await, Err(Error::Invalid(_))), "{n}");
        // Every change is gapless.
        let seqs: Vec<i64> = mb.changes_since(0, 1000).await.unwrap().changes.iter().map(|c| c.seq).collect();
        assert_eq!(seqs, (1..=seqs.len() as i64).collect::<Vec<_>>(), "{n}");
    })
    .await;
}

#[tokio::test]
async fn deliver_many_outcome_vector() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "dm").await;
        // Created / Created / in-batch Duplicate / Created, consecutive ids and uids.
        let out = mb
            .deliver_many(INBOX, vec![item("a", 1), item("b", 2), item("a", 1), item("c", 3)])
            .await
            .unwrap();
        let (
            DeliverOutcome::Created(r1),
            DeliverOutcome::Created(r2),
            DeliverOutcome::Duplicate(d1),
            DeliverOutcome::Created(r3),
        ) = (out[0], out[1], out[2], out[3])
        else {
            panic!("{n}: {out:?}")
        };
        assert_eq!(d1, r1, "{n}: the in-batch repeat is the ORIGINAL ref");
        assert_eq!(
            [(r1.id.0, r1.uid), (r2.id.0, r2.uid), (r3.id.0, r3.uid)],
            [(1, 1), (2, 2), (3, 3)],
            "{n}"
        );
        assert_eq!(change_count(&mb).await, 3, "{n}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM deliveries").await, 3, "{n}");
        // Later batch: existing delivery is Duplicate with its original ref; new ones continue.
        let out = mb.deliver_many(INBOX, vec![item("a", 1), item("d", 4)]).await.unwrap();
        assert_eq!(out[0], DeliverOutcome::Duplicate(r1), "{n}");
        assert!(matches!(out[1], DeliverOutcome::Created(r) if (r.id.0, r.uid) == (4, 4)), "{n}");
        // Same id, different content: Conflict, and NOTHING of the batch is written.
        let before = (change_count(&mb).await, scalar(&b, "SELECT count(*) FROM messages").await);
        let e = mb.deliver_many(INBOX, vec![item("new", 5), item("other-content", 1)]).await;
        assert!(is_conflict(e), "{n}: against a stored delivery");
        let e = mb.deliver_many(INBOX, vec![item("x", 6), item("y", 6)]).await;
        assert!(is_conflict(e), "{n}: inside the batch");
        assert_eq!(
            (change_count(&mb).await, scalar(&b, "SELECT count(*) FROM messages").await),
            before,
            "{n}: all or nothing"
        );
        // The next create continues without a gap.
        let out = mb.deliver_many(INBOX, vec![item("e", 7)]).await.unwrap();
        assert!(matches!(out[0], DeliverOutcome::Created(r) if (r.id.0, r.uid) == (5, 5)), "{n}");
        // Unknown folder.
        assert!(
            matches!(mb.deliver_many(FolderId(77), vec![item("z", 8)]).await, Err(Error::NotFound(_))),
            "{n}"
        );
    })
    .await;
}

#[tokio::test]
async fn deliver_many_bounds_are_enforced_by_the_facade() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "dmb").await;
        assert!(matches!(mb.deliver_many(INBOX, vec![]).await, Err(Error::Invalid(_))), "{n}");
        let over: Vec<DeliverItem> =
            (0..=MAX_BATCH).map(|i| item(&format!("o{i}"), (i % 250) as u8)).collect();
        assert!(matches!(mb.deliver_many(INBOX, over).await, Err(Error::Invalid(_))), "{n}: 1001 items");
        // 64 MiB in total: two 33 MiB messages are over, before anything is written.
        let big = |k: u8| DeliverItem {
            raw: common::sized(33 * 1024 * 1024),
            flags: Flags::default(),
            delivery_id: did(k),
        };
        assert!(
            matches!(mb.deliver_many(INBOX, vec![big(1), big(2)]).await, Err(Error::Quota(_))),
            "{n}"
        );
        assert_eq!(change_count(&mb).await, 0, "{n}");
        // Exactly 1,000 distinct items are accepted.
        let exact: Vec<DeliverItem> = (0..MAX_BATCH)
            .map(|i| DeliverItem {
                raw: raw(&format!("e{i}"), None),
                flags: Flags::default(),
                delivery_id: DeliveryId({
                    let mut d = [0u8; 32];
                    d[..8].copy_from_slice(&(i as u64).to_be_bytes());
                    d
                }),
            })
            .collect();
        let out = mb.deliver_many(INBOX, exact).await.unwrap();
        assert_eq!(out.len(), MAX_BATCH, "{n}");
        assert!(out.iter().all(|o| matches!(o, DeliverOutcome::Created(_))), "{n}");
        assert_eq!(change_count(&mb).await, MAX_BATCH, "{n}");
        // Empty single messages.
        let empty = DeliverItem { raw: Bytes::new(), flags: Flags::default(), delivery_id: did(9) };
        assert!(matches!(mb.deliver_many(INBOX, vec![empty]).await, Err(Error::Invalid(_))), "{n}");
    })
    .await;
}

#[tokio::test]
async fn deliver_many_completes_the_job_atomically_and_a_stale_fence_rolls_back() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "dmf").await;
        let job = running_job(&b, 3).await;
        let fence = |id, epoch| JobFence { id: rusty_core::JobId(id), epoch };
        let items = || vec![item("f1", 1), item("f2", 2)];
        let e = mb.deliver_many_fenced(INBOX, items(), fence(job, 2)).await;
        assert!(is_conflict(e), "{n}");
        assert_eq!(change_count(&mb).await, 0, "{n}");
        assert_eq!(job_state(&b, job).await, "running", "{n}");
        let out = mb.deliver_many_fenced(INBOX, items(), fence(job, 3)).await.unwrap();
        assert!(out.iter().all(|o| matches!(o, DeliverOutcome::Created(_))), "{n}");
        assert_eq!(job_state(&b, job).await, "done", "{n}");
        // A retried job: all Duplicate, and the new job is completed too.
        let job2 = running_job(&b, 1).await;
        let out = mb.deliver_many_fenced(INBOX, items(), fence(job2, 1)).await.unwrap();
        assert!(out.iter().all(|o| matches!(o, DeliverOutcome::Duplicate(_))), "{n}");
        assert_eq!(job_state(&b, job2).await, "done", "{n}");
        assert_eq!(change_count(&mb).await, 2, "{n}");
    })
    .await;
}

#[tokio::test]
async fn caller_supplied_dates_are_verbatim_and_bounded() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "dat").await;
        let t = 1_234_567_890;
        mb.deliver_at(INBOX, raw("at", None), Flags::default(), did(1), t).await.unwrap();
        mb.deliver_many_at(INBOX, vec![item("at2", 2), item("at3", 3)], t + 1).await.unwrap();
        let dates: Vec<i64> =
            mb.list_folder(INBOX, 0, 10).await.unwrap().iter().map(|m| m.internal_date).collect();
        assert_eq!(dates, vec![t, t + 1, t + 1], "{n}: stored verbatim as internal_date");
        assert_eq!(
            scalars(&b, "SELECT created_at FROM deliveries ORDER BY message_id").await,
            vec![t, t + 1, t + 1],
            "{n}: and as deliveries.created_at"
        );
        for bad in [-1, MAX_SUPPLIED_TIME + 1, i64::MAX] {
            assert!(
                matches!(mb.deliver_at(INBOX, raw("bad", None), Flags::default(), did(9), bad).await, Err(Error::Invalid(_))),
                "{n}: {bad}"
            );
            assert!(
                matches!(mb.deliver_many_at(INBOX, vec![item("bad", 9)], bad).await, Err(Error::Invalid(_))),
                "{n}: {bad}"
            );
            assert!(
                matches!(mb.expunge_many_at(&[MessageId(1)], bad).await, Err(Error::Invalid(_))),
                "{n}: {bad}"
            );
        }
        mb.deliver_at(INBOX, raw("edge", None), Flags::default(), did(8), MAX_SUPPLIED_TIME)
            .await
            .unwrap();
        // No `at`: the database clock.
        let db_now = scalar(&b, "SELECT rs_now()").await;
        let m = mb.append(INBOX, raw("now", None), Flags::default()).await.unwrap();
        let d = mb.get_message(m.id).await.unwrap().summary.internal_date;
        assert!((d - db_now).abs() <= 2, "{n}: {d} vs {db_now}");
    })
    .await;
}

#[tokio::test]
async fn purge_deleted_conformance() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (dom, _, mb) = acct(&b, "prg").await;
        let other = acct_in(&b, dom, "bob").await;
        // Three messages with the SAME content share one blob (refcount 3).
        let m1 = mb.append(INBOX, raw("same", None), Flags::default()).await.unwrap();
        let m2 = mb.append(INBOX, raw("same", None), Flags::default()).await.unwrap();
        let theirs = other.append(INBOX, raw("same", None), Flags::default()).await.unwrap();
        other.expunge_many_at(&[theirs.id], 100).await.unwrap();
        mb.expunge_many_at(&[m1.id], 100).await.unwrap();
        mb.expunge_many_at(&[m2.id], 200).await.unwrap();
        assert_eq!(scalar(&b, "SELECT refcount FROM blobs").await, 3, "{n}");
        // The cutoff is exclusive: deleted_at < older_than.
        assert_eq!(mb.purge_deleted(100).await.unwrap(), 0, "{n}");
        assert_eq!(mb.purge_deleted(101).await.unwrap(), 1, "{n}");
        assert_eq!(scalar(&b, "SELECT refcount FROM blobs").await, 2, "{n}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM blobs WHERE zero_since IS NOT NULL").await, 0, "{n}");
        assert!(matches!(mb.get_message(m1.id).await, Err(Error::NotFound(_))), "{n}");
        assert!(mb.get_message(m2.id).await.unwrap().deleted, "{n}");
        // Only THIS mailbox: the other one's expunged message is untouched.
        assert!(other.get_message(theirs.id).await.unwrap().deleted, "{n}");
        assert_eq!(mb.purge_deleted(201).await.unwrap(), 1, "{n}");
        assert_eq!(other.purge_deleted(i64::MAX).await.unwrap(), 1, "{n}");
        assert_eq!(scalar(&b, "SELECT refcount FROM blobs").await, 0, "{n}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM blobs WHERE zero_since IS NOT NULL").await, 1, "{n}");
        // A message that still has a delivery row is kept.
        let DeliverOutcome::Created(held) =
            mb.deliver(INBOX, raw("held", None), Flags::default(), did(1)).await.unwrap()
        else {
            panic!()
        };
        mb.expunge_many_at(&[held.id], 100).await.unwrap();
        assert_eq!(mb.purge_deleted(i64::MAX).await.unwrap(), 0, "{n}");
        assert!(mb.get_message(held.id).await.unwrap().deleted, "{n}");
        // The batch limit is honoured (repository level): 3 purgeable, limit 2 -> 2 then 1.
        let mut ids = vec![];
        for i in 0..3 {
            ids.push(mb.append(INBOX, raw(&format!("p{i}"), None), Flags::default()).await.unwrap().id);
        }
        mb.expunge_many_at(&ids, 100).await.unwrap();
        let first: Committed<u64> = b.mrepo.purge_deleted(mb.id(), i64::MAX, 2).await.unwrap();
        assert_eq!(first.value, 2, "{n}");
        assert_eq!(b.mrepo.purge_deleted(mb.id(), i64::MAX, 2).await.unwrap().value, 1, "{n}");
    })
    .await;
}

#[tokio::test]
async fn read_operations_conformance() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "rd").await;
        let f = mb.create_folder(None, "Zed").await.unwrap();
        let folders = mb.list_folders().await.unwrap();
        assert_eq!(
            folders
                .iter()
                .map(|x| (x.id.0, x.name.as_str(), x.role.as_str(), x.parent))
                .collect::<Vec<_>>(),
            vec![
                (1, "Inbox", "inbox", None),
                (2, "Sent", "sent", None),
                (3, "Drafts", "drafts", None),
                (4, "Trash", "trash", None),
                (5, "Spam", "spam", None),
                (f.0, "Zed", "custom", None),
            ],
            "{n}"
        );
        let bytes = raw("hello", Some("h@x"));
        let m = mb.append(f, bytes.clone(), Flags::SEEN).await.unwrap();
        let d = mb.get_message(m.id).await.unwrap();
        assert_eq!(d.folder, f, "{n}");
        assert_eq!(d.blob, BlobHash::of(&bytes), "{n}");
        assert!(!d.deleted && d.summary.subject == "hello" && d.summary.flags == Flags::SEEN, "{n}");
        let r = mb.get_raw_ref(m.id).await.unwrap();
        assert_eq!((r.blob, r.size), (BlobHash::of(&bytes), bytes.len() as i64), "{n}");
        assert!(matches!(mb.get_message(MessageId(99)).await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(mb.get_raw_ref(MessageId(99)).await, Err(Error::NotFound(_))), "{n}");
        assert!(mb.keywords(m.id).await.unwrap().is_empty(), "{n}");
    })
    .await;
}

#[tokio::test]
async fn every_operation_treats_another_mailboxs_ids_as_not_found() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (dom, _, a) = acct(&b, "frn").await;
        let o = acct_in(&b, dom, "bob").await;
        let f = a.create_folder(None, "Mine").await.unwrap();
        // `a` has two messages so that its second id (2) does not exist in `o`, which has one.
        a.append(f, raw("pad", None), Flags::default()).await.unwrap();
        let m = a.append(f, raw("mine", None), Flags::default()).await.unwrap();
        let theirs = o.append(INBOX, raw("theirs", None), Flags::default()).await.unwrap();
        let snapshot = (
            a.list_folders().await.unwrap(),
            a.list_folder(f, 0, 10).await.unwrap(),
            change_count(&a).await,
        );
        let nf = |r: Result<(), Error>| matches!(r, Err(Error::NotFound(_)));
        assert!(nf(b.mrepo.folder_exists(o.id(), f).await), "{n} folder_exists");
        assert!(nf(o.create_folder(Some(f), "c").await.map(|_| ())), "{n} create_folder");
        assert!(nf(o.rename_folder(f, "x").await), "{n} rename");
        assert!(nf(o.move_folder(f, None).await), "{n} move_folder");
        assert!(nf(o.delete_folder(f).await), "{n} delete_folder");
        assert!(nf(o.append(f, raw("x", None), Flags::default()).await.map(|_| ())), "{n} append");
        assert!(
            nf(o.append_many(f, vec![raw("x", None)], Flags::default()).await.map(|_| ())),
            "{n} append_many"
        );
        assert!(nf(o.deliver_many(f, vec![item("x", 1)]).await.map(|_| ())), "{n} deliver_many");
        assert!(
            nf(o.deliver(f, raw("x", None), Flags::default(), did(2)).await.map(|_| ())),
            "{n} deliver"
        );
        assert!(nf(o.set_flags(m.id, Flags::SEEN).await), "{n} set_flags");
        assert!(nf(o.add_keyword(m.id, "k").await), "{n} add_keyword");
        assert!(nf(o.remove_keyword(m.id, "k").await), "{n} remove_keyword");
        assert!(nf(o.move_message(m.id, INBOX).await.map(|_| ())), "{n} move_message src");
        assert!(nf(o.move_message(theirs.id, f).await.map(|_| ())), "{n} move_message dst");
        assert!(nf(o.expunge(m.id).await), "{n} expunge");
        assert!(nf(o.get_message(m.id).await.map(|_| ())), "{n} get_message");
        assert!(nf(o.get_raw_ref(m.id).await.map(|_| ())), "{n} get_raw_ref");
        assert!(nf(o.keywords(m.id).await.map(|_| ())), "{n} keywords");
        assert!(nf(o.list_folder(f, 0, 10).await.map(|_| ())), "{n} list_folder");
        assert!(nf(o.move_many(&[theirs.id], f).await.map(|_| ())), "{n} move_many dst");
        // Batches report foreign ids as missing and never touch them.
        assert_eq!(o.set_flags_many(&[m.id], Flags::SEEN).await.unwrap().applied, 0, "{n}");
        assert_eq!(o.expunge_many(&[m.id]).await.unwrap().applied, 0, "{n}");
        assert_eq!(o.move_many(&[m.id], INBOX).await.unwrap().moved.len(), 0, "{n}");
        o.expunge(theirs.id).await.unwrap();
        assert_eq!(o.purge_deleted(i64::MAX).await.unwrap(), 1, "{n}");
        // Nothing of `a` changed.
        assert_eq!(
            (
                a.list_folders().await.unwrap(),
                a.list_folder(f, 0, 10).await.unwrap(),
                change_count(&a).await
            ),
            snapshot,
            "{n}"
        );
    })
    .await;
}

/// Every write returns the seq of the last change it logged (the post-commit hook point), `None`
/// when it logged nothing.
#[tokio::test]
async fn writes_report_the_committed_seq() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "seq").await;
        let id = mb.id();
        let r = &b.mrepo;
        let last = |mb: &Mailbox| {
            let mb = mb.clone();
            async move { mb.changes_since(0, 1000).await.unwrap().changes.last().map(|c| c.seq) }
        };
        let c = r.create_folder(id, None, "F").await.unwrap();
        assert_eq!(c.committed_seq, last(&mb).await, "{n} create_folder");
        let f = c.value;
        assert_eq!(r.rename_folder(id, f, "G").await.unwrap().committed_seq, last(&mb).await, "{n} rename");
        let msg = new_msg(&b, f, "one").await;
        let a = r.append(id, &msg, None, None).await.unwrap();
        assert_eq!(a.committed_seq, last(&mb).await, "{n} append");
        let DeliverOutcome::Created(m1) = a.value else { panic!() };
        let msgs = vec![new_msg(&b, f, "two").await, new_msg(&b, f, "three").await];
        let am = r.append_many(id, &msgs).await.unwrap();
        assert_eq!(am.committed_seq, last(&mb).await, "{n} append_many");
        assert_eq!(am.committed_seq, Some(5), "{n}: folder, rename, append, two");
        let entries = vec![DeliverEntry { msg: new_msg(&b, f, "four").await, delivery_id: did(1) }];
        let dm = r.deliver_many(id, &entries, None, None).await.unwrap();
        assert_eq!(dm.committed_seq, last(&mb).await, "{n} deliver_many");
        let dup = r.deliver_many(id, &entries, None, None).await.unwrap();
        assert_eq!(dup.committed_seq, None, "{n}: a pure duplicate logs nothing");
        let d = Delivery { id: did(2), created_at: None };
        let m4 = new_msg(&b, f, "five").await;
        let created = r.append(id, &m4, Some(&d), None).await.unwrap();
        assert_eq!(created.committed_seq, last(&mb).await, "{n}");
        assert_eq!(r.append(id, &m4, Some(&d), None).await.unwrap().committed_seq, None, "{n}: duplicate");
        assert_eq!(r.set_flags(id, m1.id, Flags::SEEN).await.unwrap().committed_seq, last(&mb).await, "{n}");
        assert_eq!(
            r.set_flags_many(id, &[m1.id.0, 99], Flags::SEEN).await.unwrap().committed_seq,
            last(&mb).await,
            "{n}"
        );
        assert_eq!(
            r.set_flags_many(id, &[99], Flags::SEEN).await.unwrap().committed_seq,
            None,
            "{n}: none applied"
        );
        assert_eq!(r.add_keyword(id, m1.id, "k").await.unwrap().committed_seq, last(&mb).await, "{n}");
        assert_eq!(r.add_keyword(id, m1.id, "k").await.unwrap().committed_seq, None, "{n}: repeat");
        assert_eq!(r.remove_keyword(id, m1.id, "k").await.unwrap().committed_seq, last(&mb).await, "{n}");
        assert_eq!(r.remove_keyword(id, m1.id, "k").await.unwrap().committed_seq, None, "{n}: absent");
        let mv = r.move_message(id, m1.id, FolderId(2)).await.unwrap();
        assert_eq!(mv.committed_seq, last(&mb).await, "{n} move_message");
        assert_eq!(
            r.move_message(id, m1.id, FolderId(2)).await.unwrap().committed_seq,
            None,
            "{n}: already there"
        );
        let mm = r.move_many(id, &[m1.id.0], FolderId(1)).await.unwrap();
        assert_eq!(mm.committed_seq, last(&mb).await, "{n} move_many");
        assert_eq!(r.move_folder(id, f, None).await.unwrap().committed_seq, None, "{n}: same parent");
        let g = r.create_folder(id, None, "H").await.unwrap().value;
        assert_eq!(
            r.move_folder(id, g, Some(f)).await.unwrap().committed_seq,
            last(&mb).await,
            "{n} move_folder"
        );
        assert_eq!(r.expunge(id, m1.id).await.unwrap().committed_seq, last(&mb).await, "{n} expunge");
        assert_eq!(
            r.expunge_many(id, &[3], Some(5)).await.unwrap().committed_seq,
            last(&mb).await,
            "{n} expunge_many"
        );
        assert_eq!(r.delete_folder(id, g).await.unwrap().committed_seq, last(&mb).await, "{n} delete_folder");
        assert_eq!(
            r.purge_deleted(id, i64::MAX, 10).await.unwrap().committed_seq,
            None,
            "{n}: purge logs nothing"
        );
        let seqs: Vec<i64> = mb.changes_since(0, 1000).await.unwrap().changes.iter().map(|c| c.seq).collect();
        assert_eq!(seqs, (1..=seqs.len() as i64).collect::<Vec<_>>(), "{n}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keyword_limit_holds_under_concurrency() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "kwc").await;
        let m = mb.append(INBOX, raw("kwc", None), Flags::default()).await.unwrap();
        let mut tasks = vec![];
        for i in 0..40 {
            let mb = mb.clone();
            tasks.push(tokio::spawn(async move { mb.add_keyword(m.id, &format!("k{i:02}")).await }));
        }
        let (mut ok, mut quota) = (0, 0);
        for t in tasks {
            match t.await.unwrap() {
                Ok(()) => ok += 1,
                Err(Error::Quota(_)) => quota += 1,
                Err(e) => panic!("{n}: {e:?}"),
            }
        }
        assert_eq!((ok, quota), (32, 8), "{n}: the read-then-write limit is atomic");
        assert_eq!(mb.keywords(m.id).await.unwrap().len(), 32, "{n}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM keywords").await, 32, "{n}");
    })
    .await;
}

/// 50 mixed writers on a pool of 4: ids, per-folder uids, change seqs, folder ids and the
/// UIDVALIDITY sequence stay gapless (counters roll back with their transaction).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_counter_is_gapless_under_mixed_concurrency() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "gap").await;
        let mut tasks = vec![];
        for i in 0..50u8 {
            let mb = mb.clone();
            tasks.push(tokio::spawn(async move {
                match i % 5 {
                    0 => {
                        mb.create_folder(None, &format!("f{i}")).await.unwrap();
                    }
                    1 => {
                        mb.append(INBOX, raw(&format!("s{i}"), None), Flags::default()).await.unwrap();
                    }
                    2 => {
                        let refs = mb
                            .append_many(
                                INBOX,
                                (0..3).map(|k| raw(&format!("m{i}-{k}"), None)).collect(),
                                Flags::default(),
                            )
                            .await
                            .unwrap();
                        for w in refs.windows(2) {
                            assert_eq!((w[1].id.0, w[1].uid), (w[0].id.0 + 1, w[0].uid + 1));
                        }
                    }
                    3 => {
                        let out = mb
                            .deliver_many(
                                INBOX,
                                vec![item(&format!("d{i}a"), i), item(&format!("d{i}b"), i + 100)],
                            )
                            .await
                            .unwrap();
                        assert_eq!(out.len(), 2);
                    }
                    _ => {
                        mb.append(FolderId(2), raw(&format!("t{i}"), None), Flags::default())
                            .await
                            .unwrap();
                    }
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let msgs = 10 + 30 + 20 + 10;
        assert_eq!(scalars(&b, "SELECT id FROM messages ORDER BY id").await, (1..=msgs).collect::<Vec<i64>>(), "{n} ids");
        assert_eq!(
            scalars(&b, "SELECT uid FROM messages WHERE folder_id = 1 ORDER BY uid").await,
            (1..=60).collect::<Vec<i64>>(),
            "{n} inbox uids"
        );
        assert_eq!(
            scalars(&b, "SELECT uid FROM messages WHERE folder_id = 2 ORDER BY uid").await,
            (1..=10).collect::<Vec<i64>>(),
            "{n} sent uids"
        );
        assert_eq!(
            scalars(&b, "SELECT seq FROM change_log ORDER BY seq").await,
            (1..=msgs + 10).collect::<Vec<i64>>(),
            "{n} change seqs"
        );
        assert_eq!(
            scalars(&b, "SELECT id FROM folders WHERE id > 5 ORDER BY id").await,
            (6..=15).collect::<Vec<i64>>(),
            "{n} folder ids"
        );
        let uv = scalars(&b, "SELECT uid_validity FROM folders WHERE id > 5 ORDER BY id").await;
        assert!(uv.windows(2).all(|w| w[1] > w[0]), "{n}: uid_validity strictly increases: {uv:?}");
        assert_eq!(scalar(&b, "SELECT next_folder_id FROM mailboxes").await, 16, "{n}");
        assert_eq!(scalar(&b, "SELECT next_msg_id FROM mailboxes").await, msgs + 1, "{n}");
        assert_eq!(scalar(&b, "SELECT next_change_seq FROM mailboxes").await, msgs + 11, "{n}");
    })
    .await;
}

/// A reader polling `changes_since` while writers run `deliver_many` never sees a hole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reader_never_sees_a_seq_hole_during_deliver_many() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "rdb").await;
        let per_writer = 6usize;
        let total = 4 * per_writer * 5;
        let mut writers = vec![];
        for t in 0..4u8 {
            let mb = mb.clone();
            writers.push(tokio::spawn(async move {
                for i in 0..per_writer {
                    let items = (0..5u8)
                        .map(|k| item(&format!("w{t}-{i}-{k}"), t * 50 + (i as u8) * 5 + k))
                        .collect();
                    mb.deliver_many(INBOX, items).await.unwrap();
                }
            }));
        }
        let reader = {
            let mb = mb.clone();
            tokio::spawn(async move {
                let mut cursor = 0i64;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                while cursor < total as i64 {
                    assert!(std::time::Instant::now() < deadline, "reader timed out at {cursor}");
                    for c in mb.changes_since(cursor, 1000).await.unwrap().changes {
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

// ====================================================================== PG4 review: tenant scoping

/// Review I1: the delivery lookup (single `append`/`deliver` and `deliver_many`) is scoped to the
/// mailbox. The same DeliveryId in two mailboxes is two independent deliveries.
#[tokio::test]
async fn different_mailboxes_same_id_are_independent() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, a) = acct(&b, "dla").await;
        let (_, _, other) = acct(&b, "dlb").await;
        let (_, _, third) = acct(&b, "dlc").await;
        // Single deliver.
        let m = raw("one", None);
        let ra = a.deliver(INBOX, m.clone(), Flags::default(), did(1)).await.unwrap();
        let rb = other.deliver(INBOX, m.clone(), Flags::default(), did(1)).await.unwrap();
        assert!(matches!(ra, DeliverOutcome::Created(_)), "{n}: {ra:?}");
        assert!(matches!(rb, DeliverOutcome::Created(_)), "{n}: {rb:?}");
        // Same id with DIFFERENT content in a third mailbox is no conflict either.
        let rc = third
            .deliver(INBOX, raw("different", None), Flags::default(), did(1))
            .await
            .unwrap();
        assert!(matches!(rc, DeliverOutcome::Created(_)), "{n}: {rc:?}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM deliveries").await, 3, "{n}");
        // Inside each mailbox the retry is still a Duplicate of its OWN message.
        let DeliverOutcome::Created(own) = rb else { unreachable!() };
        assert_eq!(
            other.deliver(INBOX, m.clone(), Flags::default(), did(1)).await.unwrap(),
            DeliverOutcome::Duplicate(own),
            "{n}"
        );

        // Batch deliver: ids 10 and 11 in both mailboxes.
        let items = || vec![item("b1", 10), item("b2", 11)];
        let oa = a.deliver_many(INBOX, items()).await.unwrap();
        let ob = other.deliver_many(INBOX, items()).await.unwrap();
        assert!(oa.iter().all(|o| matches!(o, DeliverOutcome::Created(_))), "{n}: {oa:?}");
        assert!(ob.iter().all(|o| matches!(o, DeliverOutcome::Created(_))), "{n}: {ob:?}");
        let again = other.deliver_many(INBOX, items()).await.unwrap();
        let expected: Vec<DeliverOutcome> = ob
            .iter()
            .map(|o| match o {
                DeliverOutcome::Created(r) => DeliverOutcome::Duplicate(*r),
                x => *x,
            })
            .collect();
        assert_eq!(again, expected, "{n}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM deliveries").await, 7, "{n}");
    })
    .await;
}

async fn thread_of(mb: &Mailbox, id: MessageId) -> i64 {
    mb.get_message(id).await.unwrap().summary.thread_id.0
}

/// Review I1: thread lookup never joins a thread of another mailbox, neither through the outer
/// mailbox filter nor through the MIN(id) subquery.
#[tokio::test]
async fn thread_lookup_is_scoped_to_mailbox() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, alice) = acct(&b, "tla").await;
        let (_, _, bob) = acct(&b, "tlb").await;
        // Bob: thread 1 = { p@x (1), b2@x (2, reply to p@x) }.
        bob.append(INBOX, raw("bob root", Some("p@x")), Flags::default()).await.unwrap();
        let b2 = bob
            .append(INBOX, reply("bob reply", Some("b2@x"), Some("p@x"), &[]), Flags::default())
            .await
            .unwrap();
        assert_eq!(thread_of(&bob, b2.id).await, 1, "{n}");

        // Alice: 1 and 2 unrelated, so her replies get ids >= 3.
        alice.append(INBOX, raw("mine 1", Some("m1@x")), Flags::default()).await.unwrap();
        let m2 = alice.append(INBOX, raw("mine 2", Some("m2@x")), Flags::default()).await.unwrap();
        assert_eq!((m2.id.0, thread_of(&alice, m2.id).await), (2, 2), "{n}");
        // Reply to Bob's b2@x, which exists only in Bob's mailbox: own thread (= own id).
        let r = alice
            .append(
                INBOX,
                reply("re", Some("re@x"), Some("b2@x"), &["p@x", "b2@x"]),
                Flags::default(),
            )
            .await
            .unwrap();
        assert_eq!(r.id.0, 3, "{n}");
        assert_eq!(thread_of(&alice, r.id).await, 3, "{n}: joined a foreign thread");
        // Nearest ancestor b2@x is foreign (Bob's id 2, thread 1), the next one m2@x is Alice's
        // own id 2 (thread 2): Alice's thread wins and no foreign row is consulted.
        let r2 = alice
            .append(INBOX, reply("re2", None, None, &["m2@x", "b2@x"]), Flags::default())
            .await
            .unwrap();
        assert_eq!(thread_of(&alice, r2.id).await, 2, "{n}: foreign ancestor chosen");
        // Same through the batch path.
        let rs = alice
            .append_many(
                INBOX,
                vec![reply("re3", None, None, &["m2@x", "b2@x"]), reply("re4", None, Some("b2@x"), &["p@x"])],
                Flags::default(),
            )
            .await
            .unwrap();
        assert_eq!(thread_of(&alice, rs[0].id).await, 2, "{n}: batch, foreign ancestor chosen");
        assert_eq!(thread_of(&alice, rs[1].id).await, rs[1].id.0, "{n}: batch joined a foreign thread");

        // The MIN(id) subquery must not be fed by another mailbox: Dave's shared@x has the
        // smaller id (2) than Carol's (3); a subquery over all mailboxes would pick Dave's row.
        let (_, _, carol) = acct(&b, "tlc").await;
        let (_, _, dave) = acct(&b, "tld").await;
        dave.append(INBOX, raw("d1", Some("d1@x")), Flags::default()).await.unwrap();
        carol.append(INBOX, raw("c1", Some("c1@x")), Flags::default()).await.unwrap();
        carol.append(INBOX, raw("c2", Some("c2@x")), Flags::default()).await.unwrap();
        let anchor = carol
            .append(INBOX, raw("anchor", Some("shared@x")), Flags::default())
            .await
            .unwrap();
        dave.append(INBOX, raw("d-shared", Some("shared@x")), Flags::default()).await.unwrap();
        let rep = carol
            .append(INBOX, reply("rep", None, Some("shared@x"), &[]), Flags::default())
            .await
            .unwrap();
        assert_eq!(
            thread_of(&carol, rep.id).await,
            thread_of(&carol, anchor.id).await,
            "{n}: lookup lost the mailbox's own anchor"
        );
        assert_ne!(thread_of(&carol, rep.id).await, rep.id.0, "{n}");
    })
    .await;
}

/// Review I1 (ported from the SQLite query_shape suite): with duplicate Message-IDs the smallest
/// id is the anchor, and a soft-deleted anchor still counts.
#[tokio::test]
async fn duplicate_message_ids_choose_the_smallest_id_and_deleted_anchor_counts() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "dup").await;
        let a = mb.append(INBOX, raw("a", Some("dup@t")), Flags::default()).await.unwrap();
        let second = mb.append(INBOX, raw("b", Some("dup@t")), Flags::default()).await.unwrap();
        assert_ne!(a.id, second.id, "{n}");
        assert_ne!(thread_of(&mb, a.id).await, thread_of(&mb, second.id).await, "{n}");
        mb.expunge(a.id).await.unwrap(); // a soft-deleted anchor still anchors
        let c = mb
            .append(INBOX, reply("c", None, Some("dup@t"), &[]), Flags::default())
            .await
            .unwrap();
        let ta = thread_of(&mb, a.id).await;
        assert_eq!(thread_of(&mb, c.id).await, ta, "{n}: single append");
        // And through the batch path.
        let d = mb
            .append_many(INBOX, vec![reply("d", None, Some("dup@t"), &[])], Flags::default())
            .await
            .unwrap();
        assert_eq!(thread_of(&mb, d[0].id).await, ta, "{n}: batch");
    })
    .await;
}

// ====================================================================== PG4 review: fence lease, UIDVALIDITY

async fn job_with_lease(b: &Backend, epoch: i64, lease_expires_at: i64) -> i64 {
    exec(
        b,
        "INSERT INTO jobs (kind, payload, state, attempts, run_after, lease_owner, \
         lease_expires_at, epoch, created_at, updated_at) \
         VALUES ('deliver', '{}', 'running', 1, 0, 'w1', ?1, ?2, 0, 0) RETURNING id",
        &[Bind::I(lease_expires_at), Bind::I(epoch)],
    )
    .await
}

/// Review I2: a fence whose lease has expired (database clock) cannot commit, although the epoch
/// still matches and nobody reclaimed the job yet.
#[tokio::test]
async fn an_expired_lease_fails_the_fence_and_rolls_everything_back() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "lease").await;
        let job = job_with_lease(&b, 3, 1).await; // expired in 1970
        let fence = JobFence { id: rusty_core::JobId(job), epoch: 3 };
        let r = mb
            .deliver_fenced(INBOX, raw("x", None), Flags::default(), did(5), fence)
            .await;
        assert!(is_conflict(r), "{n}: deliver_fenced");
        let r = mb.deliver_many_fenced(INBOX, vec![item("y", 6)], fence).await;
        assert!(is_conflict(r), "{n}: deliver_many_fenced");
        assert_eq!(change_count(&mb).await, 0, "{n}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM deliveries").await, 0, "{n}");
        assert_eq!(job_state(&b, job).await, "running", "{n}");
        // The same fence with a live lease commits.
        let live = job_with_lease(&b, 3, 4102444800).await;
        let ok = mb
            .deliver_fenced(
                INBOX,
                raw("x", None),
                Flags::default(),
                did(5),
                JobFence { id: rusty_core::JobId(live), epoch: 3 },
            )
            .await
            .unwrap();
        assert!(matches!(ok, DeliverOutcome::Created(_)), "{n}");
        assert_eq!(job_state(&b, live).await, "done", "{n}");
    })
    .await;
}

async fn set_high_water(b: &Backend, mb: &Mailbox, v: i64) {
    exec(
        b,
        "UPDATE mailboxes SET uid_validity = ?1 WHERE id = ?2 RETURNING id",
        &[Bind::I(v), Bind::I(mb.id().0)],
    )
    .await;
}

async fn high_water(b: &Backend, mb: &Mailbox) -> i64 {
    scalar(b, &format!("SELECT uid_validity FROM mailboxes WHERE id = {}", mb.id().0)).await
}

async fn folder_uidv(b: &Backend, mb: &Mailbox, f: FolderId) -> i64 {
    scalar(
        b,
        &format!(
            "SELECT uid_validity FROM folders WHERE mailbox_id = {} AND id = {}",
            mb.id().0,
            f.0
        ),
    )
    .await
}

/// Review I2: the last representable UIDVALIDITY is issued; the next folder is `Quota` and
/// leaves no trace.
#[tokio::test]
async fn uid_validity_over_u32_max_is_quota() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "uvq").await;
        let max = i64::from(u32::MAX);
        set_high_water(&b, &mb, max - 1).await;
        let f = mb.create_folder(None, "last").await.unwrap();
        assert_eq!(folder_uidv(&b, &mb, f).await, max, "{n}");
        assert_eq!(high_water(&b, &mb).await, max, "{n}");
        let folders = scalar(&b, "SELECT count(*) FROM folders").await;
        let changes = change_count(&mb).await;
        let r = mb.create_folder(None, "one-too-many").await;
        assert!(matches!(r, Err(Error::Quota(_))), "{n}: {r:?}");
        assert_eq!(scalar(&b, "SELECT count(*) FROM folders").await, folders, "{n}");
        assert_eq!(high_water(&b, &mb).await, max, "{n}: high-water mark unchanged");
        assert_eq!(change_count(&mb).await, changes, "{n}");
    })
    .await;
}

/// Review I2: the folder UIDVALIDITY never goes backwards, across delete and re-create and when
/// the clock is behind the high-water mark.
#[tokio::test]
async fn uid_validity_high_water_survives_delete_and_recreate() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "uvh").await;
        let far = 4_000_000_000i64; // ahead of any clock, below u32::MAX
        set_high_water(&b, &mb, far).await;
        let f1 = mb.create_folder(None, "Projects").await.unwrap();
        let v1 = folder_uidv(&b, &mb, f1).await;
        assert_eq!(v1, far + 1, "{n}");
        mb.delete_folder(f1).await.unwrap();
        assert_eq!(high_water(&b, &mb).await, v1, "{n}: delete keeps the mark");
        let f2 = mb.create_folder(None, "Projects").await.unwrap();
        let v2 = folder_uidv(&b, &mb, f2).await;
        assert_ne!(f1, f2, "{n}: folder ids are never reused");
        assert!(v2 > v1, "{n}: {v2} must be > {v1}");
        assert_eq!(high_water(&b, &mb).await, v2, "{n}");
    })
    .await;
}

// ====================================================================== PG4 review: repository contract edges

/// Review M3: direct repository callers get the same verdicts on both backends. The facade
/// dedups ids and always passes a non-negative purge limit, so only the repository is exposed.
#[tokio::test]
async fn repository_rejects_duplicate_move_ids_and_negative_purge_limits() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "edge").await;
        let id = mb.id();
        let m1 = mb.append(INBOX, raw("m1", None), Flags::default()).await.unwrap();
        let m2 = mb.append(INBOX, raw("m2", None), Flags::default()).await.unwrap();
        let changes = change_count(&mb).await;
        // Duplicate ids: Invalid, nothing moved, no uid allocated, nothing logged.
        let r = b.mrepo.move_many(id, &[m1.id.0, m2.id.0, m1.id.0], FolderId(2)).await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{n}: {r:?}");
        let r = b.mrepo.move_many(id, &[m1.id.0, m1.id.0], FolderId(2)).await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{n}: {r:?}");
        assert_eq!(change_count(&mb).await, changes, "{n}");
        assert_eq!(mb.list_folder(INBOX, 0, 10).await.unwrap().len(), 2, "{n}");
        let next = mb.move_message(m1.id, FolderId(2)).await.unwrap();
        assert_eq!(next.uid, 1, "{n}: no uid was burnt by the rejected batch");
        // The facade still dedups for its callers.
        let moved = mb.move_many(&[m2.id, m2.id], FolderId(2)).await.unwrap();
        assert_eq!(moved.moved.len(), 1, "{n}: {moved:?}");

        // Negative purge limit: Invalid, never "unlimited".
        mb.expunge(m1.id).await.unwrap();
        for limit in [-1, i64::MIN] {
            let r = b.mrepo.purge_deleted(id, i64::MAX, limit).await;
            assert!(matches!(r, Err(Error::Invalid(_))), "{n}: limit {limit}: {r:?}");
        }
        assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, 2, "{n}: nothing purged");
        // Zero and positive limits keep working.
        assert_eq!(b.mrepo.purge_deleted(id, i64::MAX, 0).await.unwrap().value, 0, "{n}");
        assert_eq!(b.mrepo.purge_deleted(id, i64::MAX, 1).await.unwrap().value, 1, "{n}");
    })
    .await;
}
