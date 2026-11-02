use rusty_core::Error;
use rusty_directory::{AccountStatus, Authenticator, Directory, LocalAuthenticator};
use rusty_store::SqliteStore;

async fn setup() -> (Directory, LocalAuthenticator, rusty_core::DomainId) {
    let d = Directory::new(SqliteStore::open("sqlite::memory:").await.unwrap());
    let dom = d.create_domain("example.com").await.unwrap();
    (d.clone(), LocalAuthenticator::new(d), dom)
}

#[tokio::test]
async fn verify_correct_password() {
    let (d, a, dom) = setup().await;
    let acc = d
        .create_account("alice", dom, Some("s3cret"), None)
        .await
        .unwrap();
    assert_eq!(
        a.verify("Alice@example.com", "s3cret").await.unwrap(),
        Some(acc)
    );
}

#[tokio::test]
async fn verify_wrong_password_is_none() {
    let (d, a, dom) = setup().await;
    d.create_account("alice", dom, Some("s3cret"), None)
        .await
        .unwrap();
    assert_eq!(a.verify("alice@example.com", "nope").await.unwrap(), None);
}

#[tokio::test]
async fn verify_locked_account_is_none() {
    let (d, a, dom) = setup().await;
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
}

#[tokio::test]
async fn verify_account_without_hash_is_none() {
    let (d, a, dom) = setup().await;
    d.create_account("alice", dom, None, None).await.unwrap();
    assert_eq!(a.verify("alice@example.com", "").await.unwrap(), None);
    assert_eq!(a.verify("alice@example.com", "x").await.unwrap(), None);
}

#[tokio::test]
async fn verify_unknown_address_is_none() {
    let (_d, a, _) = setup().await;
    assert_eq!(a.verify("ghost@example.com", "x").await.unwrap(), None);
    assert_eq!(a.verify("garbage", "x").await.unwrap(), None);
}

#[tokio::test]
async fn verify_through_alias() {
    let (d, a, dom) = setup().await;
    let acc = d
        .create_account("alice", dom, Some("pw"), None)
        .await
        .unwrap();
    d.add_alias("ally", dom, acc).await.unwrap();
    assert_eq!(a.verify("ally@example.com", "pw").await.unwrap(), Some(acc));
}

#[tokio::test]
async fn empty_password_is_invalid() {
    let (d, _a, dom) = setup().await;
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
}
