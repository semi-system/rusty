//! The request a handler sees: Rusty-owned types only.

use bytes::Bytes;
use rusty_svc::PeerInfo;

/// The methods the server serves. Anything else is refused before routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
        }
    }
}

/// Request headers: names lower-case, values visible ASCII (a header with any other byte is dropped before it gets here).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    pub(crate) fn push(&mut self, name: String, value: String) {
        self.0.push((name, value));
    }

    /// The first value of `name` (case-insensitive).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// How many headers named `name` were sent.
    pub fn count(&self, name: &str) -> usize {
        self.0.iter().filter(|(n, _)| n.eq_ignore_ascii_case(name)).count()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }
}

/// The path parameters of the matched route, percent-decoded once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Params(pub(crate) Vec<(String, String)>);

impl Params {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    /// The path as sent (not decoded; already checked).
    pub path: String,
    pub query: Option<String>,
    pub headers: Headers,
    pub body: Bytes,
    pub peer: PeerInfo,
    pub params: Params,
}

impl Request {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.get(name)
    }
}
