//! The `storage.check` job: a queued check opens the store's row, runs the probe through a `Prober` and records the category on the row; jobs that are not ours, stores that are gone and stores
//! of another kind change nothing and do not loop. The probe itself is tested against a real S3 server in `rustfs.rs`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusty_core::storage_policy::validate_tenant_s3;
use rusty_s3::{enqueue_storage_check, Prober, StorageCheckWorker, STORAGE_CHECK_JOB_KIND};
use rusty_store::fixtures::pg_db::{pg_admin_url, PgTestDb};
use rusty_store::repo::{BackendRow, BackendState, BlobStoreRow, StoreId};
use rusty_store::{BlobPoolAdmin, BlobStoreAdmin, JobQueue, PoolPolicy, Store, StoreConfig};
use rusty_svc::{Context, Service, TlsProvider};
use tokio_util::sync::CancellationToken;

/// A prober that answers with a fixed category and counts its calls.
struct Fixed(&'static str, AtomicU64);

#[async_trait]
impl Prober for Fixed {
    async fn probe(&self, _row: &BlobStoreRow, _nonce: u64) -> &'static str {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0
    }

    async fn probe_backend(&self, _row: &BackendRow, _nonce: u64) -> &'static str {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0
    }
}

struct Rig {
    store: Store,
    ctx: Context,
    admin: BlobStoreAdmin,
    jobs: JobQueue,
    row: BlobStoreRow,
    domain: rusty_core::DomainId,
    db: PgTestDb,
    _tmp: tempfile::TempDir,
}

async fn rig() -> Option<Rig> {
    let url = pg_admin_url()?;
    let db = PgTestDb::create(&url).await;
    let store = Store::open(StoreConfig::postgres(db.url())).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = rusty_core::config::StartupConfig::dev(tmp.path());
    rusty_store::FsBlobStore::init(&cfg.blob_dir).await.unwrap();
    let blobs: Arc<dyn rusty_store::BlobStore> = Arc::new(rusty_store::FsBlobStore::open(&cfg.blob_dir).await.unwrap());
    let cfg = Arc::new(cfg);
    let tls = TlsProvider::unavailable().unwrap();
    let ctx = Context::new(store.repos(), blobs, cfg, tls).await.unwrap();
    let domain = rusty_directory::Directory::with_repo(store.repos().directory.clone()).create_domain("tenant.test").await.unwrap();
    let admin = BlobStoreAdmin::with_repo(store.repos().blob_stores.clone());
    let (row, _) = admin.set_s3(domain, &validate_tenant_s3("https://s3.example-cloud.com", "eu-west-3", "tenant-mail", "", true).unwrap()).await.unwrap();
    let jobs = JobQueue::with_repo(store.repos().jobs.clone());
    Some(Rig { store, ctx, admin, jobs, row, domain, db, _tmp: tmp })
}

/// Run a worker until `done` says so (or five seconds pass).
async fn run_worker(r: &Rig, prober: Arc<Fixed>, done: impl Fn() -> bool) {
    let worker = StorageCheckWorker::new(r.admin.clone(), prober).with_poll(Duration::from_millis(20));
    let token = CancellationToken::new();
    let task = tokio::spawn(Box::new(worker).run(r.ctx.clone(), token.clone()));
    for _ in 0..250 {
        if done() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

/// How many jobs of the kind are not finished (queued or running).
async fn unfinished(r: &Rig) -> i64 {
    let fx = rusty_store::fixtures::Fixtures::admin_of(&r.db, &r.store).await;
    fx.scalar(&format!("SELECT count(*) FROM jobs WHERE kind = '{STORAGE_CHECK_JOB_KIND}' AND state NOT IN ('done', 'failed')"), &[]).await
}

#[tokio::test]
async fn a_queued_check_records_its_category_on_the_store_and_the_job_ends() {
    let Some(r) = rig().await else { return };
    for category in ["ok", "auth", "no-bucket", "config"] {
        enqueue_storage_check(&r.jobs, r.domain, r.row.id).await.unwrap();
        let prober = Arc::new(Fixed(Box::leak(category.to_owned().into_boxed_str()), AtomicU64::new(0)));
        let p = prober.clone();
        run_worker(&r, prober, move || p.1.load(Ordering::SeqCst) >= 1).await;
        // The worker records right after the probe: give it a moment, then read the row.
        for _ in 0..50 {
            if r.admin.get(r.row.id).await.unwrap().unwrap().check_status == category {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let row = r.admin.get(r.row.id).await.unwrap().unwrap();
        assert_eq!(row.check_status, category);
        assert!(row.checked_at.is_some());
    }
    assert_eq!(unfinished(&r).await, 0, "every job ended");
}

#[tokio::test]
async fn jobs_that_are_not_ours_or_name_nothing_checkable_change_nothing_and_do_not_loop() {
    let Some(r) = rig().await else { return };
    let before = r.admin.get(r.row.id).await.unwrap().unwrap();
    // A payload without a store, the platform store, a store that does not exist and a store of another kind.
    let fs_domain = rusty_directory::Directory::with_repo(r.store.repos().directory.clone()).create_domain("fs.test").await.unwrap();
    let fs_store = r.store.repos().blob_stores.create_for_domain(fs_domain, "fs", r#"{"dir":"x"}"#).await.unwrap();
    for payload in [serde_json::json!({}), serde_json::json!({ "store": 1 }), serde_json::json!({ "store": 9999 }), serde_json::json!({ "store": fs_store.0 }), serde_json::json!({ "store": "x" })] {
        r.jobs.enqueue(STORAGE_CHECK_JOB_KIND, Some(r.domain), &payload, None).await.unwrap();
    }
    let prober = Arc::new(Fixed("ok", AtomicU64::new(0)));
    run_worker(&r, prober.clone(), || false).await;
    assert_eq!(prober.1.load(Ordering::SeqCst), 0, "the prober ran for none of them");
    assert_eq!(r.admin.get(r.row.id).await.unwrap().unwrap().check_status, before.check_status);
    assert_eq!(r.admin.get(StoreId(fs_store.0)).await.unwrap().unwrap().check_status, "unchecked");
    assert_eq!(unfinished(&r).await, 0, "every job was settled, none is left to loop");
}

#[tokio::test]
async fn the_check_of_a_pool_tries_every_backend_that_is_used_and_the_pool_is_ready_only_when_each_passed() {
    let Some(r) = rig().await else { return };
    let pools = BlobPoolAdmin::with_repos(r.store.repos().blob_stores.clone(), r.store.repos().blob_pool.clone());
    let dom = rusty_directory::Directory::with_repo(r.store.repos().directory.clone()).create_domain("pool.test").await.unwrap();
    let pool = pools.set_policy(dom, PoolPolicy::parse(r#"{"copies":2}"#).unwrap()).await.unwrap();
    let s3 = |bucket: &str| validate_tenant_s3("https://s3.example-cloud.com", "eu-west-3", bucket, "", true).unwrap();
    let a = pools.add_s3(pool.id, "a", &s3("bucket-a"), 100).await.unwrap();
    let b = pools.add_s3(pool.id, "b", &s3("bucket-b"), 100).await.unwrap();
    let c = pools.add_s3(pool.id, "c", &s3("bucket-c"), 100).await.unwrap();
    pools.set_state(c.id, BackendState::Disabled).await.unwrap();
    enqueue_storage_check(&r.jobs, dom, pool.id).await.unwrap();
    let prober = Arc::new(Fixed("ok", AtomicU64::new(0)));
    let p = prober.clone();
    let worker = StorageCheckWorker::new(r.admin.clone(), prober.clone()).with_pools(pools.clone()).with_poll(Duration::from_millis(20));
    let token = CancellationToken::new();
    let task = tokio::spawn(Box::new(worker).run(r.ctx.clone(), token.clone()));
    for _ in 0..250 {
        if r.admin.get(pool.id).await.unwrap().unwrap().check_status == "ok" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(p.1.load(Ordering::SeqCst), 2, "the two backends in use were tried, the disabled one was not");
    assert_eq!(pools.backend(a.id).await.unwrap().unwrap().check_status, "ok");
    assert_eq!(pools.backend(b.id).await.unwrap().unwrap().check_status, "ok");
    assert_eq!(pools.backend(c.id).await.unwrap().unwrap().check_status, "unchecked");
    assert_eq!(r.admin.get(pool.id).await.unwrap().unwrap().check_status, "ok");
    // A backend that was added after the pool passed makes it unchecked again until the next check.
    let d = pools.add_s3(pool.id, "d", &s3("bucket-d"), 100).await.unwrap();
    assert_eq!(r.admin.get(pool.id).await.unwrap().unwrap().check_status, "unchecked");
    assert_eq!(d.check_status, "unchecked");
    token.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}
