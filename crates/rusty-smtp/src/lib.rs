//! SMTP server logic for Rusty. Three layers, kept apart so each can be fuzzed and moved:
//!
//! * [`codec`]: bytes to typed values (line framing, command parsing, DATA framing). No I/O and no
//!   state beyond the reader buffers. Command grammar comes from a wrapped library behind `adapter`; framing is ours.
//! * [`session`]: the sans-I/O state machine. Bytes and answers in, actions out. No sockets, no
//!   clock, no database; the driver answers the asynchronous questions (is this recipient local?
//!   store this message).
//! * [`inbound`]: the I/O driver (port 25). It owns the stream, the timeouts, STARTTLS, and talks to
//!   storage only through the `Directory` and `Mailbox` facades held by `rusty_svc::Context`.
//!
//! [`message`] builds what is stored (trace headers, line endings, delivery ids), [`reply`] is the
//! one table from store errors to SMTP statuses, and [`filter`] is the seam for spam, virus and
//! authentication checks. Submission (587/465) and the outbound queue reuse these layers.
#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

mod adapter;
pub mod authres;
pub mod codec;
pub mod dkim;
pub mod filter;
pub mod fuzzing;
pub mod inbound;
pub mod limits;
pub mod message;
pub mod outbound;
pub mod outbound_queue;
pub mod reply;
pub mod resolve;
mod rsagen;
pub mod session;
pub mod stack;
