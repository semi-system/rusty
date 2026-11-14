//! What every Rusty service process needs besides its own service logic, extracted from the all-in-one
//! `rusty` crate so that each service can be its own thin binary (image design, section 3):
//!
//! * [`store`]: from the `[store]` section to a `StoreConfig` (the one place that reads the database
//!   password and the CA bundle and applies the runtime-role policy);
//! * [`process`]: the process lifecycle shared by every daemon: claim the termination signals before
//!   anything starts, reload the TLS certificate on SIGHUP, stop on SIGTERM or SIGINT, and turn the end of
//!   the services into an exit code.
//!
//! No service logic and no binary live here.
#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

pub mod process;
pub mod store;
