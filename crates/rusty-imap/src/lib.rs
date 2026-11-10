//! IMAP4rev1 server (U6/U7). Three layers, kept apart so each can be fuzzed and moved:
//!
//! * codec/parser layer (no I/O, no state across calls): [`frame`] (bytes to whole commands with
//!   size caps), [`wire`] (response writers), [`utf7`], [`seqset`], [`structure`] (MIME tree,
//!   ENVELOPE, BODYSTRUCTURE, sections), [`search`] (key evaluation). Command grammar is
//!   the grammar library 1.0.0.
//! * session layer: [`session`] is a state machine that takes decoded commands and returns bytes
//!   to send plus a driver action; it talks to storage only through the `Mailbox` facade handed
//!   out by `rusty_svc::MailboxHub`, and to the directory through `Authenticator`.
//! * I/O driver: [`driver`] reads a stream, frames, upgrades to TLS, applies timeouts.
//!
//! The public surface is [`ImapHandler`] (a `rusty_svc::SessionHandler`).
#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

pub mod frame;
pub mod seqset;
pub mod utf7;
pub mod wire;
pub mod structure;
pub mod address;
pub mod envelope;
pub mod headers;
pub mod search;
pub mod fuzz_checks;
pub mod auth;
pub mod adapter;
pub mod command;
pub mod ext;
pub mod errors;
pub mod folders;
pub mod out;
pub mod select;
pub mod session;
mod cmds_fetch;
mod cmds_folder;
mod cmds_msg;
pub mod driver;

pub use driver::ImapHandler;
pub use folders::{FolderResolver, PersonalNamespace};
pub use session::Limits;
