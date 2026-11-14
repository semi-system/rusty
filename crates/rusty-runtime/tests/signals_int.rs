mod common;

use std::time::Duration;

use common::send;
use rusty_runtime::process::Signals;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn int_stops_like_term() {
    let signals = Signals::claim().unwrap();
    let stop = CancellationToken::new();
    let handle = signals.spawn(stop.clone(), None);
    send("INT");
    tokio::time::timeout(Duration::from_secs(10), stop.cancelled()).await.expect("SIGINT must cancel the token");
    handle.await.unwrap();
}
