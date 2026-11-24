//! The calendar and contacts service (docs/design-caldav-carddav.md) as a process: HTTP behind the edge, Basic authentication with the own or an app password, and the methods of `rusty-dav` over the store.
//! The binary `rusty-dav` is a thin `main` over [`run::start`].
#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

pub mod auth;
pub mod handler;
pub mod health;
pub mod run;
