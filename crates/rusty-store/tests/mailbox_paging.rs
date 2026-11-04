//! Pagination caps, typed change items and thread ids, never-reused folder ids.

mod common;

use common::*;
use rusty_core::{Error, Flags, FolderId, MessageId, ThreadId};
use rusty_store::{ChangeItem, ChangeOp, MAX_PAGE};

async fn append_n(e: &Env, n: usize) {
    for i in 0..n {
        e.mb.append(
            INBOX,
            raw(&format!("m{i}"), Some(&format!("m{i}@x"))),
            Flags::empty(),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn list_folder_pages_by_uid() {
    for_each_env(|e| async move {
        append_n(&e, 25).await;
        let uids =
            |v: Vec<rusty_store::MessageSummary>| v.into_iter().map(|s| s.uid).collect::<Vec<_>>();
        assert_eq!(
            uids(e.mb.list_folder(INBOX, 0, 10).await.unwrap()),
            (1..=10).collect::<Vec<_>>()
        );
        assert_eq!(
            uids(e.mb.list_folder(INBOX, 10, 10).await.unwrap()),
            (11..=20).collect::<Vec<_>>()
        );
        assert_eq!(
            uids(e.mb.list_folder(INBOX, 20, 10).await.unwrap()),
            (21..=25).collect::<Vec<_>>()
        );
        assert!(e.mb.list_folder(INBOX, 25, 10).await.unwrap().is_empty());
        assert!(e
            .mb
            .list_folder(INBOX, u32::MAX, 10)
            .await
            .unwrap()
            .is_empty());
    })
    .await;
}

#[tokio::test]
async fn zero_limit_is_invalid() {
    for_each_env(|e| async move {
        let r = e.mb.list_folder(INBOX, 0, 0).await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
        let r = e.mb.changes_since(0, 0).await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
    })
    .await;
}

#[tokio::test]
async fn list_folder_limit_is_clamped() {
    for_each_env(|e| async move {
        let h = "ab".repeat(32);
        insert_blob_row(&e.b, &h, 10).await;
        // 1,005 messages inserted directly (no change log needed for a listing).
        e.exec(
            &format!(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1005) \
             INSERT INTO messages (mailbox_id, id, folder_id, uid, blob_hash, size, internal_date, \
             thread_id) SELECT {mb}, i, 1, i, {hash}, 10, 0, i FROM n",
                mb = e.mb.id().0,
                hash = hash_lit(&e.b, &h)
            ),
            &[],
        )
        .await;
        let page = e.mb.list_folder(INBOX, 0, u32::MAX).await.unwrap();
        assert_eq!(page.len(), MAX_PAGE as usize);
        assert_eq!(page.last().unwrap().uid, 1000);
        let rest = e.mb.list_folder(INBOX, 1000, u32::MAX).await.unwrap();
        assert_eq!(rest.len(), 5);
    })
    .await;
}

#[tokio::test]
async fn changes_since_pages_with_more_flag() {
    for_each_env(|e| async move {
        append_n(&e, 25).await;
        let p1 = e.mb.changes_since(0, 10).await.unwrap();
        assert_eq!(p1.changes.len(), 10);
        assert!(p1.more);
        let p2 =
            e.mb.changes_since(p1.changes.last().unwrap().seq, 10)
                .await
                .unwrap();
        assert_eq!(p2.changes.first().unwrap().seq, 11);
        assert!(p2.more);
        // Exactly the rest: more is false.
        let p3 = e.mb.changes_since(20, 5).await.unwrap();
        assert_eq!(p3.changes.len(), 5);
        assert!(!p3.more);
        let p4 = e.mb.changes_since(25, 10).await.unwrap();
        assert!(p4.changes.is_empty() && !p4.more);
    })
    .await;
}

#[tokio::test]
async fn changes_since_limit_is_clamped() {
    for_each_env(|e| async move {
        e.exec(
            &format!(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1500) \
             INSERT INTO change_log (mailbox_id, seq, kind, item_id, op) \
             SELECT {mb}, i, 'message', i, 'create' FROM n",
                mb = e.mb.id().0
            ),
            &[],
        )
        .await;
        let p = e.mb.changes_since(0, u32::MAX).await.unwrap();
        assert_eq!(p.changes.len(), MAX_PAGE as usize);
        assert!(p.more);
        let p = e.mb.changes_since(1000, u32::MAX).await.unwrap();
        assert_eq!(p.changes.len(), 500);
        assert!(!p.more);
    })
    .await;
}

#[tokio::test]
async fn change_items_and_thread_ids_are_typed() {
    for_each_env(|e| async move {
        let f = e.mb.create_folder(None, "Work").await.unwrap();
        let m =
            e.mb.append(f, raw("s", Some("s@x")), Flags::empty())
                .await
                .unwrap();
        let items: Vec<(ChangeItem, ChangeOp)> =
            e.mb.changes_since(0, 10)
                .await
                .unwrap()
                .changes
                .into_iter()
                .map(|c| (c.item, c.op))
                .collect();
        assert_eq!(
            items,
            vec![
                (ChangeItem::Folder(FolderId(6)), ChangeOp::Create),
                (ChangeItem::Message(MessageId(1)), ChangeOp::Create),
            ]
        );
        let list = e.mb.list_folder(f, 0, 10).await.unwrap();
        assert_eq!(list[0].thread_id, ThreadId(m.id.0));
    })
    .await;
}

// ---------- folder ids are never reused (migration 0003) ----------

async fn next_folder_id(e: &Env) -> i64 {
    e.opt_i64(
        "SELECT next_folder_id FROM mailboxes WHERE id = ?",
        &[Bind::I(e.mb.id().0)],
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn deleted_folder_id_is_not_reused() {
    for_each_env(|e| async move {
        let a = e.mb.create_folder(None, "A").await.unwrap();
        assert_eq!(a, FolderId(6));
        e.mb.delete_folder(a).await.unwrap();
        let b = e.mb.create_folder(None, "A").await.unwrap();
        assert_ne!(a, b);
        assert_eq!(b, FolderId(7));
        assert_eq!(next_folder_id(&e).await, 8);

        // Nested: delete the highest id, then create again.
        let c = e.mb.create_folder(Some(b), "C").await.unwrap();
        e.mb.delete_folder(c).await.unwrap();
        let d = e.mb.create_folder(Some(b), "C").await.unwrap();
        assert!(d.0 > c.0, "{d:?} after {c:?}");

        // The change log never shows a destroy followed by a create of the same folder id.
        let changes = e.mb.changes_since(0, 1000).await.unwrap().changes;
        for (i, ch) in changes.iter().enumerate() {
            if let (ChangeItem::Folder(id), ChangeOp::Destroy) = (ch.item, ch.op) {
                assert!(
                    !changes[i + 1..]
                        .iter()
                        .any(|c| c.item == ChangeItem::Folder(id) && c.op == ChangeOp::Create),
                    "folder id {id:?} recreated"
                );
            }
        }
    })
    .await;
}

#[tokio::test]
async fn failed_create_folder_does_not_consume_an_id() {
    for_each_env(|e| async move {
        e.mb.create_folder(None, "A").await.unwrap();
        let before = next_folder_id(&e).await;
        assert!(matches!(
            e.mb.create_folder(None, "A").await,
            Err(Error::Conflict(_))
        ));
        assert_eq!(
            next_folder_id(&e).await,
            before,
            "rolled back with the transaction"
        );
    })
    .await;
}

/// Backfill of `mailboxes.next_folder_id` from existing folders (migration 0003 applied to a
/// database at schema version 0002).
#[tokio::test]
// backend-neutral: SQLite-only, replays the frozen SQLite migration 0003 against a version-2 database
async fn migration_0003_backfills_next_folder_id() {
    use sqlx::sqlite::SqlitePoolOptions;
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    for sql in [
        include_str!("../migrations/0001_init.sql"),
        include_str!("../migrations/0002_blob_zero_since.sql"),
    ] {
        sqlx::raw_sql(sql).execute(&pool).await.unwrap();
    }
    sqlx::raw_sql(
        "INSERT INTO domains (id, name, created_at) VALUES (1, 'example.com', 0); \
         INSERT INTO accounts (id, local_part, domain_id, status, created_at) \
           VALUES (1, 'a', 1, 'active', 0), (2, 'b', 1, 'active', 0); \
         INSERT INTO mailboxes (id, account_id, uid_validity) VALUES (1, 1, 7), (2, 2, 7); \
         INSERT INTO folders (mailbox_id, id, name, role, uid_validity) VALUES \
           (1, 1, 'Inbox', 'inbox', 7), (1, 2, 'Sent', 'sent', 7), (1, 9, 'X', 'custom', 7);",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0003_folder_id_counter.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let got: Vec<(i64, i64)> =
        sqlx::query_as("SELECT id, next_folder_id FROM mailboxes ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(got, vec![(1, 10), (2, 1)]);
    let r = sqlx::query("UPDATE mailboxes SET next_folder_id = 0 WHERE id = 1")
        .execute(&pool)
        .await;
    assert!(r.is_err(), "CHECK (next_folder_id >= 1)");
}
