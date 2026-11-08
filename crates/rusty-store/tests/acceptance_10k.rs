//! Store acceptance (plan PG6): 10,000 messages over 20 mailboxes through `append` and
//! `deliver_many`, then flags, moves, expunge, purge and blob GC, on SQLite AND Postgres.
//! Checks final counts, reference counts, gapless counters and that the blob files equal the rows.
//! Time is bounded per backend (`RUSTY_ACCEPTANCE_MAX_SECS`, default 180). Run with
//! `-- --nocapture` to see the elapsed times.

mod common;

use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use common::backends::{acct, acct_in, backends, scalar, Backend};
use rusty_core::{BlobHash, DeliveryId, Flags, FolderId, MessageId};
use rusty_store::{DeliverItem, Mailbox};

const MAILBOXES: usize = 20;
const PER_MAILBOX: usize = 500;
const SENT: FolderId = FolderId(2);

/// Message `i` of mailbox `m`. Every tenth message is the same content in every mailbox
/// (shared blobs); all others are unique.
fn body(m: usize, i: usize) -> Bytes {
    let (tag, id) = if i % 10 == 0 { (0, i) } else { (m + 1, i) };
    Bytes::from(format!(
        "From: s{}@remote.test\r\nTo: u{m}@example.test\r\nSubject: acceptance {tag}-{id}\r\n\
         Message-ID: <a{tag}-{id}@acc.test>\r\nDate: Tue, 03 Nov 2026 10:00:00 +0000\r\n\r\n\
         body {tag} {id} {}\r\n",
        i % 50,
        "x".repeat(20 + i % 40)
    ))
}

fn delivery_id(m: usize, i: usize) -> DeliveryId {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(m as u64).to_be_bytes());
    b[8..16].copy_from_slice(&(i as u64).to_be_bytes());
    DeliveryId(b)
}

#[allow(dead_code)]
struct Placed {
    id: MessageId,
    i: usize,
}

/// First half by `append`, second half by `deliver_many` in batches of 50.
async fn fill(mb: Mailbox, m: usize) -> Vec<Placed> {
    let mut out = Vec::new();
    for i in 0..PER_MAILBOX / 2 {
        let r = mb.append(FolderId(1), body(m, i), Flags::empty()).await.unwrap();
        out.push(Placed { id: r.id, i });
    }
    for start in (PER_MAILBOX / 2..PER_MAILBOX).step_by(50) {
        let items: Vec<DeliverItem> = (start..start + 50)
            .map(|i| DeliverItem { raw: body(m, i), flags: Flags::empty(), delivery_id: delivery_id(m, i) })
            .collect();
        let outcomes = mb.deliver_many(FolderId(1), items).await.unwrap();
        assert_eq!(outcomes.len(), 50);
        for (k, o) in outcomes.into_iter().enumerate() {
            match o {
                rusty_store::DeliverOutcome::Created(r) => out.push(Placed { id: r.id, i: start + k }),
                other => panic!("unexpected outcome {other:?}"),
            }
        }
    }
    out
}

async fn run(b: Backend) {
    let started = Instant::now();
    let (dom, _a, first) = acct(&b, "acc10k").await;
    let mut boxes = vec![first];
    for m in 1..MAILBOXES {
        boxes.push(acct_in(&b, dom, &format!("u{m}")).await);
    }

    // 1. fill: all mailboxes concurrently
    let mut tasks = vec![];
    for (m, mb) in boxes.iter().cloned().enumerate() {
        tasks.push(tokio::spawn(fill(mb, m)));
    }
    let mut placed: Vec<Vec<Placed>> = vec![];
    for t in tasks {
        placed.push(t.await.unwrap());
    }
    let total = MAILBOXES * PER_MAILBOX;
    assert_eq!(total, 10_000);
    assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, total as i64);
    // gapless ids, uids and change sequence per mailbox
    assert_eq!(scalar(&b, "SELECT count(*) FROM (SELECT 1 FROM messages GROUP BY mailbox_id HAVING max(id) <> count(*)) x").await, 0);
    assert_eq!(scalar(&b, "SELECT count(*) FROM (SELECT 1 FROM messages GROUP BY mailbox_id, folder_id HAVING max(uid) <> count(*)) x").await, 0);
    assert_eq!(scalar(&b, "SELECT count(*) FROM (SELECT 1 FROM change_log GROUP BY mailbox_id HAVING max(seq) <> count(*)) x").await, 0);
    assert_eq!(scalar(&b, "SELECT count(*) FROM mailboxes WHERE next_msg_id <> 501").await, 0);
    assert_eq!(scalar(&b, "SELECT count(*) FROM mailboxes WHERE next_change_seq <> 501").await, 0);
    // references: one per message; one row per distinct content
    let distinct: HashSet<String> = (0..MAILBOXES)
        .flat_map(|m| (0..PER_MAILBOX).map(move |i| BlobHash::of(&body(m, i)).as_str().to_owned()))
        .collect();
    assert_eq!(scalar(&b, "SELECT CAST(COALESCE(SUM(refcount), 0) AS BIGINT) FROM blobs").await, total as i64);
    assert_eq!(scalar(&b, "SELECT count(*) FROM blobs").await, distinct.len() as i64);
    assert_eq!(b.blobs.list().await.unwrap().len(), distinct.len());

    // 2. flags (every 3rd), moves to Sent (every 7th), expunge (every 10th), per mailbox in batches
    let mut expunged = 0usize;
    for (m, mb) in boxes.iter().enumerate() {
        let ids = |f: &dyn Fn(usize) -> bool| -> Vec<MessageId> {
            placed[m].iter().enumerate().filter(|(k, _)| f(*k)).map(|(_, p)| p.id).collect()
        };
        let flagged = ids(&|k| k % 3 == 0);
        let r = mb.set_flags_many(&flagged[..flagged.len().min(1000)], Flags::SEEN).await.unwrap();
        assert_eq!(r.applied as usize, flagged.len());
        let moved = ids(&|k| k % 7 == 0);
        let r = mb.move_many(&moved, SENT).await.unwrap();
        assert_eq!(r.moved.len(), moved.len());
        let gone = ids(&|k| k % 10 == 0);
        let r = mb.expunge_many(&gone).await.unwrap();
        assert_eq!(r.applied as usize, gone.len());
        expunged += gone.len();
    }
    assert_eq!(scalar(&b, "SELECT count(*) FROM messages WHERE deleted_at IS NOT NULL").await, expunged as i64);
    // soft-deleted messages still hold their references
    assert_eq!(scalar(&b, "SELECT CAST(COALESCE(SUM(refcount), 0) AS BIGINT) FROM blobs").await, total as i64);
    assert_eq!(scalar(&b, "SELECT count(*) FROM (SELECT 1 FROM change_log GROUP BY mailbox_id HAVING max(seq) <> count(*)) x").await, 0);
    let mut live_listed = 0usize;
    for mb in &boxes {
        for f in [FolderId(1), SENT] {
            let mut after = 0;
            loop {
                let page = mb.list_folder(f, after, 1000).await.unwrap();
                if page.is_empty() {
                    break;
                }
                after = page.last().unwrap().uid;
                live_listed += page.len();
            }
        }
    }
    assert_eq!(live_listed, total - expunged);

    // 3. purge the soft-deleted rows, then collect blobs nobody references
    // Messages created by deliver_many are held back while their delivery row exists (replay
    // protection), so the first purge takes only the appended half; purging the delivery rows
    // releases the rest.
    let purged = b.gc().purge_deleted(i64::MAX).await.unwrap();
    assert_eq!(purged as usize, expunged / 2, "delivered messages are held back");
    assert_eq!(b.gc().purge_deleted(i64::MAX).await.unwrap(), 0);
    let rows = b.gc().purge_deliveries(i64::MAX).await.unwrap();
    assert_eq!(rows as usize, MAILBOXES * PER_MAILBOX / 2);
    let purged = b.gc().purge_deleted(i64::MAX).await.unwrap();
    assert_eq!(purged as usize, expunged / 2);
    assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, (total - expunged) as i64);
    assert_eq!(scalar(&b, "SELECT CAST(COALESCE(SUM(refcount), 0) AS BIGINT) FROM blobs").await, (total - expunged) as i64);
    let live: HashSet<String> = (0..MAILBOXES)
        .flat_map(|m| (0..PER_MAILBOX).filter(|i| i % 10 != 0).map(move |i| (m, i)))
        .map(|(m, i)| BlobHash::of(&body(m, i)).as_str().to_owned())
        .collect();
    // positions k % 10 == 0 were expunged; positions equal indexes i, so those i are gone. Shared
    // content (i % 10 == 0) therefore has no live reference at all.
    let far = SystemTime::now() + Duration::from_secs(10 * 86_400);
    let report = b.gc().collect_blobs_at(b.blobs.as_ref(), Duration::ZERO, far).await.unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.rows_deleted as usize, distinct.len() - live.len());
    assert_eq!(report.files_deleted as usize, distinct.len() - live.len());
    assert_eq!(scalar(&b, "SELECT count(*) FROM blobs").await, live.len() as i64);
    assert_eq!(scalar(&b, "SELECT count(*) FROM blobs WHERE refcount = 0").await, 0);
    assert_eq!(b.blobs.list().await.unwrap().len(), live.len(), "blob files equal blob rows");
    assert_eq!(scalar(&b, "SELECT count(*) FROM (SELECT 1 FROM change_log GROUP BY mailbox_id HAVING max(seq) <> count(*)) x").await, 0);

    let elapsed = started.elapsed();
    eprintln!("acceptance_10k {}: {elapsed:.2?}", b.name);
    let max: u64 = std::env::var("RUSTY_ACCEPTANCE_MAX_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(180);
    assert!(elapsed < Duration::from_secs(max), "{}: too slow: {elapsed:?} (limit {max}s)", b.name);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_thousand_messages_over_twenty_mailboxes() {
    for b in backends().await {
        eprintln!("-- backend {}", b.name);
        run(b).await;
    }
}
