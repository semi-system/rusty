//! A streamed response (`Response::streaming`): the chunks reach the client as they are produced, the stream outlives the request timeout (which covers the handler
//! up to the response head only), it ends when the sender is dropped, and a client that goes away is noticed by the sender.
mod common;

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::*;
use rusty_http::{handler, Limits, Method, Request, Response, Router};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

/// `GET /stream`: sends `first`, then waits for the test's signal, then sends `second` and ends. The sender's fate is reported on `done`.
fn stream_router(go: Arc<tokio::sync::Mutex<Option<oneshot::Receiver<()>>>>, done: mpsc::UnboundedSender<bool>) -> Router {
    Router::new().route(Method::Get, "/stream", handler(move |_req: Request| {
        let (go, done) = (go.clone(), done.clone());
        async move {
            let (tx, rx) = mpsc::channel::<Bytes>(4);
            tokio::spawn(async move {
                let ok = tx.send(Bytes::from_static(b"event: resync\n\n")).await.is_ok();
                if let Some(signal) = go.lock().await.take() {
                    let _ = signal.await;
                }
                let ok = ok && tx.send(Bytes::from_static(b"event: changed\n\n")).await.is_ok();
                // Whether the client was still there for the last chunk.
                let _ = done.send(ok);
            });
            Response::streaming(200, rx).with_header("content-type", "text/event-stream")
        }
    }))
}

async fn read_until(s: &mut TcpStream, needle: &str, limit: Duration) -> String {
    let mut got = Vec::new();
    let mut buf = [0u8; 1024];
    let _ = tokio::time::timeout(limit, async {
        while !String::from_utf8_lossy(&got).contains(needle) {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
            }
        }
    })
    .await;
    String::from_utf8_lossy(&got).into_owned()
}

const REQ: &[u8] = b"GET /stream HTTP/1.1\r\nHost: t\r\n\r\n";

#[tokio::test]
async fn chunks_arrive_as_they_are_sent_and_the_stream_ends_with_its_sender() {
    let (go_tx, go_rx) = oneshot::channel();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let s = start_router(stream_router(Arc::new(tokio::sync::Mutex::new(Some(go_rx))), done_tx), Limits::default(), calls).await;
    let mut c = TcpStream::connect(s.addr).await.unwrap();
    c.write_all(REQ).await.unwrap();
    let head = read_until(&mut c, "event: resync", Duration::from_secs(3)).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.to_ascii_lowercase().contains("content-type: text/event-stream"), "{head}");
    assert!(head.to_ascii_lowercase().contains("transfer-encoding: chunked"), "{head}");
    assert!(head.to_ascii_lowercase().contains("connection: close"), "{head}");
    assert!(head.to_ascii_lowercase().contains("content-security-policy"), "the secure headers are on a stream too: {head}");
    assert!(!head.contains("event: changed"), "the second chunk was not sent yet: {head}");
    go_tx.send(()).unwrap();
    let rest = read_until(&mut c, "event: changed", Duration::from_secs(3)).await;
    assert!(rest.contains("event: changed"), "{rest}");
    assert_eq!(done_rx.recv().await, Some(true));
    // The sender is gone: the response ends and the server closes the connection.
    let mut tail = Vec::new();
    let ended = tokio::time::timeout(Duration::from_secs(3), c.read_to_end(&mut tail)).await;
    assert!(ended.is_ok(), "the stream did not end with its sender");
    s.shutdown().await;
}

#[tokio::test]
async fn a_stream_outlives_the_request_timeout() {
    let (go_tx, go_rx) = oneshot::channel();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel();
    let calls = Arc::new(AtomicUsize::new(0));
    // The request timeout is 600 ms: the handler returned at once, so the stream may take longer.
    let s = start_router(stream_router(Arc::new(tokio::sync::Mutex::new(Some(go_rx))), done_tx), small_limits(), calls).await;
    let mut c = TcpStream::connect(s.addr).await.unwrap();
    c.write_all(REQ).await.unwrap();
    read_until(&mut c, "event: resync", Duration::from_secs(3)).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    go_tx.send(()).unwrap();
    let rest = read_until(&mut c, "event: changed", Duration::from_secs(3)).await;
    assert!(rest.contains("event: changed"), "the stream was cut at the request timeout: {rest}");
    assert_eq!(done_rx.recv().await, Some(true));
    s.shutdown().await;
}

#[tokio::test]
async fn a_client_that_goes_away_is_noticed_by_the_sender() {
    let (go_tx, go_rx) = oneshot::channel();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let s = start_router(stream_router(Arc::new(tokio::sync::Mutex::new(Some(go_rx))), done_tx), Limits::default(), calls).await;
    let mut c = TcpStream::connect(s.addr).await.unwrap();
    c.write_all(REQ).await.unwrap();
    read_until(&mut c, "event: resync", Duration::from_secs(3)).await;
    drop(c);
    // Give the server a moment to see the close, then let the sender try again: it must fail, not wait for ever.
    tokio::time::sleep(Duration::from_millis(300)).await;
    go_tx.send(()).unwrap();
    assert_eq!(tokio::time::timeout(Duration::from_secs(3), done_rx.recv()).await.unwrap(), Some(false), "the sender kept sending to a closed connection");
    s.shutdown().await;
}
