//! The response a handler returns.

use bytes::Bytes;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Response {
    pub fn new(status: u16) -> Response {
        Response { status, headers: Vec::new(), body: Bytes::new() }
    }

    /// Add a header. A name or value that is not valid for HTTP makes the server answer 500 instead of this response.
    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Response {
        self.headers.push((name.to_ascii_lowercase(), value.into()));
        self
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
