//! U6.0: the store operations the IMAP server needs and the traits lacked, on BOTH backends
//! (one body per behaviour, run through `for_each_backend`): UIDVALIDITY / UIDNEXT on
//! `FolderInfo`, per-folder counts, folder subscriptions, message copy and raw-message reads.

mod common;

use bytes::Bytes;
use common::backends::*;
use common::raw;
use rusty_core::{DeliveryId, Error, Flags, FolderId, MessageId};
use rusty_store::{ChangeItem, ChangeOp, DeliverOutcome};

const DRAFTS: FolderId = FolderId(3);

fn did(n: u32) -> DeliveryId {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(&n.to_be_bytes());
    DeliveryId(b)
}

async fn put(mb: &rusty_store::Mailbox, folder: FolderId, tag: &str, flags: Flags) -> MessageId {
    mb.append(folder, raw(tag, Some(&format!("{tag}@t.test"))), flags).await.unwrap().id
}

#[tokio::test]
async fn folder_info_surfaces_uid_validity_uid_next_and_subscription() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "fi").await;
        let folders = mb.list_folders().await.unwrap();
        assert_eq!(folders.len(), 5, "{n}");
        let mut seen = std::collections::HashSet::new();
        for f in &folders {
            assert!(f.uid_validity > 0, "{n}: {f:?}");
            assert!(seen.insert(f.uid_validity), "{n}: uid_validity is distinct per folder");
            assert_eq!(f.uid_next, 1, "{n}: {f:?}");
            assert!(f.subscribed, "{n}: system folders start subscribed: {f:?}");
        }
        put(&mb, INBOX, "a", Flags::empty()).await;
        put(&mb, INBOX, "b", Flags::empty()).await;
        let inbox = mb.list_folders().await.unwrap().into_iter().find(|f| f.id == INBOX).unwrap();
        assert_eq!(inbox.uid_next, 3, "{n}");
        // A new folder gets a UIDVALIDITY nobody else has, and is not subscribed.
        let f = mb.create_folder(None, "Projects").await.unwrap();
        let all = mb.list_folders().await.unwrap();
        let p = all.iter().find(|x| x.id == f).unwrap();
        assert!(!p.subscribed, "{n}");
        assert!(all.iter().filter(|x| x.uid_validity == p.uid_validity).count() == 1, "{n}");
    })
    .await;
}

#[tokio::test]
async fn folder_counts_count_live_and_unseen_messages_of_this_folder_only() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "fc").await;
        let c = mb.folder_counts(INBOX).await.unwrap();
        assert_eq!((c.messages, c.unseen), (0, 0), "{n}");
        let m1 = put(&mb, INBOX, "a", Flags::empty()).await;
        put(&mb, INBOX, "b", Flags::SEEN).await;
        put(&mb, INBOX, "c", Flags::SEEN | Flags::FLAGGED).await;
        put(&mb, DRAFTS, "d", Flags::empty()).await;
        let c = mb.folder_counts(INBOX).await.unwrap();
        assert_eq!((c.messages, c.unseen), (3, 1), "{n}");
        mb.expunge(m1).await.unwrap();
        let c = mb.folder_counts(INBOX).await.unwrap();
        assert_eq!((c.messages, c.unseen), (2, 0), "{n}: expunged messages do not count");
        assert!(matches!(mb.folder_counts(FolderId(99)).await, Err(Error::NotFound(_))), "{n}");
    })
    .await;
}

#[tokio::test]
async fn folder_counts_never_see_another_mailbox() {
    for_each_backend(|b| async move {
        let n = b.name;
        let dom = b.dir.create_domain("fx.test").await.unwrap();
        let mut mbs = vec![];
        for who in ["alice", "bob"] {
            let acc = b.dir.create_account(who, dom, None, None).await.unwrap();
            mbs.push(
                rusty_store::Mailbox::open_with(b.mrepo.clone(), b.blobs.clone(), acc).await.unwrap(),
            );
        }
        let f = mbs[0].create_folder(None, "Private").await.unwrap();
        put(&mbs[0], f, "secret", Flags::empty()).await;
        // Folder id 6 exists in alice's mailbox only.
        assert!(matches!(mbs[1].folder_counts(f).await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(mbs[1].set_subscribed(f, true).await, Err(Error::NotFound(_))), "{n}");
    })
    .await;
}

#[tokio::test]
async fn subscriptions_toggle_idempotently_and_follow_the_folder() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "sub").await;
        let f = mb.create_folder(None, "Projects").await.unwrap();
        let sub = |mb: rusty_store::Mailbox, id: FolderId| async move {
            mb.list_folders().await.unwrap().into_iter().find(|x| x.id == id).unwrap().subscribed
        };
        assert!(!sub(mb.clone(), f).await, "{n}");
        mb.set_subscribed(f, true).await.unwrap();
        mb.set_subscribed(f, true).await.unwrap();
        assert!(sub(mb.clone(), f).await, "{n}");
        mb.set_subscribed(f, false).await.unwrap();
        mb.set_subscribed(f, false).await.unwrap();
        assert!(!sub(mb.clone(), f).await, "{n}");
        // System folders can be unsubscribed too (a client's choice) and resubscribed.
        mb.set_subscribed(FolderId(5), false).await.unwrap();
        assert!(!sub(mb.clone(), FolderId(5)).await, "{n}");
        assert!(matches!(mb.set_subscribed(FolderId(99), true).await, Err(Error::NotFound(_))), "{n}");
        // Subscription state is not a message change, but it is a folder update: other sessions
        // can notice it through the change log.
        let page = mb.changes_since(0, 1000).await.unwrap();
        assert!(
            page.changes.iter().any(|c| c.item == ChangeItem::Folder(f) && c.op == ChangeOp::Update),
            "{n}: {page:?}"
        );
    })
    .await;
}

#[tokio::test]
async fn copy_duplicates_message_rows_sharing_the_blob() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "cp").await;
        let a = put(&mb, INBOX, "one", Flags::SEEN | Flags::FLAGGED).await;
        let c = put(&mb, INBOX, "two", Flags::empty()).await;
        mb.add_keyword(a, "$Label1").await.unwrap();
        let dst = mb.create_folder(None, "Archive2").await.unwrap();
        let before = mb.get_message(a).await.unwrap();
        let out = mb.copy_many(&[a, c], dst).await.unwrap();
        assert!(out.missing.is_empty(), "{n}");
        assert_eq!(out.copied.len(), 2, "{n}");
        let (src0, r0) = out.copied[0];
        let (src1, r1) = out.copied[1];
        assert_eq!((src0, src1), (a, c), "{n}: input order");
        assert_eq!((r0.uid, r1.uid), (1, 2), "{n}: consecutive uids in the destination");
        assert!(r0.id != a && r1.id != c && r0.id != r1.id, "{n}: new message ids");
        // The copy carries flags, keywords, internal date, and the same blob.
        let copy = mb.get_message(r0.id).await.unwrap();
        assert_eq!(copy.folder, dst, "{n}");
        assert_eq!(copy.blob, before.blob, "{n}: blob is shared, not rewritten");
        assert_eq!(copy.summary.flags, Flags::SEEN | Flags::FLAGGED, "{n}");
        assert_eq!(copy.summary.internal_date, before.summary.internal_date, "{n}");
        assert_eq!(copy.summary.subject, before.summary.subject, "{n}");
        assert_eq!(copy.summary.thread_id, before.summary.thread_id, "{n}");
        assert_eq!(mb.keywords(r0.id).await.unwrap(), vec!["$label1".to_owned()], "{n}");
        // The source is untouched and still live.
        let src = mb.get_message(a).await.unwrap();
        assert!(!src.deleted && src.folder == INBOX && src.summary.uid == before.summary.uid, "{n}");
        assert_eq!(mb.list_folder(INBOX, 0, 10).await.unwrap().len(), 2, "{n}");
        assert_eq!(mb.list_folder(dst, 0, 10).await.unwrap().len(), 2, "{n}");
        // Two rows reference the blob.
        let refs = fetch_i64s(&b, "SELECT refcount FROM blobs ORDER BY refcount", &[]).await;
        assert_eq!(refs, vec![2, 2], "{n}: each of the two blobs is referenced by source and copy");
        // One create entry per copy in the change log.
        let page = mb.changes_since(0, 1000).await.unwrap();
        for r in [r0, r1] {
            assert!(
                page.changes.iter().any(|c| c.item == ChangeItem::Message(r.id) && c.op == ChangeOp::Create),
                "{n}"
            );
        }
        // The bytes are readable through the facade.
        let bytes = mb.get_raw(r1.id).await.unwrap();
        assert_eq!(bytes, raw("two", Some("two@t.test")), "{n}");
    })
    .await;
}

#[tokio::test]
async fn copy_into_the_same_folder_adds_a_second_message() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "cs").await;
        let a = put(&mb, INBOX, "one", Flags::empty()).await;
        let out = mb.copy_many(&[a], INBOX).await.unwrap();
        assert_eq!(out.copied[0].1.uid, 2, "{n}");
        assert_eq!(mb.list_folder(INBOX, 0, 10).await.unwrap().len(), 2, "{n}");
    })
    .await;
}

#[tokio::test]
async fn copy_reports_missing_and_rejects_bad_input_without_side_effects() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "cm").await;
        let a = put(&mb, INBOX, "one", Flags::empty()).await;
        let gone = put(&mb, INBOX, "gone", Flags::empty()).await;
        mb.expunge(gone).await.unwrap();
        let dst = mb.create_folder(None, "D").await.unwrap();
        let out = mb.copy_many(&[gone, a, MessageId(9999)], dst).await.unwrap();
        assert_eq!(out.copied.len(), 1, "{n}");
        assert_eq!(out.missing, vec![gone, MessageId(9999)], "{n}");
        // Duplicate ids in the input copy once.
        let out = mb.copy_many(&[a, a], dst).await.unwrap();
        assert_eq!(out.copied.len(), 1, "{n}");
        // Unknown destination: NotFound even when every id is missing; nothing written.
        let seq_before = mb.changes_since(0, 1000).await.unwrap().changes.len();
        assert!(matches!(mb.copy_many(&[a], FolderId(99)).await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(mb.copy_many(&[MessageId(9999)], FolderId(99)).await, Err(Error::NotFound(_))), "{n}");
        assert_eq!(mb.changes_since(0, 1000).await.unwrap().changes.len(), seq_before, "{n}");
        // Empty and oversized batches are Invalid.
        assert!(matches!(mb.copy_many(&[], dst).await, Err(Error::Invalid(_))), "{n}");
        let many: Vec<MessageId> = (1..=1001).map(MessageId).collect();
        assert!(matches!(mb.copy_many(&many, dst).await, Err(Error::Invalid(_))), "{n}");
    })
    .await;
}

#[tokio::test]
async fn copy_cannot_reach_another_mailbox() {
    for_each_backend(|b| async move {
        let n = b.name;
        let dom = b.dir.create_domain("cx.test").await.unwrap();
        let mut mbs = vec![];
        for who in ["alice", "bob"] {
            let acc = b.dir.create_account(who, dom, None, None).await.unwrap();
            mbs.push(
                rusty_store::Mailbox::open_with(b.mrepo.clone(), b.blobs.clone(), acc).await.unwrap(),
            );
        }
        let secret = put(&mbs[0], INBOX, "secret", Flags::empty()).await;
        let out = mbs[1].copy_many(&[secret], INBOX).await.unwrap();
        assert!(out.copied.is_empty() && out.missing == vec![secret], "{n}");
        assert!(mbs[1].list_folder(INBOX, 0, 10).await.unwrap().is_empty(), "{n}");
        assert!(matches!(mbs[1].get_raw(secret).await, Err(Error::NotFound(_))), "{n}");
        assert!(matches!(mbs[0].get_raw(MessageId(9999)).await, Err(Error::NotFound(_))), "{n}");
    })
    .await;
}

#[tokio::test]
async fn copy_is_not_a_delivery_and_leaves_delivery_ids_alone() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "cd").await;
        let DeliverOutcome::Created(r) =
            mb.deliver(INBOX, raw("d", Some("d@t.test")), Flags::empty(), did(1)).await.unwrap()
        else {
            panic!("created")
        };
        // A copy is a new message, not a delivery: redelivering the original id is still a Duplicate
        // of the ORIGINAL, never of the copy.
        let out = mb.copy_many(&[r.id], DRAFTS).await.unwrap();
        let again = mb.deliver(INBOX, raw("d", Some("d@t.test")), Flags::empty(), did(1)).await.unwrap();
        assert_eq!(again, DeliverOutcome::Duplicate(r), "{n}");
        assert!(out.copied[0].1.id != r.id, "{n}");
        let _: Bytes = mb.get_raw(r.id).await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn latest_change_seq_is_the_last_logged_seq_of_this_mailbox() {
    for_each_backend(|b| async move {
        let n = b.name;
        let dom = b.dir.create_domain("cs.test").await.unwrap();
        let mut mbs = vec![];
        for who in ["alice", "bob"] {
            let acc = b.dir.create_account(who, dom, None, None).await.unwrap();
            mbs.push(
                rusty_store::Mailbox::open_with(b.mrepo.clone(), b.blobs.clone(), acc).await.unwrap(),
            );
        }
        assert_eq!(mbs[0].latest_change_seq().await.unwrap(), 0, "{n}: nothing logged yet");
        let a = put(&mbs[0], INBOX, "a", Flags::empty()).await;
        mbs[0].set_flags(a, Flags::SEEN).await.unwrap();
        let last = mbs[0].changes_since(0, 1000).await.unwrap().changes.last().unwrap().seq;
        assert_eq!(mbs[0].latest_change_seq().await.unwrap(), last, "{n}");
        assert_eq!(mbs[1].latest_change_seq().await.unwrap(), 0, "{n}: another mailbox is separate");
        put(&mbs[0], INBOX, "b", Flags::empty()).await;
        assert_eq!(mbs[0].latest_change_seq().await.unwrap(), last + 1, "{n}");
    })
    .await;
}

#[tokio::test]
async fn keywords_many_returns_the_keywords_of_live_messages_in_one_call() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "km").await;
        let a = put(&mb, INBOX, "a", Flags::empty()).await;
        let c = put(&mb, INBOX, "c", Flags::empty()).await;
        let plain = put(&mb, INBOX, "p", Flags::empty()).await;
        let gone = put(&mb, INBOX, "g", Flags::empty()).await;
        mb.add_keyword(a, "$label1").await.unwrap();
        mb.add_keyword(a, "work").await.unwrap();
        mb.add_keyword(c, "work").await.unwrap();
        mb.add_keyword(gone, "work").await.unwrap();
        mb.expunge(gone).await.unwrap();
        let m = mb.keywords_many(&[a, c, plain, gone, MessageId(9999), a]).await.unwrap();
        assert_eq!(m.get(&a), Some(&vec!["$label1".to_owned(), "work".to_owned()]), "{n}");
        assert_eq!(m.get(&c), Some(&vec!["work".to_owned()]), "{n}");
        assert!(!m.contains_key(&plain) && !m.contains_key(&gone) && !m.contains_key(&MessageId(9999)), "{n}");
        assert!(matches!(mb.keywords_many(&[]).await, Err(Error::Invalid(_))), "{n}");
        let many: Vec<MessageId> = (1..=1001).map(MessageId).collect();
        assert!(matches!(mb.keywords_many(&many).await, Err(Error::Invalid(_))), "{n}");
    })
    .await;
}

#[tokio::test]
async fn keywords_many_never_reads_another_mailbox() {
    for_each_backend(|b| async move {
        let n = b.name;
        let dom = b.dir.create_domain("kx.test").await.unwrap();
        let mut mbs = vec![];
        for who in ["alice", "bob"] {
            let acc = b.dir.create_account(who, dom, None, None).await.unwrap();
            mbs.push(
                rusty_store::Mailbox::open_with(b.mrepo.clone(), b.blobs.clone(), acc).await.unwrap(),
            );
        }
        let secret = put(&mbs[0], INBOX, "s", Flags::empty()).await;
        mbs[0].add_keyword(secret, "private").await.unwrap();
        assert!(mbs[1].keywords_many(&[secret]).await.unwrap().is_empty(), "{n}");
    })
    .await;
}

#[tokio::test]
async fn append_dated_stores_the_clients_internal_date_within_bounds() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "ad").await;
        let r = mb.append_dated(INBOX, raw("old", Some("old@t.test")), Flags::SEEN, 1_262_304_000).await.unwrap();
        let d = mb.get_message(r.id).await.unwrap();
        assert_eq!(d.summary.internal_date, 1_262_304_000, "{n}");
        assert_eq!(d.summary.flags, Flags::SEEN, "{n}");
        let plain = mb.append(INBOX, raw("new", Some("new@t.test")), Flags::empty()).await.unwrap();
        assert!(mb.get_message(plain.id).await.unwrap().summary.internal_date > 1_700_000_000, "{n}: undated appends use the database clock");
        for bad in [-1, rusty_store::MAX_SUPPLIED_TIME + 1] {
            assert!(matches!(mb.append_dated(INBOX, raw("x", None), Flags::empty(), bad).await, Err(Error::Invalid(_))), "{n}: {bad}");
        }
        assert_eq!(mb.list_folder(INBOX, 0, 10).await.unwrap().len(), 2, "{n}: rejected dates stored nothing");
    })
    .await;
}

#[tokio::test]
async fn flag_deltas_do_not_clobber_what_another_session_changed() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "fd").await;
        let a = put(&mb, INBOX, "a", Flags::SEEN).await;
        let c = put(&mb, INBOX, "c", Flags::empty()).await;
        // Another session flagged the first message after our snapshot: +\Answered must keep it.
        mb.set_flags(a, Flags::SEEN | Flags::FLAGGED).await.unwrap();
        let out = mb.update_flags_many(&[a, c, MessageId(9999)], Flags::ANSWERED, Flags::empty()).await.unwrap();
        assert_eq!(out.updated, vec![(a, Flags::SEEN | Flags::FLAGGED | Flags::ANSWERED), (c, Flags::ANSWERED)], "{n}");
        assert_eq!(out.missing, vec![MessageId(9999)], "{n}");
        // Removal is a delta too, and removing what is not there is fine.
        let out = mb.update_flags_many(&[a], Flags::DRAFT, Flags::SEEN | Flags::DELETED).await.unwrap();
        assert_eq!(out.updated, vec![(a, Flags::FLAGGED | Flags::ANSWERED | Flags::DRAFT)], "{n}");
        assert_eq!(mb.get_message(a).await.unwrap().summary.flags, Flags::FLAGGED | Flags::ANSWERED | Flags::DRAFT, "{n}");
        // Expunged messages are missing; empty/oversized batches are Invalid.
        mb.expunge(c).await.unwrap();
        assert_eq!(mb.update_flags_many(&[c], Flags::SEEN, Flags::empty()).await.unwrap().missing, vec![c], "{n}");
        assert!(matches!(mb.update_flags_many(&[], Flags::SEEN, Flags::empty()).await, Err(Error::Invalid(_))), "{n}");
        // The change log records it.
        assert!(mb.changes_since(0, 1000).await.unwrap().changes.iter().any(|x| x.item == ChangeItem::Message(a) && x.op == ChangeOp::Update), "{n}");
    })
    .await;
}

#[tokio::test]
async fn expunge_deleted_only_takes_messages_that_are_still_deleted() {
    for_each_backend(|b| async move {
        let n = b.name;
        let (_, _, mb) = acct(&b, "ed").await;
        let keep = put(&mb, INBOX, "keep", Flags::DELETED).await;
        let go = put(&mb, INBOX, "go", Flags::DELETED).await;
        // Another session cleared \Deleted on `keep` after our snapshot said it was deleted.
        mb.set_flags(keep, Flags::empty()).await.unwrap();
        let out = mb.expunge_deleted_many(&[keep, go, MessageId(9999)]).await.unwrap();
        assert_eq!(out.applied, 1, "{n}");
        assert_eq!(out.missing, vec![keep, MessageId(9999)], "{n}");
        assert!(!mb.get_message(keep).await.unwrap().deleted, "{n}: the un-deleted message survives");
        assert!(mb.get_message(go).await.unwrap().deleted, "{n}");
    })
    .await;
}
