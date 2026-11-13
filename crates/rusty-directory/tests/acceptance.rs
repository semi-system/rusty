//! Foundation acceptance (spec section 8): 10,000 messages through the whole stack on
//! file-backed SQLite and Postgres plus the filesystem blob store. Lives here because rusty-store cannot
//! depend on rusty-directory. Run with `-- --nocapture` to see the elapsed time.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use rusty_core::{BlobHash, Flags, FolderId, MessageId};
use rusty_store::{Change, ChangeItem, ChangeOp, Mailbox, MessageSummary, MAX_BATCH, MAX_PAGE};

mod common;
use common::for_each_backend;

/// Every live message of `f`, paging through `list_folder` (pages of at most `MAX_PAGE`).
async fn list_all(mb: &Mailbox, f: FolderId) -> Vec<MessageSummary> {
    let mut out: Vec<MessageSummary> = Vec::new();
    loop {
        let after = out.last().map_or(0, |s| s.uid);
        let page = mb.list_folder(f, after, MAX_PAGE).await.unwrap();
        assert!(page.len() <= MAX_PAGE as usize);
        if page.is_empty() {
            return out;
        }
        out.extend(page);
    }
}

/// Every change after `seq`, paging through `changes_since`.
async fn changes_all(mb: &Mailbox, mut seq: i64) -> Vec<Change> {
    let mut out = Vec::new();
    loop {
        let page = mb.changes_since(seq, MAX_PAGE).await.unwrap();
        assert!(page.changes.len() <= MAX_PAGE as usize);
        if let Some(last) = page.changes.last() {
            seq = last.seq;
        }
        out.extend(page.changes);
        if !page.more {
            return out;
        }
    }
}

const TOTAL: usize = 10_000;
const DUPLICATES: usize = 500;
const UNIQUE: usize = TOTAL - DUPLICATES;
const INBOX: FolderId = FolderId(1);
const SENT: FolderId = FolderId(2);

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Raw message number `n` (unique per n). Every 10th message replies to the one before it.
fn raw_message(n: usize) -> Vec<u8> {
    let reply = if n % 10 == 5 {
        format!(
            "In-Reply-To: <m{}@acc.test>\r\nReferences: <m{}@acc.test>\r\n",
            n - 1,
            n - 1
        )
    } else {
        String::new()
    };
    let subject = match n % 3 {
        0 => format!("Weekly report {n}"),
        1 => format!("Invoice #{n} for review"),
        _ => format!("Re: meeting notes {n}"),
    };
    let body = match n % 3 {
        0 => format!(
            "Status for item {n}: all green.\r\n{}",
            "lorem ipsum ".repeat(20 + n % 40)
        ),
        1 => format!(
            "Please find invoice {n} attached.\r\n{}",
            "amount due ".repeat(10 + n % 30)
        ),
        _ => format!("Notes {n}:\r\n{}", "- action item\r\n".repeat(3 + n % 10)),
    };
    format!(
        "From: sender{}@remote.test\r\nTo: alice@example.com\r\nSubject: {subject}\r\n\
         Message-ID: <m{n}@acc.test>\r\n{reply}Date: Tue, 03 Nov 2026 10:00:00 +0000\r\n\
         \r\n{body}\r\n",
        n % 50
    )
    .into_bytes()
}

struct Msg {
    id: MessageId,
    folder: FolderId,
    hash: BlobHash,
    expunged: bool,
}

fn count_blob_files(root: &std::path::Path) -> usize {
    fn walk(p: &std::path::Path, n: &mut usize) {
        for e in std::fs::read_dir(p).unwrap() {
            let e = e.unwrap();
            // Skip lock, temporary and quarantine files (dot-prefixed).
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if e.file_type().unwrap().is_dir() {
                walk(&e.path(), n);
            } else {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(root, &mut n);
    n
}

#[tokio::test]
async fn acceptance_10k_messages() {
    for_each_backend(|b| async move {
    let started = Instant::now();
    let blobs = b.blobs().await;
    let blob_root = b.tmp.path().join("blobs");

    let dir = b.dir.clone();
    let dom = dir.create_domain("example.com").await.unwrap();
    let acc = dir.create_account("alice", dom, None, None).await.unwrap();
    let mb = b.mailbox(acc).await;

    // 3-level folder tree.
    let projects = mb.create_folder(None, "Projects").await.unwrap();
    let y2026 = mb.create_folder(Some(projects), "2026").await.unwrap();
    let q4 = mb.create_folder(Some(y2026), "Q4").await.unwrap();
    let targets = [INBOX, projects, y2026, q4];

    // Append: 9,500 distinct messages, then 500 exact duplicates of the first 500.
    let mut msgs: Vec<Msg> = Vec::with_capacity(TOTAL);
    for i in 0..TOTAL {
        let n = if i < UNIQUE { i } else { i - UNIQUE };
        let raw = raw_message(n);
        let hash = BlobHash::of(&raw);
        let folder = targets[i % 4];
        let r = mb
            .append(folder, Bytes::from(raw), Flags::empty())
            .await
            .unwrap();
        msgs.push(Msg {
            id: r.id,
            folder,
            hash,
            expunged: false,
        });
    }

    // Threading: a reply lands in its parent's thread.
    let mut thread_of: HashMap<i64, rusty_core::ThreadId> = HashMap::new();
    for f in targets {
        for s in list_all(&mb, f).await {
            thread_of.insert(s.id.0, s.thread_id);
        }
    }
    let mut replies = 0;
    for n in (5..UNIQUE).step_by(10) {
        assert_eq!(
            thread_of[&msgs[n].id.0],
            thread_of[&msgs[n - 1].id.0],
            "reply {n}"
        );
        replies += 1;
    }
    assert!(replies > 900);
    // Unrelated messages (no shared references) must not be merged into one thread.
    let unrelated: HashSet<rusty_core::ThreadId> = [0usize, 1, 2, 3, 4, 6, 7, 10, 11, 12]
        .iter()
        .map(|&n| thread_of[&msgs[n].id.0])
        .collect();
    assert_eq!(unrelated.len(), 10, "unrelated messages share thread ids");

    // Flags on half (even positions), moves and expunges: batch calls of at most MAX_BATCH ids.
    let phase = Instant::now();
    let even: Vec<MessageId> = msgs.iter().step_by(2).map(|m| m.id).collect();
    for chunk in even.chunks(MAX_BATCH) {
        let out = mb.set_flags_many(chunk, Flags::SEEN).await.unwrap();
        assert_eq!((out.applied as usize, out.missing.len()), (chunk.len(), 0));
    }
    // Move 1,000 (i % 10 == 3) to Sent.
    let to_move: Vec<MessageId> = (0..msgs.len())
        .filter(|i| i % 10 == 3)
        .map(|i| msgs[i].id)
        .collect();
    for chunk in to_move.chunks(MAX_BATCH) {
        let out = mb.move_many(chunk, SENT).await.unwrap();
        assert_eq!((out.moved.len(), out.missing.len()), (chunk.len(), 0));
    }
    for (i, m) in msgs.iter_mut().enumerate() {
        if i % 10 == 3 {
            m.folder = SENT;
        }
    }
    // Expunge 1,000 (i % 10 == 7).
    let to_expunge: Vec<MessageId> = (0..msgs.len())
        .filter(|i| i % 10 == 7)
        .map(|i| msgs[i].id)
        .collect();
    for chunk in to_expunge.chunks(MAX_BATCH) {
        let out = mb.expunge_many(chunk).await.unwrap();
        assert_eq!((out.applied as usize, out.missing.len()), (chunk.len(), 0));
    }
    for (i, m) in msgs.iter_mut().enumerate() {
        if i % 10 == 7 {
            m.expunged = true;
        }
    }
    eprintln!("acceptance_10k_messages flag+move+expunge phases: {:.2?}", phase.elapsed());
    let live = msgs.iter().filter(|m| !m.expunged).count();
    assert_eq!(live, 9000);

    // list_folder: counts per folder, ordered by uid, uids unique.
    let mut listed_total = 0;
    for f in [INBOX, SENT, projects, y2026, q4] {
        let expected: HashSet<MessageId> = msgs
            .iter()
            .filter(|m| m.folder == f && !m.expunged)
            .map(|m| m.id)
            .collect();
        let list = list_all(&mb, f).await;
        let uids: Vec<u32> = list.iter().map(|s| s.uid).collect();
        assert!(
            uids.windows(2).all(|w| w[0] < w[1]),
            "uids ordered and unique in {f:?}"
        );
        let got: HashSet<MessageId> = list.iter().map(|s| s.id).collect();
        assert_eq!(got, expected, "folder {f:?}");
        assert_eq!(list.len(), expected.len());
        listed_total += list.len();
    }
    assert_eq!(listed_total, live);
    // Even positions were flagged; none of them is expunged, so 5,000 live messages are seen.
    let mut seen = 0;
    for f in [INBOX, SENT, projects, y2026, q4] {
        seen += list_all(&mb, f)
            .await
            .iter()
            .filter(|s| s.flags.contains(Flags::SEEN))
            .count();
    }
    assert_eq!(seen, 5000);

    // Change log: contiguous seq and expected op counts.
    let changes = changes_all(&mb, 0).await;
    for (k, c) in changes.iter().enumerate() {
        assert_eq!(c.seq, k as i64 + 1, "gap in change seq");
    }
    let count = |folder: bool, op| {
        changes
            .iter()
            .filter(|c| matches!(c.item, ChangeItem::Folder(_)) == folder && c.op == op)
            .count()
    };
    assert_eq!(count(true, ChangeOp::Create), 3);
    assert_eq!(count(false, ChangeOp::Create), TOTAL);
    assert_eq!(count(false, ChangeOp::Update), 5000 + 1000);
    assert_eq!(count(false, ChangeOp::Destroy), 1000);
    assert_eq!(changes.len(), 3 + TOTAL + 6000 + 1000);
    let last = changes[changes.len() - 1].seq;
    let tail = mb.changes_since(last, MAX_PAGE).await.unwrap();
    assert!(tail.changes.is_empty() && !tail.more);

    // Blob accounting before purge: soft-deleted messages still hold references.
    let refsum: i64 = b.scalar("SELECT CAST(COALESCE(SUM(refcount), 0) AS BIGINT) FROM blobs", &[]).await;
    assert_eq!(refsum, TOTAL as i64);
    let distinct_all: HashSet<&str> = msgs.iter().map(|m| m.hash.as_str()).collect();
    assert_eq!(distinct_all.len(), UNIQUE, "dedupe: duplicates share a blob");
    let rows: i64 = b.scalar("SELECT COUNT(*) FROM blobs", &[]).await;
    assert_eq!(rows as usize, UNIQUE);
    assert_eq!(count_blob_files(&blob_root), UNIQUE);

    // Purge soft-deleted messages, then collect unreferenced blobs.
    let purged = b.gc().purge_deleted(unix_now() + 1000).await.unwrap();
    assert_eq!(purged, 1000);
    let nonpurged: i64 = b.scalar("SELECT COUNT(*) FROM messages", &[]).await;
    let refsum: i64 = b.scalar("SELECT CAST(COALESCE(SUM(refcount), 0) AS BIGINT) FROM blobs", &[]).await;
    assert_eq!(nonpurged, live as i64);
    assert_eq!(refsum, nonpurged);

    let far = SystemTime::now() + Duration::from_secs(10 * 86_400);
    let report = b.gc().collect_blobs_at(blobs.as_ref(), Duration::ZERO, far)
        .await
        .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let live_hashes: HashSet<&str> = msgs
        .iter()
        .filter(|m| !m.expunged)
        .map(|m| m.hash.as_str())
        .collect();
    let expected_blobs = live_hashes.len();
    assert!(expected_blobs < UNIQUE, "some blobs became unreferenced");
    assert_eq!(report.rows_deleted as usize, UNIQUE - expected_blobs);
    assert_eq!(report.files_deleted as usize, UNIQUE - expected_blobs);
    let rows: i64 = b.scalar("SELECT COUNT(*) FROM blobs", &[]).await;
    assert_eq!(rows as usize, expected_blobs);
    assert_eq!(count_blob_files(&blob_root), expected_blobs);
    let zero: i64 = b.scalar("SELECT COUNT(*) FROM blobs WHERE refcount = 0", &[]).await;
    assert_eq!(zero, 0);

    // Database integrity: no message points at a missing blob, no folder at a missing mailbox.
    assert_eq!(
        b.scalar(
            "SELECT count(*) FROM messages m LEFT JOIN blobs bl ON bl.hash = m.blob_hash \
             WHERE bl.hash IS NULL",
            &[]
        )
        .await,
        0
    );
    assert_eq!(
        b.scalar(
            "SELECT count(*) FROM folders f LEFT JOIN mailboxes mb ON mb.id = f.mailbox_id \
             WHERE mb.id IS NULL",
            &[]
        )
        .await,
        0
    );

    let elapsed = started.elapsed();
    eprintln!("acceptance_10k_messages elapsed: {elapsed:.2?}");
    let max_secs: u64 = match std::env::var("RUSTY_ACCEPTANCE_MAX_SECS") {
        // Generous default: the test is a regression guard against super-linear blowups (minutes),
        // not a benchmark; 60 s flaked when several CI lanes shared the machine (150 s needed).
        Err(std::env::VarError::NotPresent) => 150,
        Ok(v) => v.parse().unwrap_or_else(|_| {
            panic!("RUSTY_ACCEPTANCE_MAX_SECS must be a whole number of seconds, got {v:?}")
        }),
        Err(e) => panic!("RUSTY_ACCEPTANCE_MAX_SECS is unusable: {e}"),
    };
    assert!(
        elapsed < Duration::from_secs(max_secs),
        "too slow: {elapsed:?} (limit {max_secs}s, override with RUSTY_ACCEPTANCE_MAX_SECS)"
    );
    })
    .await;
}
