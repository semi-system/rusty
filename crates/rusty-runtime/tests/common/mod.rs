#![allow(dead_code)]
//! Signals sent to this very process, as an operator's `kill` would.

use std::time::{Duration, Instant};

pub fn send(sig: &str) {
    let st = std::process::Command::new("kill").args([&format!("-{sig}"), &std::process::id().to_string()]).status().unwrap();
    assert!(st.success(), "kill -{sig} failed");
}

pub async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !ok() {
        assert!(t0.elapsed() < Duration::from_secs(10), "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
