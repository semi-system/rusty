//! The `rusty` multi-call binary as a library: `rusty d` / `rustyd` (the all-in-one daemon) and
//! `rusty ctl` / `rustyctl` (administration, arrives with U2). This crate is only the composition
//! root: it parses the command line, opens the `Store`, builds the `Context` from `(config, trait
//! objects)` and runs the services of `rusty-svc` and the protocol crates. All service logic lives
//! in those crates, so a multi-daemon deployment is another composition root over the same code.
#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

pub mod cli;
pub mod ctl;
pub mod daemon;
pub mod storecfg;
pub mod health;
