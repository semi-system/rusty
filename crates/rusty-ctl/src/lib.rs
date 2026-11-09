//! `rustyctl` as a library: the command-line layer (`args`: bytes in, typed command out) and the
//! command layer (`exec`: a command against the `Directory` facade, text and JSON out). Terminal
//! and file I/O, and opening the store, belong to the composition root (`rusty`), which also
//! lets the multi-call binary and the tests share this code.
#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

mod args;
mod exec;

pub use args::{parse_args, Command, Parsed, PasswordSource, Source, UsageError, DEFAULT_SELECTOR, USAGE};
pub use exec::{execute, map_error, render, CtlEnv, CtlError, Outcome, PasswordReader, Secret, MIN_PASSWORD_BYTES};
