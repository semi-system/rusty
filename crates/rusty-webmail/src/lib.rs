//! `rusty-webmail`: the webmail service (docs/design-webmail.md), the people who own a mailbox reading and writing mail in a browser. It is a service of its own, reached through the edge on its own host
//! name, because it faces the internet and will parse hostile messages: it holds no sealing or unsealing key, no DKIM key and no relay credential, and reaches the mail only through the facades the IMAP
//! service uses (`Mailbox`, `Directory`).
//!
//! This is the skeleton of the first step of the design's order of work: the configuration role, the session and the sign-in over the shared pieces, `/me`, and the health check. The read path
//! (folders, messages), the write path, the front end and the address book follow in their own steps.
#![forbid(unsafe_code)]

pub mod api;
pub mod health;
pub mod mail;
pub mod read;
pub mod run;
pub mod send;
pub mod compose;
pub mod session;
pub mod web;
pub mod write;
