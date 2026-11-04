use rusty_core::Error;
use rusty_directory::{AccountStatus, Authenticator, Directory, LocalAuthenticator};
mod common;

use common::{for_each_backend, Backend};

async fn setup(b: &Backend) -> (Directory, LocalAuthenticator, rusty_core::DomainId) {
    let d = b.dir.clone();
    let dom = d.create_domain("example.com").await.unwrap();
    (d.clone(), LocalAuthenticator::new(d).await.unwrap(), dom)
}

#[tokio::test]
async fn verify_correct_password() {
    for_each_backend(|b| async move {
    let (d, a, dom) = setup(&b).await;
    let acc = d
        .create_account("alice", dom, Some("s3cret"), None)
        .await
        .unwrap();
    assert_eq!(
        a.verify("Alice@example.com", "s3cret").await.unwrap(),
        Some(acc)
    );
    })
    .await;
}

#[tokio::test]
async fn verify_wrong_password_is_none() {
    for_each_backend(|b| async move {
    let (d, a, dom) = setup(&b).await;
    d.create_account("alice", dom, Some("s3cret"), None)
        .await
        .unwrap();
    assert_eq!(a.verify("alice@example.com", "nope").await.unwrap(), None);
    })
    .await;
}

#[tokio::test]
async fn verify_locked_account_is_none() {
    for_each_backend(|b| async move {
    let (d, a, dom) = setup(&b).await;
    let acc = d
        .create_account("alice", dom, Some("s3cret"), None)
        .await
        .unwrap();
    d.set_status(acc, AccountStatus::Locked).await.unwrap();
    assert_eq!(a.verify("alice@example.com", "s3cret").await.unwrap(), None);
    d.set_status(acc, AccountStatus::Disabled).await.unwrap();
    assert_eq!(a.verify("alice@example.com", "s3cret").await.unwrap(), None);
    d.set_status(acc, AccountStatus::Active).await.unwrap();
    assert_eq!(
        a.verify("alice@example.com", "s3cret").await.unwrap(),
        Some(acc)
    );
    })
    .await;
}

#[tokio::test]
async fn verify_account_without_hash_is_none() {
    for_each_backend(|b| async move {
    let (d, a, dom) = setup(&b).await;
    d.create_account("alice", dom, None, None).await.unwrap();
    assert_eq!(a.verify("alice@example.com", "").await.unwrap(), None);
    assert_eq!(a.verify("alice@example.com", "x").await.unwrap(), None);
    })
    .await;
}

#[tokio::test]
async fn verify_unknown_address_is_none() {
    for_each_backend(|b| async move {
    let (_d, a, _) = setup(&b).await;
    assert_eq!(a.verify("ghost@example.com", "x").await.unwrap(), None);
    assert_eq!(a.verify("garbage", "x").await.unwrap(), None);
    })
    .await;
}

#[tokio::test]
async fn verify_through_alias() {
    for_each_backend(|b| async move {
    let (d, a, dom) = setup(&b).await;
    let acc = d
        .create_account("alice", dom, Some("pw"), None)
        .await
        .unwrap();
    d.add_alias("ally", dom, acc).await.unwrap();
    assert_eq!(a.verify("ally@example.com", "pw").await.unwrap(), Some(acc));
    })
    .await;
}

#[tokio::test]
async fn empty_password_is_invalid() {
    for_each_backend(|b| async move {
    let (d, _a, dom) = setup(&b).await;
    assert!(matches!(
        d.create_account("alice", dom, Some(""), None).await,
        Err(Error::Invalid(_))
    ));
    let acc = d.create_account("bob", dom, None, None).await.unwrap();
    assert!(matches!(
        d.set_password(acc, "").await,
        Err(Error::Invalid(_))
    ));
    d.set_password(acc, "new").await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn overlong_password_rejected() {
    for_each_backend(|b| async move {
    let (d, a, dom) = setup(&b).await;
    let long = "x".repeat(1025);
    assert!(matches!(
        d.create_account("alice", dom, Some(&long), None).await,
        Err(Error::Invalid(_))
    ));
    let ok = "y".repeat(1024);
    let acc = d.create_account("bob", dom, Some(&ok), None).await.unwrap();
    assert!(matches!(
        d.set_password(acc, &long).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(a.verify("bob@example.com", &long).await.unwrap(), None);
    assert_eq!(a.verify("bob@example.com", &ok).await.unwrap(), Some(acc));
    })
    .await;
}
