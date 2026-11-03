//! Batch operations: one write transaction per call, counters advance by exactly the number
//! applied, and the result equals applying the single-item calls in order.

mod common;

use std::collections::HashSet;
use std::time::Instant;

use bytes::Bytes;
use common::*;
use proptest::prelude::*;
use rusty_core::{DeliveryId, Error, Flags, FolderId, MessageId};
use rusty_store::{DeliverItem, DeliverOutcome, Mailbox, MAX_BATCH, MAX_DELIVER_BATCH_BYTES};

fn did(n: u32) -> DeliveryId {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(&n.to_be_bytes());
    DeliveryId(b)
}

fn item(subject: &str, mid: Option<&str>, n: u32) -> DeliverItem {
    DeliverItem {
        raw: raw(subject, mid),
        flags: Flags::empty(),
        delivery_id: did(n),
    }
}

/// `n` distinct messages in `folder` through `deliver_many`; returns their ids.
async fn seed(mb: &Mailbox, folder: FolderId, n: usize, tag: &str) -> Vec<MessageId> {
    let mut ids = vec![];
    for chunk in (0..n).collect::<Vec<_>>().chunks(MAX_BATCH) {
        let items = chunk
            .iter()
            .map(|i| {
                let mid = format!("{tag}{i}@t.test");
                DeliverItem {
                    raw: raw(&format!("{tag} {i}"), Some(&mid)),
                    flags: Flags::empty(),
                    delivery_id: delivery_for(tag, *i),
                }
            })
            .collect();
        for o in mb.deliver_many(folder, items).await.unwrap() {
            let DeliverOutcome::Created(r) = o else {
                panic!("expected Created")
            };
            ids.push(r.id);
        }
    }
    ids
}

fn delivery_for(tag: &str, i: usize) -> DeliveryId {
    DeliveryId::derive(
        &rusty_core::BlobHash::of(tag.as_bytes()),
        &i.to_string(),
        "seed",
    )
}

type MsgRow = (i64, i64, i64, i64, bool);

#[derive(Debug, PartialEq, Eq)]
struct Dump {
    messages: Vec<MsgRow>,
    changes: Vec<(i64, String, i64, String)>,
    counters: (i64, i64),
    next_uids: Vec<(i64, i64)>,
}

async fn dump(e: &Env) -> Dump {
    let p = e.store.pool();
    Dump {
        messages: sqlx::query_as(
            "SELECT id, folder_id, uid, flags, deleted_at IS NOT NULL FROM messages ORDER BY id",
        )
        .fetch_all(p)
        .await
        .unwrap(),
        changes: sqlx::query_as("SELECT seq, kind, item_id, op FROM change_log ORDER BY seq")
            .fetch_all(p)
            .await
            .unwrap(),
        counters: sqlx::query_as(
            "SELECT next_msg_id, next_change_seq FROM mailboxes WHERE id = (SELECT MIN(id) FROM mailboxes)",
        )
        .fetch_one(p)
        .await
        .unwrap(),
        next_uids: sqlx::query_as("SELECT id, next_uid FROM folders ORDER BY id")
            .fetch_all(p)
            .await
            .unwrap(),
    }
}

// ---------- equivalence with the single-item calls ----------

#[derive(Debug, Clone)]
enum BOp {
    Flags(Vec<usize>, u8),
    Move(Vec<usize>, i64),
    Expunge(Vec<usize>),
}

fn bop() -> impl Strategy<Value = BOp> {
    let ids = || proptest::collection::vec(0usize..24, 1..14);
    prop_oneof![
        3 => (ids(), 0u8..32).prop_map(|(i, f)| BOp::Flags(i, f)),
        3 => (ids(), 1i64..=5).prop_map(|(i, f)| BOp::Move(i, f)),
        1 => ids().prop_map(BOp::Expunge),
    ]
}

fn pick(pool: &[MessageId], i: usize) -> MessageId {
    // Indexes past the pool are ids that do not exist.
    pool.get(i).copied().unwrap_or(MessageId(10_000 + i as i64))
}

async fn batch_vs_singles(ops: Vec<BOp>) {
    let a = env().await; // batch
    let b = env().await; // singles
    let ia = seed(&a.mb, INBOX, 16, "m").await;
    let ib = seed(&b.mb, INBOX, 16, "m").await;
    assert_eq!(ia, ib);
    for op in ops {
        match op {
            BOp::Flags(ix, f) => {
                let ids: Vec<MessageId> = ix.iter().map(|&i| pick(&ia, i)).collect();
                let flags = Flags::from_bits(f).unwrap();
                let got = a.mb.set_flags_many(&ids, flags).await.unwrap();
                let (mut applied, mut missing) = (0u32, vec![]);
                for id in dedup(&ids) {
                    match b.mb.set_flags(id, flags).await {
                        Ok(()) => applied += 1,
                        Err(Error::NotFound(_)) => missing.push(id),
                        Err(e) => panic!("{e:?}"),
                    }
                }
                assert_eq!((got.applied, got.missing), (applied, missing));
            }
            BOp::Move(ix, f) => {
                let ids: Vec<MessageId> = ix.iter().map(|&i| pick(&ia, i)).collect();
                let got = a.mb.move_many(&ids, FolderId(f)).await.unwrap();
                let (mut moved, mut missing) = (vec![], vec![]);
                for id in dedup(&ids) {
                    match b.mb.move_message(id, FolderId(f)).await {
                        Ok(r) => moved.push((id, r)),
                        Err(Error::NotFound(_)) => missing.push(id),
                        Err(e) => panic!("{e:?}"),
                    }
                }
                assert_eq!((got.moved, got.missing), (moved, missing));
            }
            BOp::Expunge(ix) => {
                let ids: Vec<MessageId> = ix.iter().map(|&i| pick(&ia, i)).collect();
                let got = a.mb.expunge_many(&ids).await.unwrap();
                let (mut applied, mut missing) = (0u32, vec![]);
                for id in dedup(&ids) {
                    match b.mb.expunge(id).await {
                        Ok(()) => applied += 1,
                        Err(Error::NotFound(_)) => missing.push(id),
                        Err(e) => panic!("{e:?}"),
                    }
                }
                assert_eq!((got.applied, got.missing), (applied, missing));
            }
        }
        assert_eq!(dump(&a).await, dump(&b).await);
    }
}

fn dedup(ids: &[MessageId]) -> Vec<MessageId> {
    let mut seen = HashSet::new();
    ids.iter().copied().filter(|i| seen.insert(i.0)).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]
    #[test]
    fn set_flags_many_matches_loop_of_singles(
        flags in proptest::collection::vec((proptest::collection::vec(0usize..22, 1..16), 0u8..32), 1..5)
    ) {
        let ops = flags.into_iter().map(|(i, f)| BOp::Flags(i, f)).collect();
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
            .block_on(batch_vs_singles(ops));
    }

    #[test]
    fn mixed_batches_match_loops_of_singles(ops in proptest::collection::vec(bop(), 1..8)) {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
            .block_on(batch_vs_singles(ops));
    }
}

// ---------- semantics ----------

#[tokio::test]
async fn move_many_allocates_contiguous_uids_in_input_order() {
    let e = env().await;
    let ids = seed(&e.mb, INBOX, 6, "m").await;
    // A pre-existing message in the destination so the range does not start at 1.
    e.mb.append(SENT, raw("x", None), Flags::empty())
        .await
        .unwrap();
    let order = [ids[4], ids[0], ids[5], ids[2]];
    let out = e.mb.move_many(&order, SENT).await.unwrap();
    assert!(out.missing.is_empty());
    let uids: Vec<u32> = out.moved.iter().map(|(_, r)| r.uid).collect();
    assert_eq!(uids, vec![2, 3, 4, 5]);
    let got: Vec<MessageId> = out.moved.iter().map(|(i, _)| *i).collect();
    assert_eq!(got, order);
    let next: i64 = e.scalar("SELECT next_uid FROM folders WHERE id = 2").await;
    assert_eq!(next, 6);
    // Moving again into the same folder is a no-op that reports the current refs.
    let seq = e.scalar("SELECT next_change_seq FROM mailboxes").await;
    let again = e.mb.move_many(&order, SENT).await.unwrap();
    assert_eq!(again.moved, out.moved);
    assert_eq!(e.scalar("SELECT next_change_seq FROM mailboxes").await, seq);
    // Unknown destination.
    assert!(matches!(
        e.mb.move_many(&order, FolderId(99)).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn expunge_many_logs_one_destroy_per_message() {
    let e = env().await;
    let ids = seed(&e.mb, INBOX, 5, "m").await;
    let before = e.scalar("SELECT count(*) FROM change_log").await;
    let out = e.mb.expunge_many(&ids[1..4]).await.unwrap();
    assert_eq!(out.applied, 3);
    let destroys: Vec<(i64,)> = sqlx::query_as(
        "SELECT item_id FROM change_log WHERE op = 'destroy' AND kind = 'message' ORDER BY seq",
    )
    .fetch_all(e.store.pool())
    .await
    .unwrap();
    assert_eq!(
        destroys.into_iter().map(|d| d.0).collect::<Vec<_>>(),
        vec![ids[1].0, ids[2].0, ids[3].0]
    );
    assert_eq!(
        e.scalar("SELECT count(*) FROM change_log").await,
        before + 3
    );
    // A second pass finds them soft-deleted: missing, nothing logged.
    let again = e.mb.expunge_many(&ids[1..4]).await.unwrap();
    assert_eq!((again.applied, again.missing.len()), (0, 3));
    assert_eq!(
        e.scalar("SELECT count(*) FROM change_log").await,
        before + 3
    );
}

#[tokio::test]
async fn missing_and_foreign_ids_are_reported_and_untouched() {
    let e = env().await;
    let mine = seed(&e.mb, INBOX, 3, "m").await;
    let other = e.other("bob").await;
    let theirs = seed(&other, INBOX, 5, "o").await; // ids 1..=5, overlapping numerically
    e.mb.expunge(mine[2]).await.unwrap(); // soft-deleted
    let probe = [mine[0], theirs[4], mine[2], MessageId(777)];
    let out = e.mb.set_flags_many(&probe, Flags::FLAGGED).await.unwrap();
    assert_eq!(out.applied, 1);
    assert_eq!(out.missing, vec![theirs[4], mine[2], MessageId(777)]);
    let mv = e.mb.move_many(&probe, SENT).await.unwrap();
    assert_eq!(mv.moved.len(), 1);
    assert_eq!(mv.missing, vec![theirs[4], mine[2], MessageId(777)]);
    let ex = e.mb.expunge_many(&probe).await.unwrap();
    assert_eq!((ex.applied, ex.missing.len()), (1, 3));
    // The other mailbox is untouched, including the message sharing a numeric id.
    let rows: Vec<(i64, i64, i64, bool)> = sqlx::query_as(
        "SELECT id, folder_id, flags, deleted_at IS NOT NULL FROM messages \
         WHERE mailbox_id = ? ORDER BY id",
    )
    .bind(other.id().0)
    .fetch_all(e.store.pool())
    .await
    .unwrap();
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|r| r.1 == 1 && r.2 == 0 && !r.3));
    // And the soft-deleted message kept its folder and flags.
    let (f, fl): (i64, i64) =
        sqlx::query_as("SELECT folder_id, flags FROM messages WHERE mailbox_id = ? AND id = ?")
            .bind(e.mb.id().0)
            .bind(mine[2].0)
            .fetch_one(e.store.pool())
            .await
            .unwrap();
    assert_eq!((f, fl), (1, 0));
}

#[tokio::test]
async fn duplicate_ids_apply_once() {
    let e = env().await;
    let ids = seed(&e.mb, INBOX, 2, "m").await;
    let seq = e.scalar("SELECT next_change_seq FROM mailboxes").await;
    let out =
        e.mb.set_flags_many(&[ids[0], ids[1], ids[0], ids[0]], Flags::SEEN)
            .await
            .unwrap();
    assert_eq!(out.applied, 2);
    assert_eq!(
        e.scalar("SELECT next_change_seq FROM mailboxes").await,
        seq + 2
    );
    let mv = e.mb.move_many(&[ids[0], ids[0]], SENT).await.unwrap();
    assert_eq!(mv.moved.len(), 1);
    let ex = e.mb.expunge_many(&[ids[1], ids[1]]).await.unwrap();
    assert_eq!(ex.applied, 1);
}

#[tokio::test]
async fn empty_and_oversized_batches_are_invalid() {
    let e = env().await;
    let ids = seed(&e.mb, INBOX, 1, "m").await;
    let big = vec![ids[0]; MAX_BATCH + 1];
    let exactly = vec![ids[0]; MAX_BATCH];
    assert!(matches!(
        e.mb.set_flags_many(&[], Flags::SEEN).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        e.mb.set_flags_many(&big, Flags::SEEN).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        e.mb.move_many(&[], SENT).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        e.mb.move_many(&big, SENT).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        e.mb.expunge_many(&[]).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        e.mb.expunge_many(&big).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        e.mb.deliver_many(INBOX, vec![]).await,
        Err(Error::Invalid(_))
    ));
    let too_many = (0..MAX_BATCH as u32 + 1)
        .map(|n| item("s", None, n))
        .collect();
    assert!(matches!(
        e.mb.deliver_many(INBOX, too_many).await,
        Err(Error::Invalid(_))
    ));
    // Exactly MAX_BATCH ids (all the same) is accepted.
    assert_eq!(e.mb.expunge_many(&exactly).await.unwrap().applied, 1);
}

async fn install_trigger(e: &Env, abort_at_seq: i64) {
    sqlx::query(&format!(
        "CREATE TRIGGER boom BEFORE INSERT ON change_log WHEN NEW.seq = {abort_at_seq} \
         BEGIN SELECT RAISE(ABORT, 'boom'); END"
    ))
    .execute(e.store.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn batch_is_atomic_on_failure() {
    let e = env().await;
    let ids = seed(&e.mb, INBOX, 600, "m").await;
    let before = dump(&e).await;
    // The change-log row of the 500th message of the batch aborts.
    install_trigger(&e, before.counters.1 + 499).await;
    assert!(e.mb.set_flags_many(&ids, Flags::SEEN).await.is_err());
    assert_eq!(dump(&e).await, before);
    assert!(e.mb.move_many(&ids, SENT).await.is_err());
    assert_eq!(dump(&e).await, before);
    assert!(e.mb.expunge_many(&ids).await.is_err());
    assert_eq!(dump(&e).await, before);
    let items = (0..600u32)
        .map(|n| item(&format!("n{n}"), None, 90_000 + n))
        .collect();
    assert!(e.mb.deliver_many(INBOX, items).await.is_err());
    assert_eq!(dump(&e).await, before);
    assert_eq!(e.scalar("SELECT count(*) FROM deliveries").await, 600);
}

#[tokio::test]
async fn batch_is_one_transaction() {
    let e = env().await;
    let ids = seed(&e.mb, INBOX, 300, "m").await;
    let n = ids.len() as i64;
    let d0 = dump(&e).await;
    e.mb.set_flags_many(&ids, Flags::SEEN).await.unwrap();
    let d1 = dump(&e).await;
    assert_eq!(
        d1.counters.1,
        d0.counters.1 + n,
        "one seq per message, no gaps"
    );
    assert_eq!(
        d1.counters.0, d0.counters.0,
        "flags do not take message ids"
    );
    let seqs: Vec<i64> = d1.changes[d0.changes.len()..].iter().map(|c| c.0).collect();
    assert_eq!(seqs, (d0.counters.1..d0.counters.1 + n).collect::<Vec<_>>());

    e.mb.move_many(&ids, SENT).await.unwrap();
    let d2 = dump(&e).await;
    assert_eq!(d2.counters.1, d1.counters.1 + n);
    let uid = |d: &Dump, f: i64| d.next_uids.iter().find(|u| u.0 == f).unwrap().1;
    assert_eq!(
        uid(&d2, 2),
        uid(&d1, 2) + n,
        "destination next_uid by exactly n"
    );
    assert_eq!(uid(&d2, 1), uid(&d1, 1), "source folder counter untouched");

    let items = (0..200u32)
        .map(|k| item(&format!("d{k}"), None, k))
        .collect();
    e.mb.deliver_many(INBOX, items).await.unwrap();
    let d3 = dump(&e).await;
    assert_eq!(
        d3.counters.0,
        d2.counters.0 + 200,
        "next_msg_id by exactly 200"
    );
    assert_eq!(d3.counters.1, d2.counters.1 + 200);
    assert_eq!(uid(&d3, 1), uid(&d2, 1) + 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_batches_have_no_duplicate_uids_or_seqs() {
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(dir.path()).await;
    let e = env_with(store, dir).await;
    let mut tasks = vec![];
    for t in 0..8u32 {
        let mb = e.mb.clone();
        tasks.push(tokio::spawn(async move {
            let items: Vec<DeliverItem> = (0..100u32)
                .map(|i| {
                    item(
                        &format!("t{t} {i}"),
                        Some(&format!("t{t}-{i}@x")),
                        t * 1000 + i,
                    )
                })
                .collect();
            let out = mb.deliver_many(INBOX, items).await.unwrap();
            let ids: Vec<MessageId> = out
                .iter()
                .map(|o| match o {
                    DeliverOutcome::Created(r) => r.id,
                    DeliverOutcome::Duplicate(_) => panic!("unexpected duplicate"),
                })
                .collect();
            // Then move this task's messages to Sent in one batch.
            mb.move_many(&ids, SENT).await.unwrap();
            mb.set_flags_many(&ids, Flags::SEEN).await.unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let d = dump(&e).await;
    let uids: Vec<i64> = d.messages.iter().map(|m| m.2).collect();
    assert_eq!(
        uids.iter().collect::<HashSet<_>>().len(),
        800,
        "unique uids in Sent"
    );
    assert_eq!(*uids.iter().min().unwrap(), 1);
    assert_eq!(*uids.iter().max().unwrap(), 800, "gapless");
    let seqs: Vec<i64> = d.changes.iter().map(|c| c.0).collect();
    assert_eq!(seqs, (1..=2400).collect::<Vec<i64>>());
    let ids: Vec<i64> = d.messages.iter().map(|m| m.0).collect();
    assert_eq!(ids, (1..=800).collect::<Vec<i64>>());
}

// ---------- deliver_many ----------

#[tokio::test]
async fn deliver_many_mixes_created_and_duplicate_outcomes() {
    let e = env().await;
    let first =
        e.mb.deliver(INBOX, raw("old", Some("old@x")), Flags::empty(), did(1))
            .await
            .unwrap();
    let DeliverOutcome::Created(old) = first else {
        panic!()
    };
    let items = vec![
        item("old", Some("old@x"), 1), // duplicate of the earlier delivery
        item("new a", Some("a@x"), 2), // created
        item("new a", Some("a@x"), 2), // duplicate inside the batch
        item("new b", Some("b@x"), 3), // created
    ];
    let out = e.mb.deliver_many(INBOX, items).await.unwrap();
    assert_eq!(out.len(), 4);
    assert_eq!(out[0], DeliverOutcome::Duplicate(old));
    let DeliverOutcome::Created(a) = out[1] else {
        panic!()
    };
    assert_eq!(out[2], DeliverOutcome::Duplicate(a));
    let DeliverOutcome::Created(b) = out[3] else {
        panic!()
    };
    assert_eq!((a.id.0, a.uid, b.id.0, b.uid), (2, 2, 3, 3));
    assert_eq!(e.scalar("SELECT count(*) FROM messages").await, 3);
    assert_eq!(e.scalar("SELECT count(*) FROM deliveries").await, 3);
    assert_eq!(e.scalar("SELECT count(*) FROM change_log").await, 3);
}

#[tokio::test]
async fn deliver_many_rejects_reused_id_with_different_content_atomically() {
    let e = env().await;
    e.mb.deliver(INBOX, raw("one", None), Flags::empty(), did(1))
        .await
        .unwrap();
    let before = dump(&e).await;
    let items = vec![
        item("fresh", None, 5),
        item("different", None, 1), // id 1 was delivered with other content
        item("fresh 2", None, 6),
    ];
    assert!(matches!(
        e.mb.deliver_many(INBOX, items).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(dump(&e).await, before);
    assert_eq!(e.scalar("SELECT count(*) FROM deliveries").await, 1);
    // Reuse inside one batch is also a conflict.
    let items = vec![item("x", None, 9), item("y", None, 9)];
    assert!(matches!(
        e.mb.deliver_many(INBOX, items).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(dump(&e).await, before);
}

#[tokio::test]
async fn deliver_many_respects_total_size_cap() {
    let e = env().await;
    let big = |n: u32| DeliverItem {
        raw: {
            let mut v = sized(33 * 1024 * 1024).to_vec();
            v[20] = b'a' + (n % 26) as u8;
            Bytes::from(v)
        },
        flags: Flags::empty(),
        delivery_id: did(n),
    };
    const { assert!(2 * 33 * 1024 * 1024 > MAX_DELIVER_BATCH_BYTES) };
    assert!(matches!(
        e.mb.deliver_many(INBOX, vec![big(1), big(2)]).await,
        Err(Error::Quota(_))
    ));
    assert!(e.blob_files().is_empty(), "rejected before any blob write");
    assert_eq!(e.scalar("SELECT count(*) FROM messages").await, 0);
    // The per-message limit still applies.
    let huge = DeliverItem {
        raw: sized(51 * 1024 * 1024),
        flags: Flags::empty(),
        delivery_id: did(3),
    };
    assert!(matches!(
        e.mb.deliver_many(INBOX, vec![huge]).await,
        Err(Error::Quota(_))
    ));
    // An empty message and a missing folder are rejected as for deliver.
    let empty = DeliverItem {
        raw: Bytes::new(),
        flags: Flags::empty(),
        delivery_id: did(4),
    };
    assert!(matches!(
        e.mb.deliver_many(INBOX, vec![empty]).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        e.mb.deliver_many(FolderId(99), vec![item("s", None, 5)])
            .await,
        Err(Error::NotFound(_))
    ));
    // One 33 MiB message is fine.
    assert!(e.mb.deliver_many(INBOX, vec![big(7)]).await.is_ok());
}

#[tokio::test]
async fn deliver_many_threads_like_a_loop_of_deliver() {
    let a = env().await;
    let b = env().await;
    // Anchor in the DB, a reply to it, a reply to an in-batch message that arrives LATER
    // (no match), a reply to an earlier in-batch message, and a self-reference.
    for e in [&a, &b] {
        e.mb.append(INBOX, raw("anchor", Some("anchor@x")), Flags::empty())
            .await
            .unwrap();
    }
    let mk = || -> Vec<DeliverItem> {
        vec![
            DeliverItem {
                raw: reply("r1", Some("r1@x"), Some("anchor@x"), &[]),
                flags: Flags::SEEN,
                delivery_id: did(1),
            },
            DeliverItem {
                raw: reply("r2", Some("r2@x"), None, &["later@x"]),
                flags: Flags::empty(),
                delivery_id: did(2),
            },
            DeliverItem {
                raw: reply("later", Some("later@x"), None, &[]),
                flags: Flags::empty(),
                delivery_id: did(3),
            },
            DeliverItem {
                raw: reply("r3", Some("r3@x"), Some("nope@x"), &["x@x", "r1@x"]),
                flags: Flags::empty(),
                delivery_id: did(4),
            },
            DeliverItem {
                raw: reply("self", Some("self@x"), None, &["self@x"]),
                flags: Flags::empty(),
                delivery_id: did(5),
            },
            DeliverItem {
                raw: reply("dup", Some("later@x"), None, &[]),
                flags: Flags::empty(),
                delivery_id: did(6),
            },
            DeliverItem {
                raw: reply("r4", None, None, &["later@x"]),
                flags: Flags::empty(),
                delivery_id: did(7),
            },
        ]
    };
    let got = a.mb.deliver_many(INBOX, mk()).await.unwrap();
    let mut want: Vec<DeliverOutcome> = vec![];
    for it in mk() {
        want.push(
            b.mb.deliver(INBOX, it.raw, it.flags, it.delivery_id)
                .await
                .unwrap(),
        );
    }
    assert_eq!(got, want);
    async fn threads(e: &Env) -> Vec<(i64, i64, i64)> {
        sqlx::query_as("SELECT id, thread_id, flags FROM messages ORDER BY id")
            .fetch_all(e.store.pool())
            .await
            .unwrap()
    }
    assert_eq!(threads(&a).await, threads(&b).await);
    let d = |x: Dump| (x.changes, x.counters, x.next_uids);
    let (da, db) = (dump(&a).await, dump(&b).await);
    assert_eq!(d(da), d(db));
}

// ---------- performance sanity ----------

#[tokio::test]
async fn batch_beats_loop() {
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(dir.path()).await;
    let e = env_with(store, dir).await;
    let ids = seed(&e.mb, INBOX, 1000, "m").await;

    let t = Instant::now();
    for id in &ids {
        e.mb.set_flags(*id, Flags::SEEN).await.unwrap();
    }
    let singles = t.elapsed();
    let t = Instant::now();
    let out = e.mb.set_flags_many(&ids, Flags::FLAGGED).await.unwrap();
    let batch = t.elapsed();
    assert_eq!(out.applied, 1000);
    eprintln!("batch_beats_loop: 1000 singles {singles:?}, one set_flags_many {batch:?}");
    let per_call = singles / 1000;
    if per_call > std::time::Duration::from_millis(1) {
        let ratio = singles.as_secs_f64() / batch.as_secs_f64().max(1e-9);
        eprintln!("batch_beats_loop: ratio {ratio:.1}x");
        assert!(ratio >= 2.0, "batch only {ratio:.1}x faster");
    } else {
        eprintln!(
            "batch_beats_loop: NOTE singles took {per_call:?} per call (< 1 ms, commits are \
             cheap on this filesystem); the ratio assertion is SKIPPED"
        );
    }
}

// ---------- deliver_many against a loop of deliver (proptest) ----------

#[derive(Debug, Clone)]
struct DItem {
    did: u8,
    own: Option<u8>,
    irt: Option<u8>,
    refs: Vec<u8>,
    variant: bool,
}

fn ditem() -> impl Strategy<Value = DItem> {
    (
        0u8..6,
        proptest::option::of(0u8..8),
        proptest::option::of(0u8..8),
        proptest::collection::vec(0u8..8, 0..4),
        proptest::bool::weighted(0.15),
    )
        .prop_map(|(did, own, irt, refs, variant)| DItem {
            did,
            own,
            irt,
            refs,
            variant,
        })
}

fn mid(n: u8) -> String {
    format!("m{n}@t.test")
}

fn build(it: &DItem) -> DeliverItem {
    let own = it.own.map(mid);
    let irt = it.irt.map(mid);
    let refs: Vec<String> = it.refs.iter().map(|r| mid(*r)).collect();
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    DeliverItem {
        raw: reply(
            &format!("s{}", u8::from(it.variant)),
            own.as_deref(),
            irt.as_deref(),
            &refs,
        ),
        flags: Flags::empty(),
        delivery_id: did(u32::from(it.did)),
    }
}

async fn threads_of(e: &Env) -> Vec<(i64, i64)> {
    sqlx::query_as("SELECT id, thread_id FROM messages ORDER BY id")
        .fetch_all(e.store.pool())
        .await
        .unwrap()
}

async fn deliver_equivalence(deleted: Vec<bool>, batches: Vec<Vec<DItem>>) {
    let a = env().await; // deliver_many
    let b = env().await; // loop of deliver
    for e in [&a, &b] {
        for (i, del) in deleted.iter().enumerate() {
            let r =
                e.mb.append(INBOX, raw("anchor", Some(&mid(i as u8))), Flags::empty())
                    .await
                    .unwrap();
            if *del {
                e.mb.expunge(r.id).await.unwrap();
            }
        }
    }
    let mut known: std::collections::HashMap<u32, Bytes> = Default::default();
    for batch in batches {
        let items: Vec<DeliverItem> = batch.iter().map(build).collect();
        // A reused delivery id with different content makes the whole batch a Conflict.
        let mut seen = known.clone();
        let mut conflict = false;
        for it in &items {
            let k = u32::from_be_bytes(it.delivery_id.0[..4].try_into().unwrap());
            match seen.get(&k) {
                Some(raw) if *raw != it.raw => conflict = true,
                Some(_) => {}
                None => {
                    seen.insert(k, it.raw.clone());
                }
            }
        }
        let got = a.mb.deliver_many(INBOX, items.clone()).await;
        if conflict {
            assert!(matches!(got, Err(Error::Conflict(_))), "{got:?}");
            // The whole batch rolled back; the loop side is not run.
            assert_eq!(dump(&a).await, dump(&b).await);
            assert_eq!(threads_of(&a).await, threads_of(&b).await);
            continue;
        }
        known = seen;
        let mut want = vec![];
        for it in items {
            want.push(
                b.mb.deliver(INBOX, it.raw, it.flags, it.delivery_id)
                    .await
                    .unwrap(),
            );
        }
        assert_eq!(got.unwrap(), want);
        assert_eq!(dump(&a).await, dump(&b).await);
        assert_eq!(threads_of(&a).await, threads_of(&b).await);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]
    #[test]
    fn deliver_many_matches_loop_of_deliver(
        deleted in proptest::collection::vec(proptest::bool::ANY, 0..4),
        batches in proptest::collection::vec(proptest::collection::vec(ditem(), 1..8), 1..4),
    ) {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
            .block_on(deliver_equivalence(deleted, batches));
    }
}
