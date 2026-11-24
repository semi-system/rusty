//! ManageSieve (RFC 5804) for the IMAP service (docs/design-sieve.md): a mail client uploads and activates the Sieve scripts of the account. Two layers, kept apart so each can be tested and fuzzed:
//!
//! * [`command`]: bytes to one command (quoted strings, `{n+}` literals, numbers), strictly, with no state and no I/O;
//! * [`driver`]: the protocol and its I/O (limits, `STARTTLS`, `AUTHENTICATE PLAIN` through the directory, the scripts through the repository).
//!
//! The public surface is [`ManageSieveHandler`] (a `rusty_svc::SessionHandler`).
#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

pub mod command;
pub mod driver;
pub mod fuzz_checks;

pub use driver::{Limits, ManageSieveHandler};
