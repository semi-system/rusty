//! POP3 for the IMAP service (docs/design-pop3.md). Three layers, kept apart so each can be tested and fuzzed:
//!
//! * [`command`]: one line to one command, strictly, with no state and no I/O;
//! * [`session`]: the protocol state machine. It owns no socket and reads no clock; it reaches the password check and the maildrop only through the [`session::Backend`] it is given;
//! * [`driver`]: the I/O (lines, limits, `STLS`) and the backend over the real directory and mailbox.
//!
//! The public surface is [`Pop3Handler`] (a `rusty_svc::SessionHandler`).
#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

pub mod command;
pub mod driver;
pub mod fuzz_checks;
pub mod session;

pub use driver::{Limits, Pop3Handler, Port};
