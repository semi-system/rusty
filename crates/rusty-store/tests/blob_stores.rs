//! Blob stores per tenant (migration 0023, docs/design-s3-per-tenant.md): a domain with a store of its own keeps its bodies there and nowhere else; two domains never see
//! each other's bodies; garbage collection is per store and never touches another store's rows or objects.
mod common;

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use common::backends::*;
use common::raw;
use rusty_core::{Error, Flags};
use rusty_store::repo::{BlobStoreRepo, StoreId, PLATFORM_STORE};
use rusty_store::{BlobRouter, BlobStores, FsStoreOpener, Gc, Mailbox};

const HOUR: Duration = Duration::from_secs(3600);

#[tokio::test]
async fn a_domain_with_a_store_of_its_own_keeps_its_bodies_there_and_gc_works_per_store() {
    let Some(b) = pg_backend().await else { return };
    let repo = Arc::new(b.pg.as_ref().unwrap().1.clone());
    let tmp = tempfile::tempdir().unwrap();
    let router: Arc<dyn BlobRouter> = Arc::new(BlobStores::new(b.blobs.clone(), repo.clone(), Arc::new(FsStoreOpener::new(tmp.path().join("stores")))));

    let (da, db_) = (b.dir.create_domain("a.test").await.unwrap(), b.dir.create_domain("b.test").await.unwrap());
    let (alice, bob) = (b.dir.create_account("alice", da, None, None).await.unwrap(), b.dir.create_account("bob", db_, None, None).await.unwrap());

    // b.test gets a store of its own before it holds any message; a.test stays on the platform store.
    let own = repo.create_for_domain(db_, "fs", r#"{"dir":"tenant-b"}"#).await.unwrap();
    assert_ne!(own, PLATFORM_STORE);
    let rows = repo.list().await.unwrap();
    assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![PLATFORM_STORE, own]);
    assert_eq!(rows[1].domain, Some(db_));

    let ma = Mailbox::open_with(b.mrepo.clone(), router.clone(), alice).await.unwrap();
    let mb = Mailbox::open_with(b.mrepo.clone(), router.clone(), bob).await.unwrap();
    // The very same bytes to both: one object and one row in each store, never shared.
    let body = raw("same body", Some("<same@x>"));
    let ra = ma.append(INBOX, body.clone(), Flags::default()).await.unwrap();
    let rb = mb.append(INBOX, body.clone(), Flags::default()).await.unwrap();

    let platform = router.store(PLATFORM_STORE).await.unwrap();
    let tenant = router.store(own).await.unwrap();
    assert_eq!(platform.list().await.unwrap().len(), 1, "one object in the platform store");
    assert_eq!(tenant.list().await.unwrap().len(), 1, "one object in the tenant's store");
    let stores: Vec<i64> = sqlx::query_scalar("SELECT store_id FROM blobs ORDER BY store_id").fetch_all(b.fx()).await.unwrap();
    assert_eq!(stores, vec![PLATFORM_STORE.0, own.0], "the same content is two rows, one per store");
    let msg_stores: Vec<i64> = sqlx::query_scalar("SELECT blob_store_id FROM messages ORDER BY blob_store_id").fetch_all(b.fx()).await.unwrap();
    assert_eq!(msg_stores, vec![PLATFORM_STORE.0, own.0]);
    assert_eq!(ma.get_raw(ra.id).await.unwrap(), body);
    assert_eq!(mb.get_raw(rb.id).await.unwrap(), body);
    assert_eq!(ma.get_raw_ref(ra.id).await.unwrap().store, PLATFORM_STORE);
    assert_eq!(mb.get_raw_ref(rb.id).await.unwrap().store, own);

    // Losing an object of one store breaks that tenant's read only.
    let h = mb.get_raw_ref(rb.id).await.unwrap().blob;
    assert!(tenant.delete_if_older(&h, SystemTime::now() + HOUR).await.unwrap());
    assert!(mb.get_raw(rb.id).await.is_err(), "the tenant's body is gone from its store");
    assert_eq!(ma.get_raw(ra.id).await.unwrap(), body, "the other tenant is unaffected");
    // put again (same bytes) restores it: the object is addressed by content inside its own store.
    tenant.put(body.clone()).await.unwrap();
    assert_eq!(mb.get_raw(rb.id).await.unwrap(), body);

    // GC per store. Expunge and purge b's message; only store `own` has anything to collect.
    let gc = Gc::new(b.grepo.clone());
    assert_eq!(gc.store_ids().await.unwrap(), vec![PLATFORM_STORE, own]);
    mb.expunge_many_at(&[rb.id], 1).await.unwrap();
    gc.purge_deleted(i64::MAX).await.unwrap();
    let later = SystemTime::now() + 2 * HOUR;
    let on_platform = gc.for_store(PLATFORM_STORE).collect_blobs_at(&*platform, HOUR, later).await.unwrap();
    assert_eq!(on_platform.rows_deleted, 0, "nothing of the platform store is unreferenced");
    assert_eq!(platform.list().await.unwrap().len(), 1);
    let on_tenant = gc.for_store(own).collect_blobs_at(&*tenant, HOUR, later).await.unwrap();
    assert_eq!(on_tenant.rows_deleted, 1);
    assert!(tenant.list().await.unwrap().is_empty(), "the tenant's object was collected");
    assert_eq!(platform.list().await.unwrap().len(), 1, "the platform's object is still there");
    assert_eq!(ma.get_raw(ra.id).await.unwrap(), body);
    let left: Vec<i64> = sqlx::query_scalar("SELECT store_id FROM blobs").fetch_all(b.fx()).await.unwrap();
    assert_eq!(left, vec![PLATFORM_STORE.0]);
}

#[tokio::test]
async fn giving_a_domain_a_store_has_rules() {
    let Some(b) = pg_backend().await else { return };
    let repo = Arc::new(b.pg.as_ref().unwrap().1.clone());
    let (d1, d2) = (b.dir.create_domain("one.test").await.unwrap(), b.dir.create_domain("two.test").await.unwrap());
    let acc = b.dir.create_account("u", d1, None, None).await.unwrap();

    // Only filesystem and S3 stores belong to a domain; the platform row is not made here.
    assert!(matches!(repo.create_for_domain(d1, "platform", "{}").await, Err(Error::Invalid(_))));
    assert!(matches!(repo.create_for_domain(rusty_core::DomainId(9999), "fs", "{}").await, Err(Error::NotFound(_))));
    // A domain that already holds a message is refused (version 1 does not move bodies).
    let tmp = tempfile::tempdir().unwrap();
    let mb = Mailbox::open_with(b.mrepo.clone(), b.blobs.clone(), acc).await.unwrap();
    mb.append(INBOX, raw("first", None), Flags::default()).await.unwrap();
    assert!(matches!(repo.create_for_domain(d1, "fs", r#"{"dir":"x"}"#).await, Err(Error::Conflict(_))));
    drop(tmp);
    // A domain gets one store, once.
    repo.create_for_domain(d2, "fs", r#"{"dir":"y"}"#).await.unwrap();
    assert!(matches!(repo.create_for_domain(d2, "fs", r#"{"dir":"z"}"#).await, Err(Error::Conflict(_))));
    // The database refuses a second store for one domain, and a platform shape for a domain's row.
    let dup = sqlx::query("INSERT INTO blob_stores (domain_id, kind, created_at) VALUES ($1, 'fs', 0)").bind(d2.0).execute(b.fx()).await;
    assert!(dup.is_err());
    let bad = sqlx::query("INSERT INTO blob_stores (domain_id, kind, created_at) VALUES ($1, 'platform', 0)").bind(d1.0).execute(b.fx()).await;
    assert!(bad.is_err());
}

#[tokio::test]
async fn a_router_without_the_store_refuses_instead_of_falling_back_to_the_platform_store() {
    let Some(b) = pg_backend().await else { return };
    // The single-store router (what a deployment without tenant stores, and most tests, use) serves the platform store only.
    let single: Arc<dyn BlobRouter> = rusty_store::IntoRouter::into_router(b.blobs.clone());
    assert!(single.store(PLATFORM_STORE).await.is_ok());
    assert!(matches!(single.store(StoreId(2)).await, Err(Error::NotFound(_))));
    // A body whose message row names a store the router cannot open is an error, never another store's body.
    let repo = Arc::new(b.pg.as_ref().unwrap().1.clone());
    let d = b.dir.create_domain("c.test").await.unwrap();
    let acc = b.dir.create_account("c", d, None, None).await.unwrap();
    let own = repo.create_for_domain(d, "fs", r#"{"dir":"c"}"#).await.unwrap();
    assert_ne!(own, PLATFORM_STORE);
    let mb = Mailbox::open_with(b.mrepo.clone(), single, acc).await.unwrap();
    assert!(mb.append(INBOX, raw("x", None), Flags::default()).await.is_err(), "a write to a store the router does not have fails");
}

#[tokio::test]
async fn collecting_one_store_leaves_the_same_unreferenced_content_of_another_store_alone() {
    let Some(b) = pg_backend().await else { return };
    let repo = Arc::new(b.pg.as_ref().unwrap().1.clone());
    let tmp = tempfile::tempdir().unwrap();
    let router: Arc<dyn BlobRouter> = Arc::new(BlobStores::new(b.blobs.clone(), repo.clone(), Arc::new(FsStoreOpener::new(tmp.path().join("stores")))));
    let (da, db_) = (b.dir.create_domain("a.test").await.unwrap(), b.dir.create_domain("b.test").await.unwrap());
    let (alice, bob) = (b.dir.create_account("alice", da, None, None).await.unwrap(), b.dir.create_account("bob", db_, None, None).await.unwrap());
    let own = repo.create_for_domain(db_, "fs", r#"{"dir":"tenant-b"}"#).await.unwrap();
    let (ma, mb) = (Mailbox::open_with(b.mrepo.clone(), router.clone(), alice).await.unwrap(), Mailbox::open_with(b.mrepo.clone(), router.clone(), bob).await.unwrap());
    let body = raw("same body again", Some("<same2@x>"));
    let ra = ma.append(INBOX, body.clone(), Flags::default()).await.unwrap();
    let rb = mb.append(INBOX, body.clone(), Flags::default()).await.unwrap();
    // Both copies become unreferenced: two rows with the same hash, one in each store.
    ma.expunge_many_at(&[ra.id], 1).await.unwrap();
    mb.expunge_many_at(&[rb.id], 1).await.unwrap();
    let gc = Gc::new(b.grepo.clone());
    gc.purge_deleted(i64::MAX).await.unwrap();
    let unreferenced: i64 = sqlx::query_scalar("SELECT count(*) FROM blobs WHERE refcount = 0").fetch_one(b.fx()).await.unwrap();
    assert_eq!(unreferenced, 2);

    let (platform, tenant) = (router.store(PLATFORM_STORE).await.unwrap(), router.store(own).await.unwrap());
    let later = SystemTime::now() + 2 * HOUR;
    // The tenant's run takes the tenant's row and object and nothing else.
    let r = gc.for_store(own).collect_blobs_at(&*tenant, HOUR, later).await.unwrap();
    assert_eq!(r.rows_deleted, 1);
    let left: Vec<i64> = sqlx::query_scalar("SELECT store_id FROM blobs").fetch_all(b.fx()).await.unwrap();
    assert_eq!(left, vec![PLATFORM_STORE.0], "the platform store's row of the same content is still there");
    assert_eq!(platform.list().await.unwrap().len(), 1, "and so is its object");
    // Then the platform's own run takes its own.
    let r = gc.for_store(PLATFORM_STORE).collect_blobs_at(&*platform, HOUR, later).await.unwrap();
    assert_eq!(r.rows_deleted, 1);
    assert!(platform.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_s3_settings_of_a_domain_change_only_while_its_store_is_empty() {
    use rusty_core::storage_policy::validate_tenant_s3;
    use rusty_store::BlobStoreAdmin;
    let Some(b) = pg_backend().await else { return };
    let repo = Arc::new(b.pg.as_ref().unwrap().1.clone());
    let admin = BlobStoreAdmin::with_repo(repo.clone());
    let d = b.dir.create_domain("s3.test").await.unwrap();
    let s3 = |bucket: &str| validate_tenant_s3("https://s3.example-cloud.com", "eu-west-3", bucket, "mail", true).unwrap();

    let (row, created) = admin.set_s3(d, &s3("bucket-one")).await.unwrap();
    assert!(created);
    assert_eq!((row.kind.as_str(), row.domain, row.check_status.as_str()), ("s3", Some(d), "unchecked"));
    // The same settings again: nothing changes (that is how credentials are rotated).
    admin.record_check(row.id, "ok").await.unwrap();
    let (same, created) = admin.set_s3(d, &s3("bucket-one")).await.unwrap();
    assert!((!created) && same.id == row.id);
    assert_eq!((same.check_status.as_str(), same.checked_at), ("unchecked", None), "new keys: the store has to pass a check again");
    // Other settings while the store is empty are fine, and forget the old check.
    admin.record_check(row.id, "ok").await.unwrap();
    let (changed, created) = admin.set_s3(d, &s3("bucket-two")).await.unwrap();
    assert!(!created && changed.config.contains("bucket-two") && changed.check_status == "unchecked" && changed.checked_at.is_none(), "{changed:?}");
    // A check records a fixed category and its time; anything else is refused.
    admin.record_check_at(row.id, "auth", 1234).await.unwrap();
    let r = admin.get(row.id).await.unwrap().unwrap();
    assert_eq!((r.check_status.as_str(), r.checked_at), ("auth", Some(1234)));
    assert!(matches!(admin.record_check(row.id, "whatever the server said").await, Err(Error::Invalid(_))));
    assert!(matches!(admin.record_check(StoreId(9999), "ok").await, Err(Error::NotFound(_))));
    // Once a body is stored the settings are frozen: they would leave the bodies behind.
    sqlx::query("INSERT INTO blobs (store_id, hash, size) VALUES ($1, decode(repeat('ab',32),'hex'), 1)").bind(row.id.0).execute(b.fx()).await.unwrap();
    assert!(matches!(admin.set_s3(d, &s3("bucket-three")).await, Err(Error::Conflict(_))));
    assert!(admin.set_s3(d, &s3("bucket-two")).await.is_ok(), "the unchanged settings are still accepted");
    // The platform store has no owner and is never changed through this; a store of another kind is not an S3 store.
    assert!(matches!(repo.update_config(PLATFORM_STORE, "{}").await, Err(Error::NotFound(_))));
    let fs_dom = b.dir.create_domain("fs.test").await.unwrap();
    repo.create_for_domain(fs_dom, "fs", r#"{"dir":"x"}"#).await.unwrap();
    assert!(matches!(admin.set_s3(fs_dom, &s3("bucket-one")).await, Err(Error::Conflict(_))));
    // A domain that already holds messages on the platform store cannot get a store.
    let other = b.dir.create_domain("held.test").await.unwrap();
    let held = b.dir.create_account("h", other, None, None).await.unwrap();
    Mailbox::open_with(b.mrepo.clone(), b.blobs.clone(), held).await.unwrap().append(INBOX, raw("x", None), Flags::default()).await.unwrap();
    assert!(matches!(admin.set_s3(other, &s3("bucket-one")).await, Err(Error::Conflict(_))));
}

/// An opener that hands out a filesystem store whatever the row says: the test is about when the registry opens a row, not about S3.
struct AnyKindOpener(std::path::PathBuf);

#[async_trait::async_trait]
impl rusty_store::StoreOpener for AnyKindOpener {
    async fn open(&self, _row: &rusty_store::BlobStoreRow) -> Result<Arc<dyn rusty_store::BlobStore>, Error> {
        rusty_store::FsBlobStore::init(&self.0).await?;
        Ok(Arc::new(rusty_store::FsBlobStore::open(&self.0).await?))
    }
}

#[tokio::test]
async fn a_tenants_s3_store_is_opened_only_after_a_check_recorded_ok() {
    use rusty_core::storage_policy::validate_tenant_s3;
    use rusty_store::BlobStoreAdmin;
    let Some(b) = pg_backend().await else { return };
    let repo = Arc::new(b.pg.as_ref().unwrap().1.clone());
    let admin = BlobStoreAdmin::with_repo(repo.clone());
    let tmp = tempfile::tempdir().unwrap();
    let router = BlobStores::new(b.blobs.clone(), repo.clone(), Arc::new(AnyKindOpener(tmp.path().join("s3-stand-in"))));
    let d = b.dir.create_domain("ready.test").await.unwrap();
    let (row, _) = admin.set_s3(d, &validate_tenant_s3("https://s3.example-cloud.com", "eu-west-3", "tenant-mail", "", true).unwrap()).await.unwrap();

    // Never checked, or a check that failed: the mail of the domain gets a temporary failure, the store is not touched.
    assert!(matches!(router.store(row.id).await, Err(Error::Busy(_))), "unchecked");
    admin.record_check(row.id, "auth").await.unwrap();
    assert!(matches!(router.store(row.id).await, Err(Error::Busy(_))), "a failed check");
    admin.record_check(row.id, "ok").await.unwrap();
    assert!(router.store(row.id).await.is_ok(), "a passed check opens it");
    // New keys (the same settings again) make it unchecked once more; a store that the registry already holds keeps working until the services restart (a later version drops it on the change).
    admin.set_s3(d, &validate_tenant_s3("https://s3.example-cloud.com", "eu-west-3", "tenant-mail", "", true).unwrap()).await.unwrap();
    router.forget(row.id);
    assert!(matches!(router.store(row.id).await, Err(Error::Busy(_))), "unchecked again after a key change");
    // A filesystem store never needs a check; the platform store neither.
    let fs_dom = b.dir.create_domain("fsready.test").await.unwrap();
    let fs_id = repo.create_for_domain(fs_dom, "fs", r#"{"dir":"x"}"#).await.unwrap();
    assert!(router.store(fs_id).await.is_ok());
    assert!(router.store(PLATFORM_STORE).await.is_ok());
}
