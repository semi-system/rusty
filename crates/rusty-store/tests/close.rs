//! `close()`: a graceful shutdown ends the pool and, on SQLite, folds the write-ahead log into
//! the database file so that a copy of the one file is complete (backup and restart rely on it).

mod common;

use bytes::Bytes;
use common::backends::{acct, for_each_backend, Backend, INBOX};
use rusty_core::Error;
use rusty_core::Flags;

async fn close(b: &Backend) -> rusty_core::Result<()> {
    #[cfg(feature = "postgres")]
    if let Some((store, _)) = &b.pg {
        return store.close().await;
    }
    b.sqlite.as_ref().expect("a backend is SQLite or Postgres").close().await
}

#[tokio::test]
async fn close_ends_the_pool_and_checkpoints_the_wal() {
    for_each_backend(|b| async move {
        let (_dom, _acc, mb) = acct(&b, "closer").await;
        for i in 0..20 {
            let raw = format!("Subject: m{i}\r\n\r\nbody {i}\r\n");
            mb.append(INBOX, Bytes::from(raw), Flags::default()).await.unwrap();
        }
        close(&b).await.unwrap();
        // The pool is gone: further work fails closed as a storage error, not a panic or a hang.
        match mb.list_folders().await {
            Err(Error::Storage { .. }) | Err(Error::Busy(_)) => {}
            other => panic!("use after close: {other:?}"),
        }
        // Closing twice is harmless.
        close(&b).await.unwrap();
        if b.name == "sqlite" {
            let wal = b._tmp.path().join("meta.sqlite-wal");
            let len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
            assert_eq!(len, 0, "WAL not checkpointed on close");
        }
    })
    .await;
}
