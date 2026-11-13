#![allow(dead_code)]
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use rusty_core::{AccountId, FolderId};
use rusty_store::{BlobStore, FsBlobStore, Mailbox, SqliteStore, BLOB_ROOT_MARKER, GC_LOCK_FILE};

pub mod backends;
#[cfg(feature = "postgres")]
pub mod pg;
pub mod shapes;
#[allow(unused_imports)]
pub use backends::{
    blob_row_exists, clear_injected_failure, hash_lit, inject_insert_failure, insert_blob_row,
    set_zero_since, Bind,
};

/// Fresh private in-memory store with migrations applied.
pub async fn mem() -> SqliteStore {
    SqliteStore::open("sqlite::memory:").await.unwrap()
}

/// File-backed store (pool of 4) inside `dir`.
pub async fn file_store(dir: &Path) -> SqliteStore {
    let url = format!("sqlite://{}", dir.join("meta.sqlite").display());
    SqliteStore::open(&url).await.unwrap()
}

pub const INBOX: FolderId = FolderId(1);
pub const SENT: FolderId = FolderId(2);
pub const DRAFTS: FolderId = FolderId(3);
pub const TRASH: FolderId = FolderId(4);
pub const SPAM: FolderId = FolderId(5);

/// Insert domain `example.com` (if missing), account `local`, its mailbox and the five
/// system folders with ids 1..=5, mirroring `rusty_directory::Directory::create_account`
/// (rusty-store cannot depend on rusty-directory).
pub async fn account(store: &SqliteStore, local: &str) -> AccountId {
    let p = store.pool();
    sqlx::query(
        "INSERT OR IGNORE INTO domains (id, name, created_at) VALUES (1, 'example.com', 0)",
    )
    .execute(p)
    .await
    .unwrap();
    let acc: i64 = sqlx::query_scalar(
        "INSERT INTO accounts (local_part, domain_id, status, created_at) \
         VALUES (?, 1, 'active', 0) RETURNING id",
    )
    .bind(local)
    .fetch_one(p)
    .await
    .unwrap();
    let mb: i64 = sqlx::query_scalar(
        "INSERT INTO mailboxes (account_id, uid_validity, next_folder_id) VALUES (?, 7, 6) \
         RETURNING id",
    )
    .bind(acc)
    .fetch_one(p)
    .await
    .unwrap();
    let system = [
        (1i64, "Inbox", "inbox"),
        (2, "Sent", "sent"),
        (3, "Drafts", "drafts"),
        (4, "Trash", "trash"),
        (5, "Spam", "spam"),
    ];
    for (id, name, role) in system {
        sqlx::query(
            "INSERT INTO folders (mailbox_id, id, parent_id, name, role, uid_validity) \
             VALUES (?, ?, NULL, ?, ?, 11)",
        )
        .bind(mb)
        .bind(id)
        .bind(name)
        .bind(role)
        .execute(p)
        .await
        .unwrap();
    }
    AccountId(acc)
}

/// A mailbox over an in-memory store with filesystem blobs in a temp dir.
pub struct LiteEnv {
    pub store: SqliteStore,
    pub blobs: Arc<FsBlobStore>,
    pub dir: tempfile::TempDir,
    pub account: AccountId,
    pub mb: Mailbox,
}

impl LiteEnv {
    /// Files under the blob root except the root's own marker and GC lock file.
    pub fn blob_files(&self) -> Vec<PathBuf> {
        let root = self.dir.path().join("blobs");
        all_files(&root)
            .into_iter()
            .filter(|p| {
                !(p.parent() == Some(root.as_path())
                    && (p.ends_with(BLOB_ROOT_MARKER) || p.ends_with(GC_LOCK_FILE)))
            })
            .collect()
    }

    /// Open a mailbox for another account sharing this store and blob store.
    pub async fn other(&self, local: &str) -> Mailbox {
        let acc = account(&self.store, local).await;
        Mailbox::open(
            self.store.clone(),
            self.blobs.clone() as Arc<dyn BlobStore>,
            acc,
        )
        .await
        .unwrap()
    }

    pub async fn scalar(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql)
            .fetch_one(self.store.pool())
            .await
            .unwrap()
    }
}

pub async fn lite_env_with(store: SqliteStore, dir: tempfile::TempDir) -> LiteEnv {
    let blobs = Arc::new(blob_store(&dir.path().join("blobs")).await);
    let account = account(&store, "alice").await;
    let mb = Mailbox::open(store.clone(), blobs.clone() as Arc<dyn BlobStore>, account)
        .await
        .unwrap();
    LiteEnv {
        store,
        blobs,
        dir,
        account,
        mb,
    }
}

/// `init` + `open` a blob store at `root`.
pub async fn blob_store(root: &Path) -> FsBlobStore {
    FsBlobStore::init(root).await.unwrap();
    FsBlobStore::open(root).await.unwrap()
}

pub async fn lite_env() -> LiteEnv {
    lite_env_with(mem().await, tempfile::tempdir().unwrap()).await
}

pub fn all_files(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// A small RFC 5322 message.
pub fn raw(subject: &str, message_id: Option<&str>) -> Bytes {
    reply(subject, message_id, None, &[])
}

pub fn reply(
    subject: &str,
    message_id: Option<&str>,
    in_reply_to: Option<&str>,
    references: &[&str],
) -> Bytes {
    let mut s = String::from("From: Bob <bob@example.org>\r\nTo: alice@example.com\r\n");
    s.push_str(&format!("Subject: {subject}\r\n"));
    if let Some(id) = message_id {
        s.push_str(&format!("Message-ID: <{id}>\r\n"));
    }
    if let Some(id) = in_reply_to {
        s.push_str(&format!("In-Reply-To: <{id}>\r\n"));
    }
    if !references.is_empty() {
        let refs: Vec<String> = references.iter().map(|r| format!("<{r}>")).collect();
        s.push_str(&format!("References: {}\r\n", refs.join(" ")));
    }
    s.push_str("\r\nhello\r\n");
    Bytes::from(s)
}

/// A message of exactly `len` bytes (len >= 32).
pub fn sized(len: usize) -> Bytes {
    let head = b"Subject: sized\r\n\r\n";
    assert!(len >= head.len());
    let mut v = head.to_vec();
    v.resize(len, b'x');
    Bytes::from(v)
}

/// Counts the SQL statements sqlx executes (test-only; production code carries no counter).
/// sqlx emits one sqlx::query tracing event per executed statement (including BEGIN and
/// COMMIT), from its connection worker thread, so the counter is process-global. probe()
/// therefore also takes a process-wide lock: every test in a binary that uses it must call
/// probe() first, which serializes them.
pub mod stmts {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, MutexGuard, Once};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    static COUNT: AtomicUsize = AtomicUsize::new(0);
    static TEXTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static LOCK: Mutex<()> = Mutex::new(());
    static INIT: Once = Once::new();

    struct Counter;

    /// Reads the executed statement text from the `db.statement` field of a sqlx query event.
    struct Text {
        summary: Option<String>,
        statement: Option<String>,
    }

    impl Text {
        /// sqlx puts the full statement in `db.statement` only when the one-line `summary` is not
        /// the whole statement (long or multi-line SQL); otherwise `db.statement` is empty.
        fn text(self) -> Option<String> {
            match self.statement {
                Some(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
                _ => self.summary,
            }
        }
    }

    impl tracing::field::Visit for Text {
        fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
            match f.name() {
                "db.statement" => self.statement = Some(v.to_owned()),
                "summary" => self.summary = Some(v.to_owned()),
                _ => {}
            }
        }
        fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
            let s = format!("{v:?}");
            let s = s.trim_matches('"').to_owned();
            match f.name() {
                "db.statement" => self.statement = Some(s),
                "summary" => self.summary = Some(s),
                _ => {}
            }
        }
    }

    impl Subscriber for Counter {
        fn enabled(&self, m: &Metadata<'_>) -> bool {
            m.target() == "sqlx::query"
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, e: &Event<'_>) {
            if e.metadata().target() == "sqlx::query" {
                COUNT.fetch_add(1, Ordering::SeqCst);
                let mut v = Text {
                    summary: None,
                    statement: None,
                };
                e.record(&mut v);
                if let Some(t) = v.text() {
                    TEXTS.lock().unwrap_or_else(|p| p.into_inner()).push(t);
                }
            }
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    pub struct Probe {
        _lock: MutexGuard<'static, ()>,
    }

    /// Take the lock, install the counter (once) and start from zero.
    pub fn probe() -> Probe {
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        INIT.call_once(|| {
            tracing::subscriber::set_global_default(Counter).expect("global subscriber");
        });
        COUNT.store(0, Ordering::SeqCst);
        TEXTS.lock().unwrap_or_else(|p| p.into_inner()).clear();
        Probe { _lock: lock }
    }

    /// Hold the probe lock while a test environment is built: opening a connection runs
    /// statements (PRAGMA, migrations) that must not be counted by a test holding the probe.
    pub fn setup_guard() -> MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    impl Probe {
        pub fn count(&self) -> usize {
            COUNT.load(Ordering::SeqCst)
        }
        pub fn reset(&self) {
            COUNT.store(0, Ordering::SeqCst);
            TEXTS.lock().unwrap_or_else(|p| p.into_inner()).clear();
        }
        /// Statement texts executed since the last reset, in order (BEGIN and COMMIT included).
        pub fn statements(&self) -> Vec<String> {
            TEXTS.lock().unwrap_or_else(|p| p.into_inner()).clone()
        }
    }
}

// ------------------------------------------------------------------ backend-aware Env (PG6)

/// A mailbox over one backend (SQLite file store or a fresh Postgres database) with filesystem
/// blobs in a temp dir. Fixtures go through the repositories and the dialect-neutral helpers of
/// [`backends`]; no test holds a raw pool.
pub struct Env {
    pub b: backends::Backend,
    pub blobs: Arc<dyn BlobStore>,
    pub domain: rusty_core::DomainId,
    pub account: AccountId,
    pub mb: Mailbox,
}

impl Env {
    pub async fn new(b: backends::Backend) -> Env {
        let blobs = b.blobs.clone();
        let (domain, account, mb) = backends::acct(&b, "example").await;
        Env {
            b,
            blobs,
            domain,
            account,
            mb,
        }
    }

    pub fn is_sqlite(&self) -> bool {
        self.b.sqlite.is_some()
    }

    /// Files under the blob root except the root's own marker and GC lock file.
    pub fn blob_files(&self) -> Vec<PathBuf> {
        let root = self.b._tmp.path().join("blobs");
        all_files(&root)
            .into_iter()
            .filter(|p| {
                !(p.parent() == Some(root.as_path())
                    && (p.ends_with(BLOB_ROOT_MARKER) || p.ends_with(GC_LOCK_FILE)))
            })
            .collect()
    }

    /// A further account in the same domain, with its mailbox.
    pub async fn other(&self, local: &str) -> Mailbox {
        backends::acct_in(&self.b, self.domain, local).await
    }

    /// Open the mailbox of `account` again (a second handle over the same repository).
    pub async fn reopen(&self, account: AccountId) -> Result<Mailbox, rusty_core::Error> {
        Mailbox::open_with(self.b.mrepo.clone(), self.blobs.clone(), account).await
    }

    pub async fn scalar(&self, sql: &str) -> i64 {
        backends::scalar(&self.b, sql).await
    }

    pub async fn scalars(&self, sql: &str, binds: &[backends::Bind<'_>]) -> Vec<i64> {
        backends::fetch_i64s(&self.b, sql, binds).await
    }

    pub async fn strings(&self, sql: &str, binds: &[backends::Bind<'_>]) -> Vec<String> {
        backends::fetch_strings(&self.b, sql, binds).await
    }

    pub async fn opt_i64(&self, sql: &str, binds: &[backends::Bind<'_>]) -> Option<i64> {
        backends::fetch_opt_i64(&self.b, sql, binds).await
    }

    pub async fn exec(&self, sql: &str, binds: &[backends::Bind<'_>]) -> u64 {
        backends::execute(&self.b, sql, binds).await
    }

    /// The database clock (unix seconds).
    pub async fn db_clock(&self) -> i64 {
        backends::db_clock(&self.b).await
    }
}

/// Run `f` once per backend: Postgres, the only one (`RUSTY_TEST_PG_URL` is mandatory).
#[allow(clippy::await_holding_lock)] // the probe lock is deliberately held while an environment is built
pub async fn for_each_env<F, Fut>(f: F)
where
    F: Fn(Env) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let all = {
        let _g = stmts::setup_guard();
        backends::backends().await
    };
    for b in all {
        eprintln!("-- backend {}", b.name);
        let e = {
            let _g = stmts::setup_guard();
            Env::new(b).await
        };
        f(e).await;
    }
}

impl Env {
    /// SQLite over a private in-memory database (a single connection).
    pub async fn sqlite_mem() -> Env {
        Env::new(backends::sqlite_mem_backend().await).await
    }
}

// Same reason as `Backend`: with a Drop impl an `async move` block captures the whole `Env`
// (edition-2021 disjoint capture would otherwise leave the temp dir and database behind in the
// closure parameter, which is dropped at once).
impl Drop for Env {
    fn drop(&mut self) {}
}

impl Env {
    /// Rows of `cols` text cells (see [`backends::fetch_rows`]); NULL is the cell "NULL".
    pub async fn rows(&self, sql: &str, cols: usize) -> Vec<Vec<String>> {
        backends::fetch_rows(&self.b, sql, &[], cols).await
    }
}

/// Parse a text cell as an integer.
pub fn int(cell: &str) -> i64 {
    cell.parse()
        .unwrap_or_else(|_| panic!("not an integer: {cell}"))
}

/// Run `f` once per available backend with TWO independent environments of that backend.
pub async fn for_each_env_pair<F, Fut>(f: F)
where
    F: Fn(Env, Env) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    for first in backends::backends().await {
        let name = first.name;
        eprintln!("-- backend {name} (pair)");
        let second = backends::pg_backend().await.expect("postgres backend");
        f(Env::new(first).await, Env::new(second).await).await;
    }
}

impl Env {
    /// Run a statement the database is expected to reject (or accept) and report which.
    pub async fn try_exec(
        &self,
        sql: &str,
        binds: &[backends::Bind<'_>],
    ) -> Result<u64, backends::SqlFailure> {
        backends::try_execute(&self.b, sql, binds).await
    }
}
