mod common;

use std::time::Duration;

use common::*;
use rusty_core::{AccountId, BlobHash, Error, Flags, FolderId, MessageId, ThreadId};
use rusty_store::{Change, ChangeItem, ChangeOp, Mailbox, MailboxLimits, MessageRef, MAX_PAGE};

fn mref(id: i64, uid: u32) -> MessageRef {
    MessageRef {
        id: MessageId(id),
        uid,
    }
}

fn change(seq: i64, item: ChangeItem, op: ChangeOp) -> Change {
    Change { seq, item, op }
}

async fn keywords(e: &Env, mb: &Mailbox, id: i64) -> Vec<String> {
    e.strings(
        "SELECT keyword FROM keywords WHERE mailbox_id = ? AND message_id = ? ORDER BY keyword",
        &[Bind::I(mb.id().0), Bind::I(id)],
    )
    .await
}

// ---------- open ----------

#[tokio::test]
async fn open_without_mailbox_is_not_found() {
    for_each_env(|e| async move {
        let r = e.reopen(AccountId(999)).await;
        assert!(matches!(r, Err(Error::NotFound(_))), "{r:?}");
    })
    .await;
}

#[tokio::test]
async fn default_limit_is_50_mib() {
    for_each_env(|e| async move {
        assert_eq!(MailboxLimits::default().max_message_bytes, 50 * 1024 * 1024);
        assert_eq!(e.mb.limits(), MailboxLimits::default());
    })
    .await;
}

// ---------- append ----------

#[tokio::test]
async fn append_assigns_increasing_ids_and_uids() {
    for_each_env(|e| async move {
        let a =
            e.mb.append(INBOX, raw("first", Some("a@x")), Flags::empty())
                .await
                .unwrap();
        let b =
            e.mb.append(INBOX, raw("second", Some("b@x")), Flags::SEEN)
                .await
                .unwrap();
        let c =
            e.mb.append(SENT, raw("third", Some("c@x")), Flags::empty())
                .await
                .unwrap();
        assert_eq!(a, mref(1, 1));
        assert_eq!(b, mref(2, 2));
        assert_eq!(c, mref(3, 1));

        let list = e.mb.list_folder(INBOX, 0, MAX_PAGE).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, MessageId(1));
        assert_eq!(list[0].uid, 1);
        assert_eq!(list[0].subject, "first");
        assert_eq!(list[0].from_addr, "bob@example.org");
        assert_eq!(list[0].size, raw("first", Some("a@x")).len() as i64);
        assert_eq!(list[0].flags, Flags::empty());
        assert!(list[0].internal_date > 1_700_000_000);
        assert_eq!(list[1].uid, 2);
        assert_eq!(list[1].flags, Flags::SEEN);

        let ch = e.mb.changes_since(0, MAX_PAGE).await.unwrap().changes;
        assert_eq!(
            ch,
            vec![
                change(1, ChangeItem::Message(MessageId(1)), ChangeOp::Create),
                change(2, ChangeItem::Message(MessageId(2)), ChangeOp::Create),
                change(3, ChangeItem::Message(MessageId(3)), ChangeOp::Create),
            ]
        );
        assert_eq!(
            e.mb.changes_since(2, MAX_PAGE).await.unwrap().changes.len(),
            1
        );
        assert_eq!(e.scalar("SELECT refcount FROM blobs LIMIT 1").await, 1);
    })
    .await;
}

#[tokio::test]
async fn append_same_content_twice_shares_blob() {
    for_each_env(|e| async move {
        e.mb.append(INBOX, raw("dup", Some("d@x")), Flags::empty())
            .await
            .unwrap();
        e.mb.append(INBOX, raw("dup", Some("d@x")), Flags::empty())
            .await
            .unwrap();
        assert_eq!(e.scalar("SELECT COUNT(*) FROM blobs").await, 1);
        assert_eq!(e.scalar("SELECT refcount FROM blobs").await, 2);
        assert_eq!(e.blob_files().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn append_without_message_id_ok() {
    for_each_env(|e| async move {
        let r =
            e.mb.append(INBOX, raw("no id", None), Flags::empty())
                .await
                .unwrap();
        assert_eq!(r, mref(1, 1));
        let list = e.mb.list_folder(INBOX, 0, MAX_PAGE).await.unwrap();
        assert_eq!(list[0].thread_id, ThreadId(1));
        assert_eq!(
            e.scalar("SELECT COUNT(*) FROM messages WHERE message_id_hdr IS NULL")
                .await,
            1
        );
    })
    .await;
}

#[tokio::test]
async fn append_garbage_bytes_ok() {
    for_each_env(|e| async move {
        let r =
            e.mb.append(
                INBOX,
                bytes::Bytes::from_static(&[0xff, 0x00, 0xfe, b'\n']),
                Flags::empty(),
            )
            .await
            .unwrap();
        assert_eq!(r, mref(1, 1));
    })
    .await;
}

#[tokio::test]
async fn append_empty_is_invalid() {
    for_each_env(|e| async move {
        let r =
            e.mb.append(INBOX, bytes::Bytes::new(), Flags::empty())
                .await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
        assert!(e.blob_files().is_empty());
        assert!(e
            .mb
            .changes_since(0, MAX_PAGE)
            .await
            .unwrap()
            .changes
            .is_empty());
    })
    .await;
}

#[tokio::test]
async fn append_over_limit_is_quota() {
    for_each_env(|e| async move {
        let mb = e.mb.clone().with_limits(MailboxLimits {
            max_message_bytes: 100,
        });
        let r = mb.append(INBOX, sized(101), Flags::empty()).await;
        assert!(matches!(r, Err(Error::Quota(_))), "{r:?}");
        assert!(e.blob_files().is_empty(), "no blob written on rejection");
        assert_eq!(e.scalar("SELECT COUNT(*) FROM messages").await, 0);
        assert_eq!(e.scalar("SELECT next_msg_id FROM mailboxes").await, 1);

        let ok = mb.append(INBOX, sized(100), Flags::empty()).await.unwrap();
        assert_eq!(ok, mref(1, 1));
        assert_eq!(e.blob_files().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn append_to_foreign_folder_is_not_found() {
    for_each_env(|e| async move {
        let bob = e.other("bob").await;
        let bobs = bob.create_folder(None, "Work").await.unwrap();
        assert_eq!(bobs, FolderId(6));

        for f in [bobs, FolderId(0), FolderId(-1), FolderId(99)] {
            let r = e.mb.append(f, raw("x", Some("x@x")), Flags::empty()).await;
            assert!(matches!(r, Err(Error::NotFound(_))), "{f:?}: {r:?}");
        }
        assert!(
            e.blob_files().is_empty(),
            "folder is checked before the blob is written"
        );
        assert_eq!(e.scalar("SELECT COUNT(*) FROM messages").await, 0);
    })
    .await;
}

#[tokio::test]
async fn failed_transaction_leaves_orphan_blob_not_metadata() {
    for_each_env(|e| async move {
        inject_insert_failure(&e.b, "messages").await;

        let body = raw("doomed", Some("doom@x"));
        let r = e.mb.append(INBOX, body.clone(), Flags::empty()).await;
        assert!(r.is_err(), "{r:?}");

        assert_eq!(e.scalar("SELECT COUNT(*) FROM messages").await, 0);
        assert_eq!(e.scalar("SELECT COUNT(*) FROM change_log").await, 0);
        assert_eq!(e.scalar("SELECT COUNT(*) FROM blobs").await, 0);
        assert_eq!(e.scalar("SELECT next_msg_id FROM mailboxes").await, 1);
        assert_eq!(e.scalar("SELECT next_change_seq FROM mailboxes").await, 1);
        assert_eq!(
            e.scalar("SELECT next_uid FROM folders WHERE id = 1").await,
            1
        );
        // The blob was written before the transaction and is now an orphan.
        assert_eq!(e.blob_files().len(), 1);
        assert_eq!(e.blobs.get(&BlobHash::of(&body)).await.unwrap(), body);

        // Once the fault is gone, numbering continues without gaps.
        clear_injected_failure(&e.b, "messages").await;
        assert_eq!(
            e.mb.append(INBOX, body, Flags::empty()).await.unwrap(),
            mref(1, 1)
        );
        assert_eq!(
            e.mb.changes_since(0, MAX_PAGE).await.unwrap().changes[0].seq,
            1
        );
    })
    .await;
}

// ---------- flags / keywords ----------

#[tokio::test]
async fn set_flags_logs_update() {
    for_each_env(|e| async move {
        e.mb.append(INBOX, raw("m", Some("m@x")), Flags::empty())
            .await
            .unwrap();
        e.mb.set_flags(MessageId(1), Flags::SEEN | Flags::FLAGGED)
            .await
            .unwrap();
        let list = e.mb.list_folder(INBOX, 0, MAX_PAGE).await.unwrap();
        assert_eq!(list[0].flags, Flags::SEEN | Flags::FLAGGED);
        let ch = e.mb.changes_since(1, MAX_PAGE).await.unwrap().changes;
        assert_eq!(
            ch,
            vec![change(
                2,
                ChangeItem::Message(MessageId(1)),
                ChangeOp::Update
            )]
        );

        let r = e.mb.set_flags(MessageId(2), Flags::SEEN).await;
        assert!(matches!(r, Err(Error::NotFound(_))), "{r:?}");
    })
    .await;
}

#[tokio::test]
async fn keyword_is_lowercased_and_idempotent() {
    for_each_env(|e| async move {
        e.mb.append(INBOX, raw("m", Some("m@x")), Flags::empty())
            .await
            .unwrap();
        e.mb.add_keyword(MessageId(1), "$Label1").await.unwrap();
        e.mb.add_keyword(MessageId(1), "$LABEL1").await.unwrap();
        e.mb.add_keyword(MessageId(1), "Work").await.unwrap();
        assert_eq!(keywords(&e, &e.mb, 1).await, vec!["$label1", "work"]);
        // create + two real additions; the duplicate logs nothing
        assert_eq!(
            e.mb.changes_since(0, MAX_PAGE).await.unwrap().changes.len(),
            3
        );

        e.mb.remove_keyword(MessageId(1), "WORK").await.unwrap();
        assert_eq!(keywords(&e, &e.mb, 1).await, vec!["$label1"]);
        let last =
            e.mb.changes_since(0, MAX_PAGE)
                .await
                .unwrap()
                .changes
                .pop()
                .unwrap();
        assert_eq!(
            last,
            change(4, ChangeItem::Message(MessageId(1)), ChangeOp::Update)
        );
        // removing an absent keyword is a no-op
        e.mb.remove_keyword(MessageId(1), "absent").await.unwrap();
        assert_eq!(
            e.mb.changes_since(0, MAX_PAGE).await.unwrap().changes.len(),
            4
        );

        let r = e.mb.add_keyword(MessageId(9), "x").await;
        assert!(matches!(r, Err(Error::NotFound(_))), "{r:?}");
    })
    .await;
}

#[tokio::test]
async fn keyword_with_invalid_chars_is_invalid() {
    for_each_env(|e| async move {
        e.mb.append(INBOX, raw("m", Some("m@x")), Flags::empty())
            .await
            .unwrap();
        let long = "k".repeat(256);
        let bad = [
            "",
            " ",
            "a b",
            "a\tb",
            "a(b",
            "a)b",
            "a{b",
            "a%b",
            "a*b",
            "a\"b",
            "a\\b",
            "\\Seen",
            "a]b",
            "caf\u{e9}",
            "a\u{7f}",
            "a\0",
            long.as_str(),
        ];
        for kw in bad {
            let r = e.mb.add_keyword(MessageId(1), kw).await;
            assert!(matches!(r, Err(Error::Invalid(_))), "{kw:?}: {r:?}");
            let r = e.mb.remove_keyword(MessageId(1), kw).await;
            assert!(matches!(r, Err(Error::Invalid(_))), "{kw:?}: {r:?}");
        }
        e.mb.add_keyword(MessageId(1), &"k".repeat(255))
            .await
            .unwrap();
        e.mb.add_keyword(MessageId(1), "$Forwarded").await.unwrap();
        e.mb.add_keyword(MessageId(1), "a[b!#&'+-.:;<=>?@^_`|~")
            .await
            .unwrap();
        assert_eq!(keywords(&e, &e.mb, 1).await.len(), 3);
    })
    .await;
}

#[tokio::test]
async fn keyword_count_over_32_is_quota() {
    for_each_env(|e| async move {
        e.mb.append(INBOX, raw("m", Some("m@x")), Flags::empty())
            .await
            .unwrap();
        for i in 0..32 {
            e.mb.add_keyword(MessageId(1), &format!("kw{i}"))
                .await
                .unwrap();
        }
        let r = e.mb.add_keyword(MessageId(1), "one-too-many").await;
        assert!(matches!(r, Err(Error::Quota(_))), "{r:?}");
        // re-adding an existing one is still fine
        e.mb.add_keyword(MessageId(1), "KW0").await.unwrap();
        assert_eq!(keywords(&e, &e.mb, 1).await.len(), 32);
    })
    .await;
}

// ---------- move / expunge ----------

#[tokio::test]
async fn move_changes_uid_and_logs_change() {
    for_each_env(|e| async move {
        e.mb.append(INBOX, raw("a", Some("a@x")), Flags::empty())
            .await
            .unwrap();
        e.mb.append(INBOX, raw("b", Some("b@x")), Flags::empty())
            .await
            .unwrap();
        e.mb.append(SENT, raw("c", Some("c@x")), Flags::empty())
            .await
            .unwrap();

        let moved = e.mb.move_message(MessageId(1), SENT).await.unwrap();
        assert_eq!(moved, mref(1, 2));
        let inbox = e.mb.list_folder(INBOX, 0, MAX_PAGE).await.unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].id, MessageId(2));
        let sent: Vec<(i64, u32)> =
            e.mb.list_folder(SENT, 0, MAX_PAGE)
                .await
                .unwrap()
                .iter()
                .map(|m| (m.id.0, m.uid))
                .collect();
        assert_eq!(sent, vec![(3, 1), (1, 2)]);
        let ch = e.mb.changes_since(3, MAX_PAGE).await.unwrap().changes;
        assert_eq!(
            ch,
            vec![change(
                4,
                ChangeItem::Message(MessageId(1)),
                ChangeOp::Update
            )]
        );

        // Moving to the folder it is already in allocates nothing.
        assert_eq!(
            e.mb.move_message(MessageId(1), SENT).await.unwrap(),
            mref(1, 2)
        );
        assert_eq!(
            e.mb.changes_since(0, MAX_PAGE).await.unwrap().changes.len(),
            4
        );
        assert_eq!(
            e.scalar("SELECT next_uid FROM folders WHERE id = 2").await,
            3
        );

        // Moving back gets a fresh uid in INBOX (never the old uid 1).
        assert_eq!(
            e.mb.move_message(MessageId(1), INBOX).await.unwrap(),
            mref(1, 3)
        );

        let r = e.mb.move_message(MessageId(1), FolderId(42)).await;
        assert!(matches!(r, Err(Error::NotFound(_))), "{r:?}");
        let r = e.mb.move_message(MessageId(42), SENT).await;
        assert!(matches!(r, Err(Error::NotFound(_))), "{r:?}");
    })
    .await;
}

#[tokio::test]
async fn expunge_hides_message_but_keeps_blob() {
    for_each_env(|e| async move {
        let body = raw("bye", Some("bye@x"));
        e.mb.append(INBOX, body.clone(), Flags::empty())
            .await
            .unwrap();
        e.mb.expunge(MessageId(1)).await.unwrap();

        assert!(e
            .mb
            .list_folder(INBOX, 0, MAX_PAGE)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(e.blobs.get(&BlobHash::of(&body)).await.unwrap(), body);
        assert_eq!(e.scalar("SELECT refcount FROM blobs").await, 1);
        assert_eq!(
            e.scalar("SELECT COUNT(*) FROM messages WHERE deleted_at IS NOT NULL")
                .await,
            1
        );
        let ch = e.mb.changes_since(1, MAX_PAGE).await.unwrap().changes;
        assert_eq!(
            ch,
            vec![change(
                2,
                ChangeItem::Message(MessageId(1)),
                ChangeOp::Destroy
            )]
        );

        for r in [
            e.mb.expunge(MessageId(1)).await,
            e.mb.set_flags(MessageId(1), Flags::SEEN).await,
            e.mb.add_keyword(MessageId(1), "x").await,
            e.mb.move_message(MessageId(1), SENT).await.map(|_| ()),
        ] {
            assert!(matches!(r, Err(Error::NotFound(_))), "{r:?}");
        }
    })
    .await;
}

#[tokio::test]
async fn uid_is_never_reused_after_expunge() {
    for_each_env(|e| async move {
        assert_eq!(
            e.mb.append(INBOX, raw("1", Some("1@x")), Flags::empty())
                .await
                .unwrap(),
            mref(1, 1)
        );
        e.mb.expunge(MessageId(1)).await.unwrap();
        assert_eq!(
            e.mb.append(INBOX, raw("2", Some("2@x")), Flags::empty())
                .await
                .unwrap(),
            mref(2, 2)
        );
    })
    .await;
}

// ---------- folders ----------

#[tokio::test]
async fn create_rename_move_folder_log_changes() {
    for_each_env(|e| async move {
        let a = e.mb.create_folder(None, "A").await.unwrap();
        let b = e.mb.create_folder(Some(a), "B").await.unwrap();
        assert_eq!((a, b), (FolderId(6), FolderId(7)));
        e.mb.rename_folder(b, "Bee").await.unwrap();
        e.mb.move_folder(b, None).await.unwrap();
        e.mb.delete_folder(b).await.unwrap();
        let ch = e.mb.changes_since(0, MAX_PAGE).await.unwrap().changes;
        assert_eq!(
            ch,
            vec![
                change(1, ChangeItem::Folder(FolderId(6)), ChangeOp::Create),
                change(2, ChangeItem::Folder(FolderId(7)), ChangeOp::Create),
                change(3, ChangeItem::Folder(FolderId(7)), ChangeOp::Update),
                change(4, ChangeItem::Folder(FolderId(7)), ChangeOp::Update),
                change(5, ChangeItem::Folder(FolderId(7)), ChangeOp::Destroy),
            ]
        );
        let mbid = e.mb.id().0;
        assert_eq!(
            e.opt_i64(
                "SELECT parent_id FROM folders WHERE mailbox_id = ? AND id = 6",
                &[Bind::I(mbid)]
            )
            .await,
            None
        );
        assert_eq!(
            e.strings(
                "SELECT name FROM folders WHERE mailbox_id = ? AND id = 6",
                &[Bind::I(mbid)]
            )
            .await,
            vec!["A".to_string()]
        );
        // ids come from mailboxes.next_folder_id: after deleting 7, the next folder is 8
        assert_eq!(e.mb.create_folder(None, "C").await.unwrap(), FolderId(8));
    })
    .await;
}

#[tokio::test]
async fn create_folder_with_slash_is_invalid() {
    for_each_env(|e| async move {
        let long = "n".repeat(256);
        for name in [
            "a/b",
            "/",
            "",
            ".",
            "..",
            "a\u{7}",
            "tab\there",
            "nl\n",
            "\u{85}",
            long.as_str(),
        ] {
            let r = e.mb.create_folder(None, name).await;
            assert!(matches!(r, Err(Error::Invalid(_))), "{name:?}: {r:?}");
        }
        let f = e.mb.create_folder(None, &"n".repeat(255)).await.unwrap();
        let r = e.mb.rename_folder(f, "x/y").await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
        assert!(e.mb.create_folder(None, "...").await.is_ok());
        assert!(e.mb.create_folder(None, "Ünïcødé 名前").await.is_ok());
        assert!(e
            .mb
            .changes_since(0, MAX_PAGE)
            .await
            .unwrap()
            .changes
            .iter()
            .all(|c| c.op == ChangeOp::Create));
    })
    .await;
}

#[tokio::test]
async fn create_folder_duplicate_sibling_conflicts() {
    for_each_env(|e| async move {
        let a = e.mb.create_folder(None, "A").await.unwrap();
        let r = e.mb.create_folder(None, "A").await;
        assert!(matches!(r, Err(Error::Conflict(_))), "{r:?}");
        let r = e.mb.create_folder(None, "Inbox").await;
        assert!(matches!(r, Err(Error::Conflict(_))), "{r:?}");
        e.mb.create_folder(Some(a), "A").await.unwrap();
        let b = e.mb.create_folder(None, "B").await.unwrap();
        let r = e.mb.rename_folder(b, "A").await;
        assert!(matches!(r, Err(Error::Conflict(_))), "{r:?}");
        let r = e.mb.create_folder(Some(FolderId(77)), "C").await;
        assert!(matches!(r, Err(Error::NotFound(_))), "{r:?}");
    })
    .await;
}

#[tokio::test]
async fn delete_folder_with_messages_conflicts() {
    for_each_env(|e| async move {
        let f = e.mb.create_folder(None, "Work").await.unwrap();
        let child = e.mb.create_folder(Some(f), "Sub").await.unwrap();
        e.mb.append(child, raw("m", Some("m@x")), Flags::empty())
            .await
            .unwrap();

        let r = e.mb.delete_folder(f).await;
        assert!(matches!(r, Err(Error::Conflict(_))), "has children: {r:?}");
        let r = e.mb.delete_folder(child).await;
        assert!(matches!(r, Err(Error::Conflict(_))), "has messages: {r:?}");

        e.mb.move_message(MessageId(1), TRASH).await.unwrap();
        e.mb.delete_folder(child).await.unwrap();
        e.mb.delete_folder(f).await.unwrap();
        let last =
            e.mb.changes_since(0, MAX_PAGE)
                .await
                .unwrap()
                .changes
                .pop()
                .unwrap();
        assert_eq!(last.item, ChangeItem::Folder(f));
        assert_eq!(last.op, ChangeOp::Destroy);
        let r = e.mb.delete_folder(f).await;
        assert!(matches!(r, Err(Error::NotFound(_))), "{r:?}");
    })
    .await;
}

#[tokio::test]
async fn delete_folder_after_expunge_ok() {
    for_each_env(|e| async move {
        let f = e.mb.create_folder(None, "Tmp").await.unwrap();
        let body = raw("m", Some("m@x"));
        e.mb.append(f, body.clone(), Flags::empty()).await.unwrap();
        e.mb.expunge(MessageId(1)).await.unwrap();
        e.mb.delete_folder(f).await.unwrap();
        // expunged rows are purged, the blob row drops to refcount 0 and the file stays
        assert_eq!(e.scalar("SELECT COUNT(*) FROM messages").await, 0);
        assert_eq!(e.scalar("SELECT refcount FROM blobs").await, 0);
        assert_eq!(e.blobs.get(&BlobHash::of(&body)).await.unwrap(), body);
    })
    .await;
}

#[tokio::test]
async fn delete_system_folder_is_invalid() {
    for_each_env(|e| async move {
        for f in [INBOX, SENT, DRAFTS, TRASH, SPAM] {
            let r = e.mb.delete_folder(f).await;
            assert!(matches!(r, Err(Error::Invalid(_))), "{f:?}: {r:?}");
            let r = e.mb.rename_folder(f, "Other").await;
            assert!(matches!(r, Err(Error::Invalid(_))), "{f:?}: {r:?}");
        }
        let custom = e.mb.create_folder(None, "Custom").await.unwrap();
        let r = e.mb.move_folder(INBOX, Some(custom)).await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
        assert_eq!(e.scalar("SELECT COUNT(*) FROM folders").await, 6);
    })
    .await;
}

#[tokio::test]
async fn move_folder_into_descendant_conflicts() {
    for_each_env(|e| async move {
        let a = e.mb.create_folder(None, "A").await.unwrap();
        let b = e.mb.create_folder(Some(a), "B").await.unwrap();
        let c = e.mb.create_folder(Some(b), "C").await.unwrap();

        for target in [a, b, c] {
            let r = e.mb.move_folder(a, Some(target)).await;
            assert!(matches!(r, Err(Error::Conflict(_))), "{target:?}: {r:?}");
        }
        let r = e.mb.move_folder(a, Some(FolderId(99))).await;
        assert!(matches!(r, Err(Error::NotFound(_))), "{r:?}");
        let r = e.mb.move_folder(FolderId(99), None).await;
        assert!(matches!(r, Err(Error::NotFound(_))), "{r:?}");

        e.mb.move_folder(c, None).await.unwrap();
        e.mb.move_folder(a, Some(c)).await.unwrap();
        let parent = e
            .opt_i64(
                "SELECT parent_id FROM folders WHERE mailbox_id = ? AND id = ?",
                &[Bind::I(e.mb.id().0), Bind::I(a.0)],
            )
            .await;
        assert_eq!(parent, Some(c.0));
    })
    .await;
}

#[tokio::test]
async fn folder_depth_is_limited_to_64() {
    for_each_env(|e| async move {
        let mut parent = None;
        let mut chain = vec![];
        for i in 0..64 {
            let f = e.mb.create_folder(parent, &format!("d{i}")).await.unwrap();
            chain.push(f);
            parent = Some(f);
        }
        let r = e.mb.create_folder(parent, "too-deep").await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");

        // moving a 2-level subtree under a 63-deep folder would make 65 levels
        let x = e.mb.create_folder(None, "x").await.unwrap();
        e.mb.create_folder(Some(x), "y").await.unwrap();
        let r = e.mb.move_folder(x, Some(chain[62])).await;
        assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
        e.mb.move_folder(x, Some(chain[61])).await.unwrap();
    })
    .await;
}

// ---------- threads ----------

#[tokio::test]
async fn thread_id_shared_by_reply() {
    for_each_env(|e| async move {
        let p =
            e.mb.append(INBOX, raw("parent", Some("p@x")), Flags::empty())
                .await
                .unwrap();
        let r1 =
            e.mb.append(
                INBOX,
                reply("re", Some("r1@x"), Some("p@x"), &[]),
                Flags::empty(),
            )
            .await
            .unwrap();
        let r2 =
            e.mb.append(
                INBOX,
                reply("re re", Some("r2@x"), None, &["unknown@x", "r1@x"]),
                Flags::empty(),
            )
            .await
            .unwrap();
        // a reply whose parent is in another folder still joins the thread
        let r3 =
            e.mb.append(
                SENT,
                reply("re3", Some("r3@x"), Some("r2@x"), &[]),
                Flags::empty(),
            )
            .await
            .unwrap();
        let thread_of = |list: &[rusty_store::MessageSummary], id: MessageId| {
            list.iter().find(|m| m.id == id).unwrap().thread_id
        };
        let inbox = e.mb.list_folder(INBOX, 0, MAX_PAGE).await.unwrap();
        let sent = e.mb.list_folder(SENT, 0, MAX_PAGE).await.unwrap();
        assert_eq!(thread_of(&inbox, p.id), ThreadId(p.id.0));
        assert_eq!(thread_of(&inbox, r1.id), ThreadId(p.id.0));
        assert_eq!(thread_of(&inbox, r2.id), ThreadId(p.id.0));
        assert_eq!(thread_of(&sent, r3.id), ThreadId(p.id.0));
    })
    .await;
}

#[tokio::test]
async fn thread_prefers_last_reference_then_in_reply_to() {
    for_each_env(|e| async move {
        let a =
            e.mb.append(INBOX, raw("a", Some("a@x")), Flags::empty())
                .await
                .unwrap();
        let b =
            e.mb.append(INBOX, raw("b", Some("b@x")), Flags::empty())
                .await
                .unwrap();
        // References: <a> <b>, In-Reply-To: <a>  -> last reference (b) wins
        let c =
            e.mb.append(
                INBOX,
                reply("c", Some("c@x"), Some("a@x"), &["a@x", "b@x"]),
                Flags::empty(),
            )
            .await
            .unwrap();
        // unknown references, known In-Reply-To -> a
        let d =
            e.mb.append(
                INBOX,
                reply("d", Some("d@x"), Some("a@x"), &["zz@x"]),
                Flags::empty(),
            )
            .await
            .unwrap();
        let list = e.mb.list_folder(INBOX, 0, MAX_PAGE).await.unwrap();
        let t = |id: MessageId| list.iter().find(|m| m.id == id).unwrap().thread_id;
        assert_eq!(t(c.id), ThreadId(b.id.0));
        assert_eq!(t(d.id), ThreadId(a.id.0));
    })
    .await;
}

#[tokio::test]
async fn thread_id_own_id_when_no_parent() {
    for_each_env(|e| async move {
        e.mb.append(INBOX, raw("x", Some("x@x")), Flags::empty())
            .await
            .unwrap();
        let r =
            e.mb.append(
                INBOX,
                reply("orphan", Some("o@x"), Some("nope@x"), &["nada@x"]),
                Flags::empty(),
            )
            .await
            .unwrap();
        let list = e.mb.list_folder(INBOX, 0, MAX_PAGE).await.unwrap();
        assert_eq!(
            list.iter().find(|m| m.id == r.id).unwrap().thread_id,
            ThreadId(r.id.0)
        );
    })
    .await;
}

#[tokio::test]
async fn thread_lookup_is_scoped_to_mailbox() {
    for_each_env(|e| async move {
        let bob = e.other("bob").await;
        // Bob: thread 1 = { p@x (id 1), b2@x (id 2, reply to p@x) }
        bob.append(INBOX, raw("bob root", Some("p@x")), Flags::empty())
            .await
            .unwrap();
        let b2 = bob
            .append(
                INBOX,
                reply("bob reply", Some("b2@x"), Some("p@x"), &[]),
                Flags::empty(),
            )
            .await
            .unwrap();
        let foreign_thread = bob
            .list_folder(INBOX, 0, MAX_PAGE)
            .await
            .unwrap()
            .iter()
            .find(|m| m.id == b2.id)
            .unwrap()
            .thread_id;
        assert_eq!(foreign_thread, ThreadId(1));

        // Alice: two unrelated messages first, so her reply gets id 3.
        e.mb.append(INBOX, raw("mine 1", Some("m1@x")), Flags::empty())
            .await
            .unwrap();
        e.mb.append(INBOX, raw("mine 2", Some("m2@x")), Flags::empty())
            .await
            .unwrap();
        // Alice replies to Bob's b2@x, which exists only in Bob's mailbox.
        let r =
            e.mb.append(
                INBOX,
                reply("re", Some("re@x"), Some("b2@x"), &["p@x", "b2@x"]),
                Flags::empty(),
            )
            .await
            .unwrap();
        assert_eq!(r.id, MessageId(3));
        let list = e.mb.list_folder(INBOX, 0, MAX_PAGE).await.unwrap();
        let own_thread = list.iter().find(|m| m.id == r.id).unwrap().thread_id;
        // No cross-mailbox inheritance: the reply starts its own thread...
        assert_eq!(own_thread, ThreadId(r.id.0));
        // ...which an unscoped lookup (Bob's thread 1) would not have produced.
        assert_ne!(own_thread, foreign_thread);
    })
    .await;
}

// ---------- folder uid_validity (RFC 3501/9051 §2.3.1.1) ----------

async fn folder_uidv(e: &Env, mb: &Mailbox, f: FolderId) -> i64 {
    e.opt_i64(
        "SELECT uid_validity FROM folders WHERE mailbox_id = ? AND id = ?",
        &[Bind::I(mb.id().0), Bind::I(f.0)],
    )
    .await
    .unwrap()
}

async fn high_water(e: &Env, mb: &Mailbox) -> i64 {
    e.opt_i64(
        "SELECT uid_validity FROM mailboxes WHERE id = ?",
        &[Bind::I(mb.id().0)],
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn recreated_folder_gets_greater_uid_validity() {
    for_each_env(|e| async move {
        // create / delete / create the same name within the same second
        let f1 = e.mb.create_folder(None, "Projects").await.unwrap();
        let v1 = folder_uidv(&e, &e.mb, f1).await;
        e.mb.delete_folder(f1).await.unwrap();
        let f2 = e.mb.create_folder(None, "Projects").await.unwrap();
        let v2 = folder_uidv(&e, &e.mb, f2).await;
        assert_ne!(f1, f2, "folder ids are never reused");
        assert!(v2 > v1, "{v2} must be > {v1}");
        assert_eq!(high_water(&e, &e.mb).await, v2);
    })
    .await;
}

#[tokio::test]
async fn folder_uid_validity_strictly_increases() {
    for_each_env(|e| async move {
        let start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut prev = high_water(&e, &e.mb).await;
        for i in 0..20 {
            let f = e.mb.create_folder(None, &format!("f{i}")).await.unwrap();
            let v = folder_uidv(&e, &e.mb, f).await;
            assert!(v > prev, "{i}: {v} must be > {prev}");
            assert!(
                v >= start,
                "uses the clock when it is ahead of the high-water mark"
            );
            assert!(v <= i64::from(u32::MAX));
            assert_eq!(high_water(&e, &e.mb).await, v);
            prev = v;
        }
    })
    .await;
}

#[tokio::test]
async fn uid_validity_high_water_mark_persists() {
    for_each_env(|e| async move {
        let f = e.mb.create_folder(None, "a").await.unwrap();
        let v1 = folder_uidv(&e, &e.mb, f).await;
        let reopened = e.reopen(e.account).await.unwrap();
        let g = reopened.create_folder(None, "b").await.unwrap();
        let v2 = folder_uidv(&e, &reopened, g).await;
        assert!(v2 > v1, "{v2} must be > {v1}");
        assert_eq!(high_water(&e, &reopened).await, v2);
    })
    .await;
}

#[tokio::test]
async fn uid_validity_over_u32_max_is_quota() {
    for_each_env(|e| async move {
        let max = i64::from(u32::MAX);
        e.exec(
            "UPDATE mailboxes SET uid_validity = ? WHERE id = ?",
            &[Bind::I(max - 1), Bind::I(e.mb.id().0)],
        )
        .await;
        // the last representable value is still issued
        let f = e.mb.create_folder(None, "last").await.unwrap();
        assert_eq!(folder_uidv(&e, &e.mb, f).await, max);
        assert_eq!(high_water(&e, &e.mb).await, max);

        let folders = e.scalar("SELECT COUNT(*) FROM folders").await;
        let changes = e.mb.changes_since(0, MAX_PAGE).await.unwrap().changes.len();
        let r = e.mb.create_folder(None, "one-too-many").await;
        assert!(matches!(r, Err(Error::Quota(_))), "{r:?}");
        assert_eq!(e.scalar("SELECT COUNT(*) FROM folders").await, folders);
        assert_eq!(
            high_water(&e, &e.mb).await,
            max,
            "high-water mark unchanged"
        );
        assert_eq!(
            e.mb.changes_since(0, MAX_PAGE).await.unwrap().changes.len(),
            changes
        );
    })
    .await;
}

// ---------- isolation ----------

#[tokio::test]
async fn cross_mailbox_ids_are_not_found() {
    for_each_env(|e| async move {
        let bob = e.other("bob").await;
        let bob_folder = bob.create_folder(None, "BobOnly").await.unwrap();
        for i in 0..3 {
            bob.append(INBOX, raw(&format!("b{i}"), None), Flags::empty())
                .await
                .unwrap();
        }
        e.mb.append(INBOX, raw("a", None), Flags::empty())
            .await
            .unwrap();

        let foreign_msg = MessageId(3); // exists only in bob's mailbox
        let nf = |r: rusty_core::Result<()>| assert!(matches!(r, Err(Error::NotFound(_))), "{r:?}");
        nf(e.mb.set_flags(foreign_msg, Flags::SEEN).await);
        nf(e.mb.add_keyword(foreign_msg, "x").await);
        nf(e.mb.remove_keyword(foreign_msg, "x").await);
        nf(e.mb.expunge(foreign_msg).await);
        nf(e.mb.move_message(foreign_msg, INBOX).await.map(|_| ()));
        nf(e.mb
            .move_message(MessageId(1), bob_folder)
            .await
            .map(|_| ()));
        nf(e.mb.rename_folder(bob_folder, "x").await);
        nf(e.mb.move_folder(bob_folder, None).await);
        nf(e.mb.delete_folder(bob_folder).await);
        nf(e.mb.create_folder(Some(bob_folder), "x").await.map(|_| ()));
        nf(e.mb.list_folder(bob_folder, 0, MAX_PAGE).await.map(|_| ()));

        // bob's data is untouched; alice's change log is separate
        let bl = bob.list_folder(INBOX, 0, MAX_PAGE).await.unwrap();
        assert_eq!(bl.len(), 3);
        assert!(bl.iter().all(|m| m.flags == Flags::empty()));
        assert_eq!(
            e.mb.changes_since(0, MAX_PAGE).await.unwrap().changes.len(),
            1
        );
        assert_eq!(
            bob.changes_since(0, MAX_PAGE).await.unwrap().changes.len(),
            4
        );
    })
    .await;
}

// ---------- concurrency / cancellation ----------

/// Run `fut` for exactly `n` polls and then drop it, wherever it is: the cancellation point is
/// picked by poll count, not by time, so it is the same on every machine. The runtime keeps
/// driving the future (wake-ups reach the outer task), which a hand-rolled "poll, sleep 1 ms,
/// poll" loop does not guarantee: with sqlx 0.9 on a multi-connection pool that loop never got
/// an append to the end, so the "some appends completed" check below could not hold.
/// Returns true if `fut` completed within the `n` polls.
async fn poll_n_then_drop<F: std::future::Future>(fut: F, n: usize) -> bool {
    struct StopAfter<F> {
        inner: std::pin::Pin<Box<F>>,
        left: usize,
    }
    impl<F: std::future::Future> std::future::Future for StopAfter<F> {
        type Output = bool;
        fn poll(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<bool> {
            self.left = self.left.saturating_sub(1);
            match self.inner.as_mut().poll(cx) {
                std::task::Poll::Ready(_) => std::task::Poll::Ready(true),
                std::task::Poll::Pending if self.left == 0 => std::task::Poll::Ready(false),
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        }
    }
    StopAfter { inner: Box::pin(fut), left: n }.await
}

/// Drop appends at every await point (pool acquire, blob write, BEGIN, each statement,
/// COMMIT) and with short timeouts; afterwards a normal append must succeed and whatever
/// committed must be gapless.
async fn cancel_appends_then_check(e: &Env) {
    // Cancel after n polls for n = 1, 2, 3, ... until one append runs to completion: every await point is hit, the
    // last one included, however many polls an append needs on this machine and under this load (a fixed range
    // such as 1..=60 failed once on a loaded run).
    let mut n = 0usize;
    loop {
        n += 1;
        assert!(n <= 10_000, "an append never completed within {n} polls");
        let fut =
            e.mb.append(INBOX, raw(&format!("c{n}"), None), Flags::empty());
        let completed = poll_n_then_drop(fut, n).await;
        // sqlx 0.9 recycles a connection that was dropped mid-statement in a background task, and an
        // `acquire` that finds the pool at its limit meanwhile retries in a yield loop (sqlx-core
        // pool/inner.rs, "woke but was unable to acquire idle connection"). That loop burns polls,
        // so the next cancelled append would never get to run to completion. A normal append here
        // lets the pool settle, and proves that it is usable after a cancellation at every point.
        let settle = tokio::time::timeout(
            Duration::from_secs(10),
            e.mb.append(INBOX, raw(&format!("s{n}"), None), Flags::empty()),
        )
        .await;
        assert!(matches!(settle, Ok(Ok(_))), "n={n}: {settle:?}");
        if completed {
            break;
        }
    }
    for k in 0..200u64 {
        let fut =
            e.mb.append(INBOX, raw(&format!("t{k}"), None), Flags::empty());
        let _ = tokio::time::timeout(Duration::from_micros((k % 50) * 20), fut).await;
    }
    let r = tokio::time::timeout(
        Duration::from_secs(10),
        e.mb.append(INBOX, raw("after", None), Flags::empty()),
    )
    .await;
    assert!(matches!(r, Ok(Ok(_))), "{r:?}");

    let n = e.scalar("SELECT COUNT(*) FROM messages").await;
    assert_eq!(e.scalar("SELECT MAX(id) FROM messages").await, n);
    assert_eq!(e.scalar("SELECT next_msg_id FROM mailboxes").await, n + 1);
    assert_eq!(e.scalar("SELECT MAX(uid) FROM messages").await, n);
    assert_eq!(e.scalar("SELECT COUNT(*) FROM change_log").await, n);
    assert_eq!(e.scalar("SELECT MAX(seq) FROM change_log").await, n);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_append_does_not_hold_write_lock() {
    for_each_env(|e| async move { cancel_appends_then_check(&e).await }).await;
}
