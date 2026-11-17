//! The usage of a mailbox and its quota (migration 0032, docs/design-plans-quota.md): the usage is kept by the database as messages come and go, a write that would pass the quota is
//! refused in its transaction and leaves nothing behind, and removing messages makes room.

mod common;

use bytes::Bytes;
use common::backends::{acct, pg_backend, scalar, INBOX};
use common::pg::pg_admin_url;
use rusty_core::{Error, Flags};
use rusty_store::MailboxLimits;

fn msg(n: usize) -> Bytes {
    Bytes::from(format!("From: a@x.test\r\nSubject: s\r\n\r\n{}\r\n", "a".repeat(n)))
}

#[tokio::test]
async fn usage_follows_the_messages_and_a_write_past_the_quota_is_refused_and_removing_makes_room() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let (_, _, mb) = acct(&b, "quota").await;
    let size = i64::try_from(msg(1000).len()).unwrap();
    let mb = mb.with_limits(MailboxLimits { quota_bytes: Some(u64::try_from(size * 2 + size / 2).unwrap()), ..MailboxLimits::default() });
    let a = mb.append(INBOX, msg(1000), Flags::empty()).await.unwrap();
    let c = mb.append(INBOX, msg(1000), Flags::empty()).await.unwrap();
    assert_eq!(scalar(&b, "SELECT used_bytes FROM mailboxes").await, size * 2, "the usage is the sum of the sizes");
    // The third does not fit: refused, and the usage and the message count are what they were.
    let over = mb.append(INBOX, msg(1000), Flags::empty()).await;
    assert!(matches!(over, Err(Error::Quota(_))), "{over:?}");
    assert_eq!(scalar(&b, "SELECT used_bytes FROM mailboxes").await, size * 2);
    assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, 2);
    // A small one still fits in the room that is left.
    mb.append(INBOX, msg(10), Flags::empty()).await.unwrap();
    // Removing a message frees its bytes.
    mb.expunge(a.id).await.unwrap();
    mb.expunge(c.id).await.unwrap();
    assert_eq!(scalar(&b, "SELECT used_bytes FROM mailboxes").await, scalar(&b, "SELECT CAST(COALESCE(sum(size), 0) AS BIGINT) FROM messages WHERE deleted_at IS NULL").await, "the counter equals the rows");
    mb.append(INBOX, msg(1000), Flags::empty()).await.unwrap();
    // No quota: nothing is refused.
    let (_, _, free) = acct(&b, "free").await;
    free.append(INBOX, msg(5000), Flags::empty()).await.unwrap();
}
