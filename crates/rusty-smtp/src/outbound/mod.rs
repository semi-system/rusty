//! The outbound path (U5): the queue worker, the SMTP client, retry policy, bounces and TLS.
//!
//! Layers, as everywhere in this crate: pure and fuzzable (`smtp_reply`, `payload`, `policy`, `dsn`),
//! the conversation (`client`), injected I/O (`dial`, `tls`, DNS in `crate::resolve`, routing in
//! `route`), and the service that ties them to the durable queue (`worker`).

pub mod client;
pub mod dial;
pub mod dsn;
pub mod payload;
pub mod policy;
pub mod route;
pub mod smtp_reply;
pub mod tls;
pub mod worker;
