//! Real-Postgres test support. Every test database is isolated and explicitly disposable.

use std::env;

use rand_core::{OsRng, RngCore};
use rusty_store::PgStore;
use sqlx::postgres::PgPoolOptions;
use url::Url;

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

/// A database created from the admin URL. Its name contains only generated hex digits.
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
        let name = format!("rusty_test_{}", hex::encode(random));

        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(admin_url)
            .await
            .expect("connect to Postgres test admin");
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
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                cleanup_database(&admin_url, &name).await;
            });
        }
    }
}

async fn cleanup_database(admin_url: &str, name: &str) {
    let Ok(admin) = PgPoolOptions::new()
        .max_connections(1)
        .connect(admin_url)
        .await
    else {
        return;
    };
    let _ = sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = $1 AND pid <> pg_backend_pid()",
    )
    .bind(name)
    .execute(&admin)
    .await;
    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\""))
        .execute(&admin)
        .await;
    admin.close().await;
}
