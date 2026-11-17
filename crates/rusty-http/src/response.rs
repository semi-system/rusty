//! The response a handler returns.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use serde::Serialize;
use tokio::sync::mpsc;

/// The chunks of a streamed body (see [`Response::streaming`]). Taken once, by the connection that sends the response.
#[derive(Clone)]
pub(crate) struct Stream(Arc<Mutex<Option<mpsc::Receiver<Bytes>>>>);

impl Stream {
    pub(crate) fn take(&self) -> Option<mpsc::Receiver<Bytes>> {
        self.0.lock().ok().and_then(|mut g| g.take())
    }
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Stream")
    }
}

impl PartialEq for Stream {
    fn eq(&self, other: &Stream) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Stream {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub body: Bytes,
    /// `Some`: the body is what arrives on this channel, sent as it arrives; `body` is empty.
    pub(crate) stream: Option<Stream>,
}

impl Response {
    pub fn new(status: u16) -> Response {
        Response { status, headers: Vec::new(), body: Bytes::new(), stream: None }
    }

    /// Add a header. A name or value that is not valid for HTTP makes the server answer 500 instead of this response.
    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Response {
        self.headers.push((name.to_ascii_lowercase(), value.into()));
        self
    }

    /// A response whose body is sent chunk by chunk as the chunks arrive on `chunks`, until the sender is dropped. The request timeout covers the handler up to this
    /// response, not the stream: whoever feeds the channel ends it (and notices a client that went away, because the send then fails). The connection is closed afterwards.
    pub fn streaming(status: u16, chunks: mpsc::Receiver<Bytes>) -> Response {
        let mut r = Response::new(status);
        r.stream = Some(Stream(Arc::new(Mutex::new(Some(chunks)))));
        r
    }

    pub fn with_body(mut self, body: impl Into<Bytes>) -> Response {
        self.body = body.into();
        self
    }

    pub fn text(status: u16, body: &str) -> Response {
        Response::new(status).with_header("content-type", "text/plain; charset=utf-8").with_body(Bytes::copy_from_slice(body.as_bytes()))
    }

    pub fn json<T: Serialize>(status: u16, value: &T) -> Response {
        match serde_json::to_vec(value) {
            Ok(v) => Response::new(status).with_header("content-type", "application/json").with_body(v),
            Err(_) => Response::error(500, "internal"),
        }
    }

    /// `{"error":"<code>"}`: a fixed code and nothing else, never backend text.
    pub fn error(status: u16, code: &'static str) -> Response {
        Response::new(status)
            .with_header("content-type", "application/json")
            .with_body(Bytes::from(format!("{{\"error\":\"{code}\"}}")))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}
