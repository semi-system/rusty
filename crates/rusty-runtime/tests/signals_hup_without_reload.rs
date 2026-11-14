mod common;

use std::time::Duration;

use common::send;
use rusty_runtime::process::Signals;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn hup_without_a_reload_is_ignored_not_fatal() {
    // A service without TLS has nothing to reload; the default action of SIGHUP (kill the process) must not apply.
    let signals = Signals::claim().unwrap();
    let stop = CancellationToken::new();
    let handle = signals.spawn(stop.clone(), None);
    send("HUP");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!stop.is_cancelled());
    send("TERM");
    tokio::time::timeout(Duration::from_secs(10), stop.cancelled()).await.expect("SIGTERM must still stop it");
    handle.await.unwrap();
}
