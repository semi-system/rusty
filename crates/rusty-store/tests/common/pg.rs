//! Real-Postgres test support. Every test database is isolated and explicitly disposable.

use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rand_core::{OsRng, RngCore};
use rusty_store::PgStore;
use sqlx::postgres::PgPoolOptions;
use url::Url;

static REAPED: AtomicBool = AtomicBool::new(false);

pub const PG_SKIP_NOTICE: &str =
    "POSTGRES_TESTS_SKIPPED: RUSTY_TEST_PG_URL is not set; no Postgres test server was used";

/// Read the admin URL without ever printing its value.
pub fn pg_admin_url() -> Option<String> {
    match env::var("RUSTY_TEST_PG_URL") {
        Ok(value) if !value.is_empty() => Some(value),
        _ if env::var_os("RUSTY_REQUIRE_PG").is_some() => {
            panic!("RUSTY_REQUIRE_PG is set but RUSTY_TEST_PG_URL is missing")
        }
        _ => {
            eprintln!("{PG_SKIP_NOTICE}");
            None
        }
    }
}

/// A database created from the admin URL. Its name contains only generated hex digits and
/// underscores: `rusty_test_<creation unix time, 10 hex>_<random 32 hex>`.
pub struct PgTestDb {
    name: String,
    url: String,
    admin_url: String,
    cleaned: bool,
}

impl PgTestDb {
    pub async fn create(admin_url: &str) -> Self {
        let mut random = [0u8; 16];
        OsRng.fill_bytes(&mut random);
        let name = format!("rusty_test_{:010x}_{}", unix_secs(), hex::encode(random));

        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(admin_url)
            .await
            .expect("connect to Postgres test admin");
        if !REAPED.swap(true, Ordering::SeqCst) {
            reap_stale(&admin).await;
        }
        // `name` is generated above from a fixed prefix and lowercase hex only. PostgreSQL
        // identifiers cannot be bound as parameters, so this is the sole intentional SQL
        // interpolation in the helper.
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&admin)
            .await
            .expect("create isolated Postgres test database");
        admin.close().await;

        let mut parsed = Url::parse(admin_url).expect("RUSTY_TEST_PG_URL must be a URL");
        parsed.set_path(&format!("/{name}"));
        let url = parsed.into();
        Self {
            name,
            url,
            admin_url: admin_url.to_owned(),
            cleaned: false,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn connect(&self) -> PgStore {
        PgStore::connect(&self.url)
            .await
            .expect("connect to isolated Postgres test database")
    }

    pub async fn cleanup(mut self) {
        cleanup_database(&self.admin_url, &self.name).await;
        self.cleaned = true;
    }
}

impl Drop for PgTestDb {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        // A failing test panics, which tears the async runtime down before a spawned cleanup
        // task could run. Clean up synchronously on a dedicated thread with its own runtime.
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        let _ = std::thread::spawn(move || {
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                rt.block_on(cleanup_database(&admin_url, &name));
            }
        })
        .join();
    }
}

/// Databases older than this are leftovers of killed runs (SIGKILL, timeout) and are reaped.
const STALE_AFTER_SECS: u64 = 15 * 60;

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Drop `rusty_test_*` databases whose embedded creation time is older than the threshold.
/// Runs once per test binary; it is best-effort and never fails a test.
async fn reap_stale(admin: &sqlx::PgPool) {
    let Ok(names) = sqlx::query_scalar::<_, String>(
        r"SELECT datname FROM pg_database WHERE datname LIKE 'rusty\_test\_%'",
    )
    .fetch_all(admin)
    .await
    else {
        return;
    };
    let now = unix_secs();
    for name in names {
        let created = name
            .strip_prefix("rusty_test_")
            .and_then(|rest| rest.split('_').next())
            .filter(|ts| ts.len() == 10 && ts.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|ts| u64::from_str_radix(ts, 16).ok());
        // Names without a parsable timestamp (older format) are treated as stale too.
        if created.is_none_or(|c| now.saturating_sub(c) > STALE_AFTER_SECS) {
            drop_database(admin, &name).await;
        }
    }
}

async fn drop_database(admin: &sqlx::PgPool, name: &str) {
    let _ = sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = $1 AND pid <> pg_backend_pid()",
    )
    .bind(name)
    .execute(admin)
    .await;
    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
        .execute(admin)
        .await;
}

async fn cleanup_database(admin_url: &str, name: &str) {
    let Ok(admin) = PgPoolOptions::new()
        .max_connections(1)
        .connect(admin_url)
        .await
    else {
        return;
    };
    drop_database(&admin, name).await;
    admin.close().await;
}
