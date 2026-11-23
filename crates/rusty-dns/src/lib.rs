//! The authoritative DNS server for the hosted domains (docs/design-authoritative-dns.md). This crate is built from the inside out: first the pure answer engine (what a name server says to a question, with no
//! network and no wire format), then the codec and the listeners, then the zones from the database.
#![forbid(unsafe_code)]

pub mod adapter;
pub mod answer;
pub mod build;
pub mod limit;
pub mod server;
pub mod zone;
