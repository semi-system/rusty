//! Deleting (docs/design-delete.md): a closed account is removed with its mailbox after the retention, its messages in batches and the account last; an account that is not closed is never
//! touched; a domain is deleted only when it holds nothing.

mod common;

use bytes::Bytes;
use common::backends::{acct, pg_backend, scalar, INBOX};
use common::pg::pg_admin_url;
use rusty_core::{Error, Flags};
use rusty_store::repo::AccountStatus;
use rusty_store::Gc;

fn msg(i: usize) -> Bytes {
    Bytes::from(format!("From: a@x.test\r\nSubject: m{i}\r\n\r\nbody {i}\r\n"))
}

#[tokio::test]
async fn a_closed_account_is_removed_with_its_mail_after_its_retention_and_an_open_one_never() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let (_, acc, mb) = acct(&b, "gone").await;
    for i in 0..5 {
        mb.append(INBOX, msg(i), Flags::empty()).await.unwrap();
    }
    let gc = Gc::new(b.grepo.clone());
    // Open: never due, whatever the retention.
    assert_eq!(gc.purge_closed_accounts(i64::MAX, 10).await.unwrap(), (0, 0));
    b.drepo.set_status(acc, AccountStatus::Disabled).await.unwrap();
    let closed: i64 = scalar(&b, "SELECT closed_at FROM accounts").await;
    assert!(closed > 0, "closing sets the time");
    // Closed but inside the retention: kept.
    assert_eq!(gc.purge_closed_accounts(closed, 10).await.unwrap(), (0, 0), "not yet due: closed_at is not before the cut-off");
    assert_eq!(scalar(&b, "SELECT count(*) FROM accounts").await, 1);
    // Reopened: the time is cleared and it is not due even when the cut-off is far in the future.
    b.drepo.set_status(acc, AccountStatus::Active).await.unwrap();
    assert_eq!(gc.purge_closed_accounts(i64::MAX, 10).await.unwrap(), (0, 0));
    b.drepo.set_status(acc, AccountStatus::Disabled).await.unwrap();
    // "Delete now" makes it due at once.
    assert!(b.drepo.request_account_delete(acc).await.unwrap());
    assert_eq!(scalar(&b, "SELECT closed_at FROM accounts").await, 0);
    // Batches of PURGE_BATCH messages: five are fewer than one batch, so one pass removes the messages and the account.
    let (messages, accounts) = gc.purge_closed_accounts(1, 10).await.unwrap();
    assert_eq!((messages, accounts), (5, 1));
    assert_eq!(scalar(&b, "SELECT count(*) FROM accounts").await, 0);
    assert_eq!(scalar(&b, "SELECT count(*) FROM mailboxes").await, 0);
    assert_eq!(scalar(&b, "SELECT count(*) FROM messages").await, 0);
    assert_eq!(scalar(&b, "SELECT count(*) FROM address_owners").await, 0, "the address is free again");
}

#[tokio::test]
async fn only_a_closed_account_can_be_asked_for_deletion() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let (_, acc, _) = acct(&b, "open").await;
    assert!(!b.drepo.request_account_delete(acc).await.unwrap(), "an open account is not deleted");
    b.drepo.set_status(acc, AccountStatus::Locked).await.unwrap();
    assert!(!b.drepo.request_account_delete(acc).await.unwrap(), "a locked one is not closed either");
}

#[tokio::test]
async fn a_domain_is_deleted_only_when_it_holds_nothing() {
    if pg_admin_url().is_none() {
        return;
    }
    let b = pg_backend().await.unwrap();
    let (dom, acc, _) = acct(&b, "full").await;
    let held = b.drepo.delete_domain(dom).await;
    assert!(matches!(held, Err(Error::Conflict(_))), "{held:?}");
    b.drepo.set_status(acc, AccountStatus::Disabled).await.unwrap();
    b.drepo.request_account_delete(acc).await.unwrap();
    Gc::new(b.grepo.clone()).purge_closed_accounts(1, 10).await.unwrap();
    b.drepo.delete_domain(dom).await.expect("an empty domain goes");
    assert_eq!(scalar(&b, "SELECT count(*) FROM domains").await, 0);
    assert!(matches!(b.drepo.delete_domain(dom).await, Err(Error::NotFound(_))));
}
