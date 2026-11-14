//! The HTTP server as a Rusty-owned seam, for the admin web now and the webmail later.
//!
//! The third-party HTTP library is named only inside `adapter` (the wrapped-libraries gate); everything
//! else here is our own small types: [`Request`], [`Response`], [`Router`], [`Limits`] and the
//! [`HttpHandler`] that plugs into the same listener loop the mail services use (`SessionHandler`), so TLS,
//! the connection caps, the per-address cap and the graceful drain are not reimplemented.
//!
//! What this layer guarantees for every handler:
//! * the request line, the head and the body are bounded, and so is the time each takes;
//! * the path is checked (no empty segment, no dot segment, no encoded slash, backslash or control
//!   character, valid UTF-8) before any route is chosen, and a refused request never reaches a handler;
//! * every response, including the ones this layer produces itself, carries the security headers.
#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

mod adapter;
mod config;
mod request;
mod response;
mod router;
mod server;

pub use config::{HttpConfig, Limits, SECURITY_HEADERS};
pub use request::{Headers, Method, Params, Request};
pub use response::Response;
pub use router::{handler, Handler, Router};
pub use server::HttpHandler;
