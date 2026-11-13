//! Real-Postgres test support (feature test-fixtures). Every test database is isolated and explicitly disposable.

use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rand_core::{OsRng, RngCore};
use crate::PgStore;
use sqlx::postgres::PgPoolOptions;
use url::Url;

static REAPED: AtomicBool = AtomicBool::new(false);

pub const PG_SKIP_NOTICE: &str =
    "POSTGRES_TESTS_SKIPPED: RUSTY_TEST_PG_URL is not set; no Postgres test server was used";

/// Read the admin URL without ever printing its value.
/// Passwords of the test roles. The test Postgres is a throw-away loopback container; these are
/// not secrets and never leave the test harness.
pub const TEST_APP_PASSWORD: &str = "rusty-test-app";
pub const TEST_MIGRATOR_PASSWORD: &str = "rusty-test-migrator";
pub const TEST_RO_PASSWORD: &str = "rusty-test-ro";

/// deploy/db/roles.sql: the same file the operator runs.
const ROLES_SQL: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/db/roles.sql"));

/// `RUSTY_TEST_PG_ROLES=1`: every test database is initialised with deploy/db/roles.sql, migrated
/// by the migrator role, and `url()` / `connect()` use the unprivileged `rusty_app` role, so a whole
/// suite runs the way production does. Anything but `1` / unset / `0` is an error.
pub fn roles_mode() -> bool {
    match env::var("RUSTY_TEST_PG_ROLES").ok().as_deref() {
        None | Some("") | Some("0") => false,
        Some("1") => true,
        Some(_) => panic!("RUSTY_TEST_PG_ROLES must be 1 (or unset / 0)"),
    }
}

fn url_for(admin_db_url: &str, user: &str, password: &str) -> String {
    let mut u = Url::parse(admin_db_url).expect("test URL");
    u.set_username(user).expect("username");
    u.set_password(Some(password)).expect("password");
    u.into()
}

/// Run deploy/db/roles.sql and give the three roles their test logins. Concurrent test binaries
/// race on the cluster-wide role catalog ("tuple concurrently updated"), so the whole step is retried.
async fn apply_roles(admin_db_url: &str) {
    let admin = PgPoolOptions::new().max_connections(1).connect(admin_db_url).await.expect("connect to the test database");
    let logins = format!(
        "ALTER ROLE rusty_migrator LOGIN PASSWORD '{TEST_MIGRATOR_PASSWORD}'; \
         ALTER ROLE rusty_app LOGIN PASSWORD '{TEST_APP_PASSWORD}'; \
         ALTER ROLE rusty_ro LOGIN PASSWORD '{TEST_RO_PASSWORD}';"
    );
    let mut last = None;
    for attempt in 0..40u64 {
        let first = sqlx::raw_sql(ROLES_SQL).execute(&admin).await;
        let res = match first {
            Ok(_) => sqlx::raw_sql(&logins).execute(&admin).await,
            Err(e) => Err(e),
        };
        match res {
            Ok(_) => {
                admin.close().await;
                return;
            }
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(30 + (attempt % 7) * 20)).await;
            }
        }
    }
    panic!("roles.sql failed in the test database: {last:?}");
}

pub fn pg_admin_url() -> Option<String> {
    admin_url_from(
        env::var("RUSTY_TEST_PG_URL").ok(),
        env::var("RUSTY_REQUIRE_PG").ok().as_deref(),
    )
}

/// Pure core of [`pg_admin_url`]. Postgres is the only backend, so a missing URL is a failure, never a
/// skip (`scripts/pg-up.sh` starts a throw-away server and writes its URL). `RUSTY_REQUIRE_PG` is still
/// parsed first (a bad value panics) for scripts that set it; it no longer changes the outcome. The
/// return type stays `Option` only so the many `let Some(url) = pg_admin_url() else { return }` call
/// sites keep compiling; the `None` arm cannot be reached.
pub fn admin_url_from(url: Option<String>, require: Option<&str>) -> Option<String> {
    let _ = parse_require_pg(require);
    match url {
        Some(value) if !value.is_empty() => Some(value),
        _ => panic!(
            "RUSTY_TEST_PG_URL is not set: the tests need a Postgres server (bash scripts/pg-up.sh starts one)"
        ),
    }
}

/// A database created from the admin URL. Its name contains only generated hex digits and
/// underscores: `rusty_test_<creation unix time, 10 hex>_<random 32 hex>`.
pub struct PgTestDb {
    name: String,
    url: String,
    admin_url: String,
    /// Superuser URL of this database (always available, also in roles mode).
    admin_db_url: String,
    cleaned: bool,
}

impl PgTestDb {
    /// A migrated database. In roles mode (`RUSTY_TEST_PG_ROLES=1`) it is initialised with roles.sql,
    /// migrated by `rusty_migrator`, and `url()` is the `rusty_app` login.
    pub async fn create(admin_url: &str) -> Self {
        Self::create_inner(admin_url, roles_mode(), true).await
    }

    /// A database with the roles applied and migrated by the migrator, whatever the environment says.
    pub async fn create_with_roles(admin_url: &str) -> Self {
        Self::create_inner(admin_url, true, true).await
    }

    /// An EMPTY database (no schema, no roles): for the migrate and schema-version tests.
    pub async fn create_unmigrated(admin_url: &str) -> Self {
        Self::create_inner(admin_url, false, false).await
    }

    async fn create_inner(admin_url: &str, roles: bool, migrate: bool) -> Self {
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
        let admin_db_url: String = parsed.into();
        let mut db = Self {
            name,
            url: admin_db_url.clone(),
            admin_url: admin_url.to_owned(),
            admin_db_url: admin_db_url.clone(),
            cleaned: false,
        };
        if roles {
            apply_roles(&admin_db_url).await;
        }
        if migrate {
            let migrator = if roles { url_for(&admin_db_url, "rusty_migrator", TEST_MIGRATOR_PASSWORD) } else { admin_db_url.clone() };
            crate::migrate_postgres(&migrator, &crate::PgSecurity::default(), roles)
                .await
                .expect("migrate the isolated Postgres test database");
        }
        if roles {
            db.url = url_for(&admin_db_url, "rusty_app", TEST_APP_PASSWORD);
        }
        db
    }

    /// Superuser URL of this database.
    pub fn admin_db_url(&self) -> &str {
        &self.admin_db_url
    }

    /// Login URL of one of the test roles in this database (`rusty_app`, `rusty_migrator`, `rusty_ro`).
    pub fn role_url(&self, role: &str) -> String {
        let pw = match role {
            "rusty_app" => TEST_APP_PASSWORD,
            "rusty_migrator" => TEST_MIGRATOR_PASSWORD,
            "rusty_ro" => TEST_RO_PASSWORD,
            _ => panic!("unknown test role"),
        };
        url_for(&self.admin_db_url, role, pw)
    }

    /// `ALTER DATABASE .. SET name = value` (a superuser statement): a setting every NEW session of this
    /// test database gets, the stand-in for a role-level `ALTER ROLE .. SET` of the production roles.
    pub async fn set_database_setting(&self, name: &str, value: &str) {
        assert!(name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'), "setting name");
        assert!(value.bytes().all(|b| b.is_ascii_alphanumeric()), "setting value");
        let admin = PgPoolOptions::new().max_connections(1).connect(&self.admin_url).await.expect("connect to Postgres test admin");
        sqlx::query(&format!("ALTER DATABASE \"{}\" SET {name} = '{value}'", self.name))
            .execute(&admin)
            .await
            .expect("alter the test database setting");
        // In roles mode `rusty_app` carries its own role-level timeouts (deploy/db/roles.sql) and a role setting
        // beats a database setting: override it for this database only, as an operator would per database.
        if roles_mode() {
            sqlx::query(&format!(
                "ALTER ROLE rusty_app IN DATABASE \"{}\" SET {name} = '{value}'",
                self.name
            ))
            .execute(&admin)
            .await
            .expect("alter the app role setting for the test database");
        }
        admin.close().await;
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

/// `RUSTY_REQUIRE_PG` accepts exactly `1` (required), or unset / empty / `0` (not required).
/// Any other value is a configuration error and fails loudly instead of silently skipping.
pub fn require_pg() -> bool {
    parse_require_pg(env::var("RUSTY_REQUIRE_PG").ok().as_deref())
}

pub fn parse_require_pg(value: Option<&str>) -> bool {
    match value {
        None | Some("") | Some("0") => false,
        Some("1") => true,
        Some(_) => panic!("RUSTY_REQUIRE_PG must be 1 (or unset / 0)"),
    }
}
