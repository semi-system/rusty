//! One test per file: signal handlers are process-wide. SIGHUP reloads, a failed reload keeps the process
//! running, SIGTERM stops it.
mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::{send, until};
use rusty_core::Error;
use rusty_runtime::process::Signals;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn hup_reloads_a_failed_reload_keeps_running_and_term_stops() {
    let signals = Signals::claim().unwrap();
    let stop = CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let handle = signals.spawn(
        stop.clone(),
        Some(Box::new(move || {
            // The second reload fails (a bad certificate): the previous one stays and the process runs on.
            if c.fetch_add(1, Ordering::SeqCst) + 1 == 2 { Err(Error::Invalid("bad certificate".into())) } else { Ok(()) }
        })),
    );
    send("HUP");
    until("the first reload", || calls.load(Ordering::SeqCst) == 1).await;
    send("HUP");
    until("the second reload", || calls.load(Ordering::SeqCst) == 2).await;
    assert!(!stop.is_cancelled(), "a failed reload must not stop the process");
    send("HUP");
    until("the third reload", || calls.load(Ordering::SeqCst) == 3).await;
    send("TERM");
    tokio::time::timeout(Duration::from_secs(10), stop.cancelled()).await.expect("SIGTERM must cancel the token");
    handle.await.unwrap();
}
