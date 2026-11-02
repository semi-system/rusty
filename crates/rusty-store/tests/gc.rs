mod common;

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use common::*;
use rusty_core::{BlobHash, Flags};
use rusty_store::{
    collect_blobs, collect_blobs_at, purge_deleted, BlobStore, GcReport, PURGE_BATCH,
};

const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(86_400);

fn root(e: &Env) -> PathBuf {
    e.dir.path().join("blobs")
}

fn blob_path(root: &Path, h: &BlobHash) -> PathBuf {
    let s = h.as_str();
    root.join(&s[0..2]).join(&s[2..4]).join(s)
}

fn set_mtime(p: &Path, t: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

async fn exec(e: &Env, sql: &str) {
    sqlx::query(sql).execute(e.store.pool()).await.unwrap();
}

async fn opt(e: &Env, sql: &str) -> Option<i64> {
    sqlx::query_scalar(sql)
        .fetch_optional(e.store.pool())
        .await
        .unwrap()
}

async fn refcount(e: &Env, h: &BlobHash) -> Option<i64> {
    opt(
        e,
        &format!("SELECT refcount FROM blobs WHERE hash='{}'", h.as_str()),
    )
    .await
}

async fn has_row(e: &Env, h: &BlobHash) -> bool {
    refcount(e, h).await.is_some()
}

/// (rows_deleted, files_deleted, tmp_swept)
fn counts(r: &GcReport) -> (u64, u64, u64) {
    (r.rows_deleted, r.files_deleted, r.tmp_swept)
}

/// Append `raw` to alice's inbox, then hard-delete the message so the blob is unreferenced.
async fn unreferenced_blob(e: &Env, raw: Bytes) -> BlobHash {
    let h = BlobHash::of(&raw);
    let m = e.mb.append(INBOX, raw, Flags::default()).await.unwrap();
    exec(
        e,
        &format!(
            "DELETE FROM messages WHERE mailbox_id={} AND id={}",
            e.mb.id().0,
            m.id.0
        ),
    )
    .await;
    assert_eq!(refcount(e, &h).await, Some(0));
    h
}

async fn delete_account(e: &Env, local: &str) {
    exec(
        e,
        &format!("DELETE FROM accounts WHERE local_part='{local}'"),
    )
    .await;
}

// ---------- purge_deleted ----------

#[tokio::test]
async fn purge_removes_only_old_deleted_messages() {
    let e = env().await;
    let mut ids = vec![];
    for s in ["a", "b", "c", "d"] {
        ids.push(
            e.mb.append(INBOX, raw(s, None), Flags::default())
                .await
                .unwrap()
                .id,
        );
    }
    e.mb.add_keyword(ids[0], "work").await.unwrap();
    e.mb.add_keyword(ids[1], "work").await.unwrap();
    for (i, at) in [(0usize, 100i64), (1, 5000), (3, 1000)] {
        e.mb.expunge(ids[i]).await.unwrap();
        exec(
            &e,
            &format!("UPDATE messages SET deleted_at={at} WHERE id={}", ids[i].0),
        )
        .await;
    }
    let changes = e.scalar("SELECT count(*) FROM change_log").await;

    assert_eq!(purge_deleted(&e.store, 1000).await.unwrap(), 1);

    let left: Vec<i64> = sqlx::query_scalar("SELECT id FROM messages ORDER BY id")
        .fetch_all(e.store.pool())
        .await
        .unwrap();
    assert_eq!(left, vec![ids[1].0, ids[2].0, ids[3].0]);
    assert_eq!(
        e.scalar(&format!(
            "SELECT count(*) FROM keywords WHERE message_id={}",
            ids[0].0
        ))
        .await,
        0,
        "keywords cascade"
    );
    assert_eq!(e.scalar("SELECT count(*) FROM keywords").await, 1);
    let ha = BlobHash::of(&raw("a", None));
    assert_eq!(refcount(&e, &ha).await, Some(0));
    assert!(opt(
        &e,
        &format!("SELECT zero_since FROM blobs WHERE hash='{}'", ha.as_str())
    )
    .await
    .is_some());
    assert_eq!(
        e.scalar("SELECT count(*) FROM change_log").await,
        changes,
        "change_log untouched"
    );

    // nothing else is old enough; live messages are never touched
    assert_eq!(purge_deleted(&e.store, 1000).await.unwrap(), 0);
    assert_eq!(purge_deleted(&e.store, i64::MAX).await.unwrap(), 2);
    let left: Vec<i64> = sqlx::query_scalar("SELECT id FROM messages")
        .fetch_all(e.store.pool())
        .await
        .unwrap();
    assert_eq!(left, vec![ids[2].0]);
}

#[tokio::test]
async fn purge_batches_over_1000_rows() {
    assert_eq!(PURGE_BATCH, 1000);
    let e = env().await;
    let live =
        e.mb.append(INBOX, raw("live", None), Flags::default())
            .await
            .unwrap();
    let h = BlobHash::of(b"many");
    exec(
        &e,
        &format!(
            "INSERT INTO blobs (hash, size) VALUES ('{}', 4)",
            h.as_str()
        ),
    )
    .await;
    exec(
        &e,
        &format!(
            "INSERT INTO messages (mailbox_id, id, folder_id, uid, blob_hash, size, internal_date, \
             thread_id, deleted_at) \
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2500) \
             SELECT {mb}, 1000 + i, 1, 1000 + i, '{h}', 4, 0, 1000 + i, 5 FROM n",
            mb = e.mb.id().0,
            h = h.as_str()
        ),
    )
    .await;
    assert_eq!(refcount(&e, &h).await, Some(2500));

    assert_eq!(purge_deleted(&e.store, 6).await.unwrap(), 2500);
    assert_eq!(refcount(&e, &h).await, Some(0));
    let left: Vec<i64> = sqlx::query_scalar("SELECT id FROM messages")
        .fetch_all(e.store.pool())
        .await
        .unwrap();
    assert_eq!(left, vec![live.id.0]);
}

// ---------- collect_blobs ----------

#[tokio::test]
async fn collect_keeps_shared_blob_after_one_account_deleted() {
    let e = env().await;
    let bob = e.other("bob").await;
    let msg = raw("shared", Some("s@x"));
    let h = BlobHash::of(&msg);
    e.mb.append(INBOX, msg.clone(), Flags::default())
        .await
        .unwrap();
    bob.append(INBOX, msg.clone(), Flags::default())
        .await
        .unwrap();
    assert_eq!(refcount(&e, &h).await, Some(2));

    delete_account(&e, "alice").await;
    assert_eq!(refcount(&e, &h).await, Some(1));

    let r = collect_blobs_at(
        &e.store,
        e.blobs.as_ref(),
        Duration::ZERO,
        SystemTime::now() + DAY,
    )
    .await
    .unwrap();
    assert_eq!(counts(&r), (0, 0, 0));
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(refcount(&e, &h).await, Some(1));
    assert_eq!(e.blobs.get(&h).await.unwrap(), msg);
}

#[tokio::test]
async fn delete_account_cascades_mailbox_and_decrements_refcount() {
    let e = env().await;
    let bob = e.other("bob").await;
    let shared = raw("shared", Some("s@x"));
    let own = raw("alice only", None);
    let (hs, ho) = (BlobHash::of(&shared), BlobHash::of(&own));
    let m =
        e.mb.append(INBOX, shared.clone(), Flags::default())
            .await
            .unwrap();
    e.mb.add_keyword(m.id, "work").await.unwrap();
    e.mb.append(SENT, own.clone(), Flags::default())
        .await
        .unwrap();
    bob.append(INBOX, shared.clone(), Flags::default())
        .await
        .unwrap();
    let alice_mb = e.mb.id().0;

    delete_account(&e, "alice").await;
    for t in [
        "mailboxes WHERE id",
        "folders WHERE mailbox_id",
        "messages WHERE mailbox_id",
        "keywords WHERE mailbox_id",
        "change_log WHERE mailbox_id",
    ] {
        assert_eq!(
            e.scalar(&format!("SELECT count(*) FROM {t}={alice_mb}"))
                .await,
            0,
            "{t}"
        );
    }
    assert_eq!(refcount(&e, &hs).await, Some(1));
    assert_eq!(refcount(&e, &ho).await, Some(0));

    // grace 0, evaluated a few seconds later so second-resolution timestamps are in the past
    let later = || SystemTime::now() + Duration::from_secs(5);
    let r = collect_blobs_at(&e.store, e.blobs.as_ref(), Duration::ZERO, later())
        .await
        .unwrap();
    assert_eq!(counts(&r), (1, 1, 0));
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(!has_row(&e, &ho).await);
    assert!(!blob_path(&root(&e), &ho).exists());
    assert_eq!(refcount(&e, &hs).await, Some(1));
    assert_eq!(e.blobs.get(&hs).await.unwrap(), shared);

    delete_account(&e, "bob").await;
    assert_eq!(refcount(&e, &hs).await, Some(0));
    let r = collect_blobs_at(&e.store, e.blobs.as_ref(), Duration::ZERO, later())
        .await
        .unwrap();
    assert_eq!(counts(&r), (1, 1, 0));
    assert!(!has_row(&e, &hs).await);
    assert!(!blob_path(&root(&e), &hs).exists());
    assert!(e.blob_files().is_empty());
}

#[tokio::test]
async fn collect_removes_unreferenced_blob_after_grace() {
    let e = env().await;
    let h = unreferenced_blob(&e, raw("gone", None)).await;
    let r = collect_blobs_at(
        &e.store,
        e.blobs.as_ref(),
        HOUR,
        SystemTime::now() + 2 * HOUR,
    )
    .await
    .unwrap();
    assert_eq!(counts(&r), (1, 1, 0));
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(!has_row(&e, &h).await);
    assert!(!blob_path(&root(&e), &h).exists());
}

#[tokio::test]
async fn collect_keeps_unreferenced_blob_within_grace() {
    let e = env().await;
    let h = unreferenced_blob(&e, raw("young", None)).await;
    for now in [SystemTime::now(), SystemTime::now() + HOUR / 2] {
        let r = collect_blobs_at(&e.store, e.blobs.as_ref(), HOUR, now)
            .await
            .unwrap();
        assert_eq!(counts(&r), (0, 0, 0));
    }
    let r = collect_blobs(&e.store, e.blobs.as_ref(), HOUR)
        .await
        .unwrap();
    assert_eq!(counts(&r), (0, 0, 0));
    assert!(has_row(&e, &h).await);
    assert!(blob_path(&root(&e), &h).is_file());
}

#[tokio::test]
async fn collect_removes_orphan_file_without_row_after_grace() {
    let e = env().await;
    let h = e
        .blobs
        .put(Bytes::from_static(b"orphan: tx failed after put"))
        .await
        .unwrap();
    let p = blob_path(&root(&e), &h);
    set_mtime(&p, SystemTime::now() - 2 * DAY);
    let r = collect_blobs(&e.store, e.blobs.as_ref(), HOUR)
        .await
        .unwrap();
    assert_eq!(counts(&r), (0, 1, 0));
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(!p.exists());
}

#[tokio::test]
async fn collect_keeps_recent_orphan_file() {
    let e = env().await;
    let h = e
        .blobs
        .put(Bytes::from_static(b"orphan in flight"))
        .await
        .unwrap();
    let p = blob_path(&root(&e), &h);
    set_mtime(&p, SystemTime::now() - HOUR / 2);
    let r = collect_blobs(&e.store, e.blobs.as_ref(), HOUR)
        .await
        .unwrap();
    assert_eq!(counts(&r), (0, 0, 0));
    assert!(p.is_file());
}

/// Deterministic replay of the put/GC race: `append` calls `put` (the file already exists, so
/// `put` returns early), then GC runs before `append`'s transaction commits. The refreshed
/// mtime must keep the file alive; the blobs row itself may go (the transaction re-inserts it).
#[tokio::test]
async fn put_refreshes_mtime_protects_from_gc() {
    let e = env().await;
    // 1. a blob whose row is old and unreferenced and whose file is old
    let msg = raw("come back", Some("back@x"));
    let h = unreferenced_blob(&e, msg.clone()).await;
    exec(
        &e,
        &format!(
            "UPDATE blobs SET zero_since = 0 WHERE hash='{}'",
            h.as_str()
        ),
    )
    .await;
    let p = blob_path(&root(&e), &h);
    set_mtime(&p, SystemTime::now() - 3 * DAY);
    // 2. append starts: put() of identical content
    assert_eq!(e.blobs.put(msg.clone()).await.unwrap(), h);
    // 3. GC runs in between
    let r = collect_blobs(&e.store, e.blobs.as_ref(), HOUR)
        .await
        .unwrap();
    assert_eq!(counts(&r), (1, 0, 0), "row may go, file must stay");
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(p.is_file(), "recently put blob survives GC");
    // 4. append's transaction: the message references a readable blob
    e.mb.append(INBOX, msg.clone(), Flags::default())
        .await
        .unwrap();
    assert_eq!(refcount(&e, &h).await, Some(1));
    assert_eq!(e.blobs.get(&h).await.unwrap(), msg);

    // same race for a file with no row at all (a retried delivery whose first tx failed)
    let orphan = Bytes::from_static(b"Subject: retry\r\n\r\nbody\r\n");
    let ho = e.blobs.put(orphan.clone()).await.unwrap();
    let po = blob_path(&root(&e), &ho);
    set_mtime(&po, SystemTime::now() - 3 * DAY);
    e.blobs.put(orphan.clone()).await.unwrap();
    let r = collect_blobs(&e.store, e.blobs.as_ref(), HOUR)
        .await
        .unwrap();
    assert_eq!(counts(&r), (0, 0, 0));
    assert!(po.is_file());
}

#[tokio::test]
async fn never_deletes_referenced_blob_even_if_zero_since_is_old() {
    let e = env().await;
    let msg = raw("keep", None);
    let h = BlobHash::of(&msg);
    e.mb.append(INBOX, msg.clone(), Flags::default())
        .await
        .unwrap();
    let p = blob_path(&root(&e), &h);
    set_mtime(&p, SystemTime::now() - 30 * DAY);

    // corrupt: referenced, refcount > 0, but zero_since is ancient
    exec(
        &e,
        &format!(
            "UPDATE blobs SET zero_since = 0 WHERE hash='{}'",
            h.as_str()
        ),
    )
    .await;
    let far = SystemTime::now() + 365 * DAY;
    let r = collect_blobs_at(&e.store, e.blobs.as_ref(), Duration::ZERO, far)
        .await
        .unwrap();
    assert_eq!(counts(&r), (0, 0, 0));
    assert!(r.errors.is_empty(), "{:?}", r.errors);

    // worse: refcount forced to 0 while a message still references it -> the FK refuses
    exec(
        &e,
        &format!("UPDATE blobs SET refcount = 0 WHERE hash='{}'", h.as_str()),
    )
    .await;
    exec(
        &e,
        &format!(
            "UPDATE blobs SET zero_since = 0 WHERE hash='{}'",
            h.as_str()
        ),
    )
    .await;
    let r = collect_blobs_at(&e.store, e.blobs.as_ref(), Duration::ZERO, far)
        .await
        .unwrap();
    assert_eq!(counts(&r), (0, 0, 0));
    assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
    assert!(r.errors[0].contains(h.as_str()));
    assert!(has_row(&e, &h).await);
    assert_eq!(e.blobs.get(&h).await.unwrap(), msg);
}

#[tokio::test]
async fn gc_errors_on_one_blob_do_not_stop_others() {
    let e = env().await;
    let r0 = root(&e);
    let bad = BlobHash::parse(&"1".repeat(64)).unwrap();
    let good = e.blobs.put(Bytes::from_static(b"good")).await.unwrap();
    let orphan = e.blobs.put(Bytes::from_static(b"orphan")).await.unwrap();
    // a directory squats on `bad`'s blob path: removing it as a file fails
    std::fs::create_dir_all(blob_path(&r0, &bad)).unwrap();
    for h in [&bad, &good] {
        exec(
            &e,
            &format!(
                "INSERT INTO blobs (hash, size) VALUES ('{}', 4)",
                h.as_str()
            ),
        )
        .await;
    }
    exec(&e, "UPDATE blobs SET zero_since = 0").await;
    let old = SystemTime::now() - 2 * DAY;
    set_mtime(&blob_path(&r0, &good), old);
    set_mtime(&blob_path(&r0, &orphan), old);
    let stale_tmp = blob_path(&r0, &good).with_file_name(format!(".{}.1.1.tmp", good.as_str()));
    std::fs::write(&stale_tmp, b"partial").unwrap();
    set_mtime(&stale_tmp, old);

    let r = collect_blobs(&e.store, e.blobs.as_ref(), HOUR)
        .await
        .unwrap();
    assert_eq!(counts(&r), (2, 2, 1), "{:?}", r.errors);
    assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
    assert!(r.errors[0].contains(bad.as_str()));
    let tmp = e.dir.path().to_string_lossy().to_string();
    assert!(
        !r.errors[0].contains(&tmp),
        "no filesystem paths in errors: {}",
        r.errors[0]
    );
    assert!(!blob_path(&r0, &good).exists());
    assert!(!blob_path(&r0, &orphan).exists());
    assert!(!stale_tmp.exists());
    assert!(blob_path(&r0, &bad).is_dir());
}

#[tokio::test]
async fn stale_tmp_files_are_swept_fresh_ones_kept() {
    let e = env().await;
    let h = e.blobs.put(Bytes::from_static(b"base")).await.unwrap();
    exec(
        &e,
        &format!(
            "INSERT INTO blobs (hash, size) VALUES ('{}', 4)",
            h.as_str()
        ),
    )
    .await;
    let d = blob_path(&root(&e), &h).parent().unwrap().to_path_buf();
    // left behind by a put future dropped mid-write
    let stale = d.join(format!(".{}.4242.0.tmp", h.as_str()));
    let fresh = d.join(format!(".{}.4242.1.tmp", h.as_str()));
    std::fs::write(&stale, b"half").unwrap();
    std::fs::write(&fresh, b"half").unwrap();
    set_mtime(&stale, SystemTime::now() - 2 * HOUR);

    let r = collect_blobs(&e.store, e.blobs.as_ref(), HOUR)
        .await
        .unwrap();
    assert_eq!(counts(&r), (0, 0, 1));
    assert!(!stale.exists());
    assert!(fresh.exists());
    assert!(blob_path(&root(&e), &h).is_file());
}

#[tokio::test]
async fn collect_ignores_symlinks_and_odd_names() {
    let e = env().await;
    let r0 = root(&e);
    std::fs::create_dir_all(&r0).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let old = SystemTime::now() - 10 * DAY;
    let file = |p: &Path| {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"junk").unwrap();
        set_mtime(p, old);
    };
    let hash = |prefix: &str| format!("{prefix}{}", "0".repeat(64 - prefix.len()));

    // a symlink with a valid blob name, pointing outside the blob root
    let target = outside.path().join("target");
    file(&target);
    let link = r0.join("ab").join("cd").join(hash("abcd"));
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    // a symlinked shard directory
    let outside_shard = outside.path().join("shard");
    let behind = outside_shard.join("12").join(hash("ef12"));
    file(&behind);
    std::os::unix::fs::symlink(&outside_shard, r0.join("ef")).unwrap();
    // a symlinked second-level directory
    let outside_l2 = outside.path().join("l2");
    let behind2 = outside_l2.join(hash("7788"));
    file(&behind2);
    std::fs::create_dir_all(r0.join("77")).unwrap();
    std::os::unix::fs::symlink(&outside_l2, r0.join("77").join("88")).unwrap();

    let d = r0.join("01").join("23");
    let odd: Vec<PathBuf> = vec![
        r0.join("README"),
        r0.join("zz").join("23").join(hash("zz23")),
        r0.join("01").join("2"),
        r0.join("01").join("zz").join(hash("01")),
        d.join(hash("4567")),                    // wrong shard
        d.join(hash("0123abc").to_uppercase()),  // uppercase
        d.join(format!("{}.bak", hash("0123"))), // suffix
        d.join(&hash("0123")[..63]),             // too short
        d.join("0123zzzz"),
    ];
    for p in &odd {
        file(p);
    }
    let non_utf8 = d.join(std::ffi::OsStr::from_bytes(&[b'0', b'1', 0xff, 0xfe]));
    file(&non_utf8);
    let dir_like_hash = d.join(hash("01231"));
    std::fs::create_dir_all(dir_like_hash.join("inner")).unwrap();
    for i in 0..3000 {
        file(&d.join(format!("junk-{i}")));
    }
    // one real orphan in the same directory: GC must reach and delete it
    let real = d.join(hash("0123"));
    file(&real);

    let listed = e.blobs.list().await.unwrap();
    let names: Vec<&str> = listed.iter().map(|b| b.hash.as_str()).collect();
    assert_eq!(names, vec![hash("0123").as_str()]);

    let r = collect_blobs_at(
        &e.store,
        e.blobs.as_ref(),
        Duration::ZERO,
        SystemTime::now() + DAY,
    )
    .await
    .unwrap();
    assert_eq!(counts(&r), (0, 1, 0), "{:?}", r.errors);
    assert!(!real.exists());
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    for p in odd.iter().chain([&target, &behind, &behind2, &non_utf8]) {
        assert!(p.exists(), "{p:?} must survive");
    }
    assert!(dir_like_hash.join("inner").is_dir());
    assert!(std::fs::symlink_metadata(r0.join("ef"))
        .unwrap()
        .file_type()
        .is_symlink());
}
