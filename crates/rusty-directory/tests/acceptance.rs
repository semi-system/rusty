//! Foundation acceptance (spec section 8): 10,000 messages through the whole stack on
//! file-backed SQLite plus the filesystem blob store. Lives here because rusty-store cannot
//! depend on rusty-directory. Run with `-- --nocapture` to see the elapsed time.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use rusty_core::{BlobHash, Flags, FolderId, MessageId};
use rusty_directory::Directory;
use rusty_store::{
    collect_blobs_at, purge_deleted, BlobStore, ChangeKind, ChangeOp, FsBlobStore, Mailbox,
    SqliteStore,
};

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
    let started = Instant::now();
    let tmp = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", tmp.path().join("meta.sqlite").display());
    let store = SqliteStore::open(&url).await.unwrap();
    let blob_root = tmp.path().join("blobs");
    let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::new(blob_root.clone()));

    let dir = Directory::new(store.clone());
    let dom = dir.create_domain("example.com").await.unwrap();
    let acc = dir.create_account("alice", dom, None, None).await.unwrap();
    let mb = Mailbox::open(store.clone(), blobs.clone(), acc)
        .await
        .unwrap();

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
    let mut thread_of: HashMap<i64, i64> = HashMap::new();
    for f in targets {
        for s in mb.list_folder(f).await.unwrap() {
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

    // Flags on half (even positions).
    for m in msgs.iter().step_by(2) {
        mb.set_flags(m.id, Flags::SEEN).await.unwrap();
    }
    // Move 1,000 (i % 10 == 3) to Sent.
    for (i, m) in msgs.iter_mut().enumerate() {
        if i % 10 == 3 {
            mb.move_message(m.id, SENT).await.unwrap();
            m.folder = SENT;
        }
    }
    // Expunge 1,000 (i % 10 == 7).
    for (i, m) in msgs.iter_mut().enumerate() {
        if i % 10 == 7 {
            mb.expunge(m.id).await.unwrap();
            m.expunged = true;
        }
    }
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
        let list = mb.list_folder(f).await.unwrap();
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
        seen += mb
            .list_folder(f)
            .await
            .unwrap()
            .iter()
            .filter(|s| s.flags.contains(Flags::SEEN))
            .count();
    }
    assert_eq!(seen, 5000);

    // Change log: contiguous seq and expected op counts.
    let changes = mb.changes_since(0).await.unwrap();
    for (k, c) in changes.iter().enumerate() {
        assert_eq!(c.seq, k as i64 + 1, "gap in change seq");
    }
    let count = |kind, op| {
        changes
            .iter()
            .filter(|c| c.kind == kind && c.op == op)
            .count()
    };
    assert_eq!(count(ChangeKind::Folder, ChangeOp::Create), 3);
    assert_eq!(count(ChangeKind::Message, ChangeOp::Create), TOTAL);
    assert_eq!(count(ChangeKind::Message, ChangeOp::Update), 5000 + 1000);
    assert_eq!(count(ChangeKind::Message, ChangeOp::Destroy), 1000);
    assert_eq!(changes.len(), 3 + TOTAL + 6000 + 1000);
    let last = changes[changes.len() - 1].seq;
    assert!(mb.changes_since(last).await.unwrap().is_empty());

    // Blob accounting before purge: soft-deleted messages still hold references.
    let pool = store.pool();
    let refsum: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(refcount), 0) FROM blobs")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(refsum, TOTAL as i64);
    let distinct_all: HashSet<&str> = msgs.iter().map(|m| m.hash.as_str()).collect();
    assert_eq!(distinct_all.len(), UNIQUE, "dedupe: duplicates share a blob");
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(rows as usize, UNIQUE);
    assert_eq!(count_blob_files(&blob_root), UNIQUE);

    // Purge soft-deleted messages, then collect unreferenced blobs.
    let purged = purge_deleted(&store, unix_now() + 1000).await.unwrap();
    assert_eq!(purged, 1000);
    let nonpurged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
        .fetch_one(pool)
        .await
        .unwrap();
    let refsum: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(refcount), 0) FROM blobs")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(nonpurged, live as i64);
    assert_eq!(refsum, nonpurged);

    let far = SystemTime::now() + Duration::from_secs(10 * 86_400);
    let report = collect_blobs_at(&store, blobs.as_ref(), Duration::ZERO, far)
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
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(rows as usize, expected_blobs);
    assert_eq!(count_blob_files(&blob_root), expected_blobs);
    let zero: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs WHERE refcount = 0")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(zero, 0);

    // Database integrity.
    let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(fk, 1);
    let ic: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(ic, "ok");
    let fkc = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(pool)
        .await
        .unwrap();
    assert!(fkc.is_empty(), "foreign_key_check returned rows");

    let elapsed = started.elapsed();
    eprintln!("acceptance_10k_messages elapsed: {elapsed:.2?}");
    assert!(elapsed < Duration::from_secs(60), "too slow: {elapsed:?}");
}
