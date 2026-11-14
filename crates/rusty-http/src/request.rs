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

    /// Headers from `(name, value)` pairs (names are lower-cased): for tests and for handlers that build requests.
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Headers {
        Headers(pairs.into_iter().map(|(n, v)| (n.to_ascii_lowercase(), v.to_owned())).collect())
    }

    /// Every value of `name` (case-insensitive), in the order sent.
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.0.iter().filter(move |(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
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

    /// The value of cookie `name`, only when it is unambiguous: absent, malformed or sent twice (a planted
    /// second cookie of the same name, in one header or two) is `None`, never a guess.
    pub fn cookie(&self, name: &str) -> Option<&str> {
        let mut found = None;
        for header in self.headers.get_all("cookie") {
            for (n, v) in crate::cookie::parse_cookies(header) {
                if n == name {
                    if found.is_some() {
                        return None;
                    }
                    found = Some(v);
                }
            }
        }
        found
    }
}
