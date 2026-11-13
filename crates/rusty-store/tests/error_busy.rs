//! D1: transient failures are `Error::Busy`, never `Storage`, never `Conflict`; the text is a
//! fixed string (no URL, no SQL, no identifiers).

mod common;

use rusty_core::Error;
use rusty_store::map_db_error;

/// Every `Busy` message must be short, fixed and free of anything that looks like a URL,
/// SQL or a database identifier.
fn assert_fixed_busy(e: &Error) {
    let Error::Busy(msg) = e else {
        panic!("expected Busy, got {e:?}");
    };
    assert!(msg.len() < 64, "{msg}");
    for needle in ["postgres", "127.0.0.1", "SELECT", "UPDATE", "mailboxes", "rusty_test", "@"] {
        assert!(!msg.contains(needle), "Busy text leaks {needle}: {msg}");
    }
    assert!(std::error::Error::source(e).is_none(), "Busy carries no source");
}

// backend-neutral: error mapping per backend: the pg module covers Postgres, the rest are pure unit tests
#[test]
fn pool_timeout_and_closed_are_busy_other_io_is_storage() {
    assert_fixed_busy(&map_db_error(sqlx::Error::PoolTimedOut));
    assert_fixed_busy(&map_db_error(sqlx::Error::PoolClosed));
    let io = sqlx::Error::Io(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset"));
    assert!(matches!(map_db_error(io), Error::Storage { .. }));
}

#[cfg(feature = "postgres")]
mod pg {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use common::pg::{pg_admin_url, PgTestDb};
    use rusty_core::{AccountId, MailboxId};
    use rusty_store::repo::pg::PgRepo;
    use rusty_store::repo::{DirectoryRepo, MailboxRepo, NewAccount};
    use rusty_store::{PgLimits, PgStore};
    use sqlx::postgres::PgPoolOptions;

    async fn account(repo: &PgRepo, tag: &str) -> (AccountId, MailboxId) {
        let dom = repo.create_domain(&format!("{tag}.test")).await.unwrap();
        let acc = repo
            .create_account(&NewAccount {
                local: "alice".into(),
                domain: dom,
                plan: None,
                password_hash: None,
            })
            .await
            .unwrap();
        let mb = repo.mailbox_for_account(acc).await.unwrap();
        (acc, mb)
    }

    /// Hold the mailbox row lock on a separate connection until the returned tx is dropped.
    async fn hold_mailbox_lock(
        url: &str,
        mb: MailboxId,
    ) -> sqlx::Transaction<'static, sqlx::Postgres> {
        let pool = PgPoolOptions::new().max_connections(1).connect(url).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM mailboxes WHERE id = $1 FOR UPDATE")
            .bind(mb.0)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx
    }

    // backend-neutral: error mapping per backend: the pg module covers Postgres, the rest are pure unit tests
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn lock_timeout_on_held_mailbox_lock_is_busy() {
        let Some(admin) = pg_admin_url() else { return };
        let db = PgTestDb::create(&admin).await;
        let setup = PgRepo::new(db.connect().await);
        let (_, mb) = account(&setup, "lt").await;
        let store = PgStore::connect_with_config(
            db.url(),
            PgLimits {
                lock_timeout: Duration::from_millis(200),
                ..PgLimits::default()
            },
        )
        .await
        .unwrap();
        let repo = PgRepo::new(store);
        let held = hold_mailbox_lock(db.url(), mb).await;
        let err = repo.create_folder(mb, None, "Projects").await.unwrap_err();
        assert_fixed_busy(&err);
        assert_eq!(repo.stats().retries.load(std::sync::atomic::Ordering::Relaxed), 0);
        drop(held);
        // After the holder is gone the same call works.
        repo.create_folder(mb, None, "Projects").await.unwrap();
        db.cleanup().await;
    }

    /// D6 (transaction pooling): `lock_timeout` is transaction-local. Inside the write
    /// transaction it has the configured value; once the transaction ended, the same (only)
    /// pooled connection reports the server default, so nothing leaks to the next borrower.
    // backend-neutral: error mapping per backend: the pg module covers Postgres, the rest are pure unit tests
    #[tokio::test]
    async fn lock_timeout_is_transaction_local() {
        let Some(admin) = pg_admin_url() else { return };
        let db = PgTestDb::create(&admin).await;
        let store = PgStore::connect_with_config(
            db.url(),
            PgLimits {
                max_connections: 1,
                lock_timeout: Duration::from_millis(2345),
                ..PgLimits::default()
            },
        )
        .await
        .unwrap();
        let mut conn = store.pool().acquire().await.unwrap();
        let before: String = sqlx::query_scalar("SHOW lock_timeout").fetch_one(&mut *conn).await.unwrap();
        drop(conn);

        for finish in ["commit", "rollback"] {
            let mut tx = store.begin_write().await.unwrap();
            let inside: String = sqlx::query_scalar("SHOW lock_timeout").fetch_one(&mut *tx).await.unwrap();
            assert_eq!(inside, "2345ms", "the configured timeout applies inside the transaction");
            if finish == "commit" {
                tx.commit().await.unwrap();
            } else {
                tx.rollback().await.unwrap();
            }
            // max_connections = 1: this is the very connection the transaction used.
            let mut conn = store.pool().acquire().await.unwrap();
            let after: String = sqlx::query_scalar("SHOW lock_timeout").fetch_one(&mut *conn).await.unwrap();
            assert_eq!(after, before, "lock_timeout leaked out of the transaction ({finish})");
            assert_ne!(after, "2345ms");
        }
        store.close().await;
        db.cleanup().await;
    }

    // backend-neutral: error mapping per backend: the pg module covers Postgres, the rest are pure unit tests
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn statement_timeout_is_busy() {
        let Some(admin) = pg_admin_url() else { return };
        let db = PgTestDb::create(&admin).await;
        let setup = PgRepo::new(db.connect().await);
        let (_, mb) = account(&setup, "st").await;
        // The statement timeout is a property of the role/database now, not of the session.
        db.set_database_setting("statement_timeout", "150ms").await;
        let store = PgStore::connect_with_config(
            db.url(),
            PgLimits {
                lock_timeout: Duration::from_secs(30),
                ..PgLimits::default()
            },
        )
        .await
        .unwrap();
        let repo = PgRepo::new(store);
        let _held = hold_mailbox_lock(db.url(), mb).await;
        let err = repo.create_folder(mb, None, "Projects").await.unwrap_err();
        assert_fixed_busy(&err);
        db.cleanup().await;
    }

    // backend-neutral: error mapping per backend: the pg module covers Postgres, the rest are pure unit tests
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pool_exhausted_is_busy_with_fixed_message() {
        let Some(admin) = pg_admin_url() else { return };
        let db = PgTestDb::create(&admin).await;
        let store = PgStore::connect_with_config(
            db.url(),
            PgLimits {
                max_connections: 1,
                acquire_timeout: Duration::from_millis(200),
                ..PgLimits::default()
            },
        )
        .await
        .unwrap();
        let repo = PgRepo::new(store.clone());
        let held = store.pool().acquire().await.unwrap();
        let err = repo.create_domain("pool.test").await.unwrap_err();
        assert_fixed_busy(&err);
        let err = repo
            .create_account(&NewAccount {
                local: "a".into(),
                domain: rusty_core::DomainId(1),
                plan: None,
                password_hash: None,
            })
            .await
            .unwrap_err();
        assert_fixed_busy(&err);
        let err = store.begin_write().await.unwrap_err();
        assert_fixed_busy(&err);
        drop(held);
        repo.create_domain("pool.test").await.unwrap();
        db.cleanup().await;
    }

    // backend-neutral: error mapping per backend: the pg module covers Postgres, the rest are pure unit tests
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn closed_pool_is_busy() {
        let Some(admin) = pg_admin_url() else { return };
        let db = PgTestDb::create(&admin).await;
        let store = db.connect().await;
        let repo = PgRepo::new(store.clone());
        store.close().await;
        assert_fixed_busy(&repo.create_domain("closed.test").await.unwrap_err());
        db.cleanup().await;
    }

    // backend-neutral: error mapping per backend: the pg module covers Postgres, the rest are pure unit tests
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn five_forced_deadlocks_end_in_busy_and_count_exhausted() {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(admin) = pg_admin_url() else { return };
        let db = PgTestDb::create(&admin).await;
        let store = db.connect().await;
        let repo = PgRepo::new(store.clone());
        let dom = repo.create_domain("dl.test").await.unwrap();
        let new = NewAccount {
            local: "alice".into(),
            domain: dom,
            plan: None,
            password_hash: None,
        };

        repo.inject_deadlocks_for_test(5);
        let err = repo.create_account(&new).await.unwrap_err();
        assert_fixed_busy(&err);
        let stats = repo.stats();
        assert_eq!(stats.exhausted.load(Relaxed), 1);
        assert_eq!(stats.deadlocks.load(Relaxed), 5);
        // No partial state: the account does not exist, and the next attempt succeeds.
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(n, 0);
        repo.create_account(&new).await.unwrap();

        // Four deadlocks then success: retried, not Busy.
        repo.inject_deadlocks_for_test(4);
        let ok = repo
            .create_account(&NewAccount {
                local: "bob".into(),
                ..new.clone()
            })
            .await;
        assert!(ok.is_ok(), "{ok:?}");
        assert_eq!(repo.stats().exhausted.load(Relaxed), 1);
        db.cleanup().await;
    }

    async fn raise(pool: &sqlx::PgPool, code: &str) -> sqlx::Error {
        // `code` is a test constant (5 characters of [A-Z0-9]).
        sqlx::query(&format!(
            "DO $$ BEGIN RAISE EXCEPTION 'boom' USING ERRCODE = '{code}'; END $$"
        ))
        .execute(pool)
        .await
        .unwrap_err()
    }

    // backend-neutral: error mapping per backend: the pg module covers Postgres, the rest are pure unit tests
    #[tokio::test]
    async fn sqlstates_map_by_table_and_unknown_codes_are_storage() {
        let Some(admin) = pg_admin_url() else { return };
        let db = PgTestDb::create(&admin).await;
        let store = db.connect().await;
        let p = store.pool();
        assert!(matches!(map_db_error(raise(p, "RS001").await), Error::Conflict(_)));
        assert!(matches!(map_db_error(raise(p, "RS002").await), Error::Invalid(_)));
        // The registry guard and any code we do not know are Storage, NEVER Conflict.
        for code in ["RS003", "RS999", "XX000", "42P01", "22012"] {
            let mapped = map_db_error(raise(p, code).await);
            assert!(matches!(mapped, Error::Storage { .. }), "{code} -> {mapped:?}");
        }
        for code in ["55P03", "57014", "53300", "40P01", "40001"] {
            assert_fixed_busy(&map_db_error(raise(p, code).await));
        }
        db.cleanup().await;
    }

    // backend-neutral: error mapping per backend: the pg module covers Postgres, the rest are pure unit tests
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn connection_reset_mid_transaction_is_not_retried() {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(admin) = pg_admin_url() else { return };
        let db = PgTestDb::create(&admin).await;
        let setup = PgRepo::new(db.connect().await);
        let (_, mb) = account(&setup, "rst").await;
        let store = PgStore::connect_with_config(
            db.url(),
            PgLimits {
                lock_timeout: Duration::from_secs(30),
                ..PgLimits::default()
            },
        )
        .await
        .unwrap();
        let repo = Arc::new(PgRepo::new(store));
        let held = hold_mailbox_lock(db.url(), mb).await;

        let r2 = repo.clone();
        let op = tokio::spawn(async move { r2.create_folder(mb, None, "Projects").await });
        // Wait until the repository connection is blocked on the lock, then kill its backend.
        let admin_pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(db.url())
            .await
            .unwrap();
        let mut killed = 0i64;
        for _ in 0..100 {
            killed = sqlx::query_scalar(
                "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity \
                 WHERE datname = current_database() AND application_name = 'rusty-store' \
                   AND wait_event_type = 'Lock'",
            )
            .fetch_one(&admin_pool)
            .await
            .unwrap();
            if killed > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(killed > 0, "never saw the blocked repository connection");
        let err = op.await.unwrap().unwrap_err();
        assert!(
            matches!(err, Error::Storage { .. } | Error::Busy(_)),
            "{err:?}"
        );
        assert_eq!(repo.stats().retries.load(Relaxed), 0, "a reset must never be retried");
        drop(held);
        db.cleanup().await;
    }

    /// SQLSTATEs the schema can raise for values it cannot store or a primary that cannot write,
    /// each produced by a real statement and judged through the production mapping (review PG6
    /// finding 4): 54000 (index row too large), 22021 (NUL in text), 22003 (numeric overflow) and
    /// 22001 are `Invalid`, never `Storage`; 25006 (a write on a read-only server, as after a
    /// failover to a standby) is `Busy`: the statement did not happen and a retry reaches the new
    /// primary, which is what `Busy` promises.
    // backend-neutral: error mapping per backend: the pg module covers Postgres, the rest are pure unit tests
    #[tokio::test]
    async fn unstorable_values_and_a_read_only_server_map_by_sqlstate() {
        let Some(admin) = pg_admin_url() else { return };
        let db = PgTestDb::create(&admin).await;
        let store = db.connect().await;
        let p = store.pool();
        let code = |e: &sqlx::Error| e.as_database_error().unwrap().code().map(|c| c.to_string());

        // 54000: a 4000-character incompressible name under the unique btree index.
        let too_big = sqlx::query(
            "INSERT INTO domains(name, created_at) \
             VALUES ((SELECT string_agg(md5(i::text), '') FROM generate_series(1, 125) i), 0)",
        )
        .execute(p)
        .await
        .unwrap_err();
        assert_eq!(code(&too_big).as_deref(), Some("54000"));
        assert!(matches!(map_db_error(too_big), Error::Invalid(_)));

        // 22021: a NUL byte in a text parameter.
        let nul = sqlx::query("INSERT INTO domains(name, created_at) VALUES ($1, 0)")
            .bind("a\0b")
            .execute(p)
            .await
            .unwrap_err();
        assert_eq!(code(&nul).as_deref(), Some("22021"));
        assert!(matches!(map_db_error(nul), Error::Invalid(_)));

        // 22003: a counter that overflows bigint.
        let overflow = sqlx::query("SELECT 9223372036854775807::bigint + 1")
            .execute(p)
            .await
            .unwrap_err();
        assert_eq!(code(&overflow).as_deref(), Some("22003"));
        assert!(matches!(map_db_error(overflow), Error::Invalid(_)));

        // 22001: a value too long for a sized column.
        let mut conn = p.acquire().await.unwrap();
        sqlx::query("CREATE TEMP TABLE narrow(a varchar(2))").execute(&mut *conn).await.unwrap();
        let truncated = sqlx::query("INSERT INTO narrow VALUES ('abc')").execute(&mut *conn).await.unwrap_err();
        assert_eq!(code(&truncated).as_deref(), Some("22001"));
        assert!(matches!(map_db_error(truncated), Error::Invalid(_)));
        drop(conn);

        // 25006: a write inside a read-only transaction.
        let mut tx = p.begin().await.unwrap();
        sqlx::query("SET TRANSACTION READ ONLY").execute(&mut *tx).await.unwrap();
        let ro = sqlx::query("INSERT INTO domains(name, created_at) VALUES ('ro.test', 0)")
            .execute(&mut *tx)
            .await
            .unwrap_err();
        assert_eq!(code(&ro).as_deref(), Some("25006"));
        assert_fixed_busy(&map_db_error(ro));
        drop(tx);

        store.close().await;
        db.cleanup().await;
    }
}
