//! `close()`: a graceful shutdown ends the pool; later use fails closed and closing twice is harmless.

mod common;

use bytes::Bytes;
use common::backends::{acct, for_each_backend, Backend, INBOX};
use rusty_core::Error;
use rusty_core::Flags;

async fn close(b: &Backend) -> rusty_core::Result<()> {
    let (store, _) = b.pg.as_ref().expect("the backend is Postgres");
    store.close().await;
    Ok(())
}

#[tokio::test]
async fn close_ends_the_pool() {
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
    })
    .await;
}
