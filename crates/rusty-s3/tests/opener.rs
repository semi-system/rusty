//! `S3StoreOpener`: the store of a `blob_stores` row is opened from its settings and its sealed credentials, and from nothing else. No network is touched: a client is built, not used.

use std::sync::Arc;

use rusty_core::storage_policy::{validate_tenant_s3, ACCESS_KEY_PURPOSE, SECRET_KEY_PURPOSE, STORE_SECRET_OWNER};
use rusty_core::Error;
use rusty_s3::S3StoreOpener;
use rusty_store::fixtures::pg_db::{pg_admin_url, PgTestDb};
use rusty_store::repo::BlobStoreRow;
use rusty_store::{BlobStoreAdmin, SealedReader, SealedWriter, Store, StoreConfig, StoreOpener};

fn tls() -> Arc<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap().with_root_certificates(rustls::RootCertStore::empty()).with_no_client_auth())
}

struct Rig {
    store: Store,
    row: BlobStoreRow,
    opener: S3StoreOpener,
    _db: PgTestDb,
}

async fn rig() -> Option<Rig> {
    let url = pg_admin_url()?;
    let db = PgTestDb::create(&url).await;
    let store = Store::open(StoreConfig::postgres(db.url())).await.unwrap();
    let domain = rusty_directory::Directory::with_repo(store.repos().directory.clone()).create_domain("tenant.test").await.unwrap();
    let admin = BlobStoreAdmin::with_repo(store.repos().blob_stores.clone());
    let s3 = validate_tenant_s3("https://s3.example-cloud.com", "eu-west-3", "tenant-mail", "mail/a", true).unwrap();
    let (row, _) = admin.set_s3(domain, &s3).await.unwrap();
    let (unseal, seal) = rusty_seal::generate();
    let writer = SealedWriter::with_repo(store.repos().sealed.clone());
    for (purpose, plain) in [(ACCESS_KEY_PURPOSE, "AKIAEXAMPLEKEY"), (SECRET_KEY_PURPOSE, "super-secret-key-material")] {
        let ctx = rusty_seal::Context::new(STORE_SECRET_OWNER, row.id.0, purpose).unwrap();
        writer.put(STORE_SECRET_OWNER, row.id.0, purpose, &seal.id().to_hex(), &seal.seal(&ctx, plain.as_bytes()).unwrap()).await.unwrap();
    }
    let opener = S3StoreOpener::new(SealedReader::with_repo(store.repos().sealed.clone()), Arc::new(rusty_seal::Unsealer::new(vec![unseal]).unwrap()), tls());
    Some(Rig { store, row, opener, _db: db })
}

#[tokio::test]
async fn a_store_is_opened_from_its_settings_and_its_sealed_keys() {
    let Some(r) = rig().await else { return };
    let client = r.opener.client(&r.row).await.unwrap();
    let c = client.config();
    assert_eq!((c.endpoint.as_str(), c.region.as_str(), c.bucket.as_str(), c.prefix.as_str(), c.path_style), ("https://s3.example-cloud.com", "eu-west-3", "tenant-mail", "mail/a", true));
    assert!(!format!("{client:?}").contains("super-secret"), "the secret key is never printed");
    assert!(r.opener.open(&r.row).await.is_ok());
}

#[tokio::test]
async fn everything_that_is_not_a_well_formed_tenant_store_with_credentials_is_refused() {
    let Some(r) = rig().await else { return };
    let refused = |row: BlobStoreRow| {
        let opener = &r.opener;
        async move { matches!(opener.client(&row).await, Err(Error::Invalid(_))) }
    };
    // Settings that were changed behind the services' back are checked again at every open.
    for config in [
        r#"{"endpoint":"http://s3.example-cloud.com","region":"r","bucket":"tenant-mail"}"#,
        r#"{"endpoint":"https://127.0.0.1","region":"r","bucket":"tenant-mail"}"#,
        r#"{"endpoint":"https://169.254.169.254","region":"r","bucket":"tenant-mail"}"#,
        r#"{"endpoint":"https://s3.example-cloud.com","region":"r","bucket":"UPPER"}"#,
        r#"{"endpoint":"https://s3.example-cloud.com","region":"r","bucket":"tenant-mail","prefix":"../x"}"#,
        r#"{"endpoint":"https://s3.example-cloud.com","region":"r","bucket":"tenant-mail","unknown":1}"#,
        r#"{"endpoint":"https://s3.example-cloud.com"}"#,
        "not json",
    ] {
        let mut row = r.row.clone();
        row.config = config.into();
        assert!(refused(row).await, "{config}");
    }
    // Another kind is not this opener's.
    let mut fs = r.row.clone();
    fs.kind = "fs".into();
    assert!(refused(fs).await);
    // A store without credentials (or with one of the two) is never opened without them.
    let writer = SealedWriter::with_repo(r.store.repos().sealed.clone());
    writer.delete(STORE_SECRET_OWNER, r.row.id.0, SECRET_KEY_PURPOSE).await.unwrap();
    assert!(refused(r.row.clone()).await, "no secret key");
    writer.delete(STORE_SECRET_OWNER, r.row.id.0, ACCESS_KEY_PURPOSE).await.unwrap();
    assert!(refused(r.row.clone()).await, "no credentials at all");
}

#[tokio::test]
async fn keys_sealed_to_another_store_or_another_key_do_not_open() {
    let Some(r) = rig().await else { return };
    // The keys of this store, copied to another store id: the seal is bound to the owner, so they cannot be opened there.
    let reader = SealedReader::with_repo(r.store.repos().sealed.clone());
    let writer = SealedWriter::with_repo(r.store.repos().sealed.clone());
    let mut other = r.row.clone();
    other.id = rusty_store::repo::StoreId(r.row.id.0 + 100);
    for purpose in [ACCESS_KEY_PURPOSE, SECRET_KEY_PURPOSE] {
        let s = reader.get(STORE_SECRET_OWNER, r.row.id.0, purpose).await.unwrap().unwrap();
        writer.put(STORE_SECRET_OWNER, other.id.0, purpose, &s.key_id, &s.sealed).await.unwrap();
    }
    assert!(r.opener.client(&other).await.is_err(), "keys moved to another store are refused");
    // A service with a different unsealing key cannot open them at all.
    let (wrong, _) = rusty_seal::generate();
    let stranger = S3StoreOpener::new(reader, Arc::new(rusty_seal::Unsealer::new(vec![wrong]).unwrap()), tls());
    assert!(stranger.client(&r.row).await.is_err());
}
