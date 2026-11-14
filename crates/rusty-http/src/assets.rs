//! The front end served from memory: files that were compiled into the binary, so nothing on the server's disk
//! can be edited to change the interface and no request reads the file system.
//!
//! * `/assets/...` (content-hashed names) are `immutable` for a year; every other file, and the index page that
//!   answers `/` and every client-side route, is `no-cache` (always revalidated by its `ETag`).
//! * An `/api/` path is never answered with the index page (a mistyped API call is a clean `404` for the client), and
//!   neither is a path that looks like a file (it has an extension) and does not exist.
//! * Only files of a known type can be added: there is no default `application/octet-stream` for a browser to sniff.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::request::Request;
use crate::response::Response;
use crate::router::{check_path, Handler};

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const REVALIDATE: &str = "no-cache";

/// One embedded file.
#[derive(Debug, Clone)]
pub struct Asset {
    path: String,
    content_type: &'static str,
    bytes: &'static [u8],
}

fn content_type_of(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next()?;
    let ext = name.rsplit_once('.')?.1;
    // A name that is only an extension (".env") has an empty stem and is not a file we serve.
    if name.starts_with('.') && !name.get(1..).is_some_and(|s| s.contains('.')) {
        return None;
    }
    Some(match ext {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => return None,
    })
}

impl Asset {
    /// `path` is the URL path (`/assets/app.3f2a1c.js`): it must pass the same check as a request path and end in a
    /// known extension. `None` otherwise.
    pub fn new(path: &str, bytes: &'static [u8]) -> Option<Asset> {
        check_path(path)?;
        if path == "/" {
            return None;
        }
        Some(Asset { path: path.to_owned(), content_type: content_type_of(path)?, bytes })
    }
}

struct Entry {
    asset: Asset,
    etag: String,
    cache: &'static str,
}

pub struct Assets {
    files: HashMap<String, Entry>,
}

impl Assets {
    /// The files, with their `ETag`s (a hash of the content) computed once. The `/index.html` file, when present, also
    /// answers `/` and every client-side route.
    pub fn new(files: Vec<Asset>) -> Assets {
        let files = files
            .into_iter()
            .map(|asset| {
                let etag = format!("\"{}\"", &hex(&Sha256::digest(asset.bytes))[..32]);
                let cache = if asset.path.starts_with("/assets/") { IMMUTABLE } else { REVALIDATE };
                (asset.path.clone(), Entry { asset, etag, cache })
            })
            .collect();
        Assets { files }
    }

    /// The handler to install with `Router::fallback`.
    pub fn handler(self) -> Arc<dyn Handler> {
        Arc::new(AssetHandler(self))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct AssetHandler(Assets);

/// `If-None-Match` matches `etag` (weak validators compare equal for a GET) or is `*`.
fn not_modified(req: &Request, etag: &str) -> bool {
    req.headers.get_all("if-none-match").any(|v| v.split(',').map(str::trim).any(|c| c == "*" || c.strip_prefix("W/").unwrap_or(c) == etag))
}

#[async_trait]
impl Handler for AssetHandler {
    async fn call(&self, req: Request) -> Response {
        let path = req.path.as_str();
        let entry = match self.0.files.get(path) {
            Some(e) => e,
            None => {
                let last = path.rsplit('/').next().unwrap_or_default();
                let api = path == "/api" || path.starts_with("/api/");
                let fileish = last.contains('.');
                match (api, fileish, self.0.files.get("/index.html")) {
                    (false, false, Some(index)) => index,
                    _ => return Response::error(404, "not_found"),
                }
            }
        };
        let base = Response::new(200).with_header("etag", entry.etag.as_str()).with_header("cache-control", entry.cache);
        if not_modified(&req, &entry.etag) {
            return Response::new(304).with_header("etag", entry.etag.as_str()).with_header("cache-control", entry.cache);
        }
        base.with_header("content-type", entry.asset.content_type).with_body(Bytes::from_static(entry.asset.bytes))
    }
}
