//! The cap on requests in flight: a burst of puts never has more than the limit at the server at once, every one of them still completes, and a request that cannot get a turn
//! within the deadline fails as a timeout instead of waiting for ever.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rusty_s3::{Credentials, DirectConnector, S3Client, S3Config, S3Fail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A server that answers every request `200` after `delay`, counting how many are being served at once.
async fn slow_server(delay: Duration) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peak = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let p = peak.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (peak, active) = (p.clone(), active.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let mut seen = 0;
                // Read until the head and the whole body (Content-Length) have arrived.
                loop {
                    let Ok(n) = sock.read(&mut buf[seen..]).await else { return };
                    if n == 0 {
                        return;
                    }
                    seen += n;
                    let text = String::from_utf8_lossy(&buf[..seen]).into_owned();
                    if let Some(head_end) = text.find("\r\n\r\n") {
                        let len = text.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                        if seen >= head_end + 4 + len {
                            break;
                        }
                    }
                }
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(delay).await;
                active.fetch_sub(1, Ordering::SeqCst);
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            });
        }
    });
    (format!("http://{addr}"), peak)
}

fn client(endpoint: &str, deadline: Duration) -> S3Client {
    let cfg = S3Config { endpoint: endpoint.into(), region: "us-east-1".into(), bucket: "bucket".into(), prefix: String::new(), path_style: true, deadline };
    S3Client::new(cfg, Credentials { access_key: "AKIAEXAMPLEKEY".into(), secret_key: "secret".into() }, Arc::new(DirectConnector { timeout: Duration::from_secs(3) }), None).unwrap()
}

#[tokio::test]
async fn requests_in_flight_never_exceed_the_cap_and_all_complete() {
    let (endpoint, peak) = slow_server(Duration::from_millis(80)).await;
    let c = Arc::new(client(&endpoint, Duration::from_secs(20)).with_max_concurrent(3));
    let mut tasks = Vec::new();
    for i in 0..15 {
        let c = c.clone();
        tasks.push(tokio::spawn(async move { c.put(&format!("k/{i}"), Bytes::from(vec![1u8; 10])).await }));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert!(peak.load(Ordering::SeqCst) <= 3, "never more than the cap at the server: {}", peak.load(Ordering::SeqCst));
    assert!(peak.load(Ordering::SeqCst) >= 2, "the cap is used, not one at a time: {}", peak.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_request_that_waits_longer_than_the_deadline_for_its_turn_is_a_timeout() {
    let (endpoint, _) = slow_server(Duration::from_millis(1500)).await;
    let c = Arc::new(client(&endpoint, Duration::from_millis(400)).with_max_concurrent(1));
    let first = {
        let c = c.clone();
        tokio::spawn(async move { c.put("a", Bytes::from_static(b"x")).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    // The only permit is held by the slow request (which itself times out at the deadline); the second waits for it and is refused as a timeout.
    let second = c.put("b", Bytes::from_static(b"x")).await;
    assert_eq!(second.unwrap_err(), S3Fail::Timeout);
    let _ = first.await;
}
