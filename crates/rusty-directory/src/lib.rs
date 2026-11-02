#![forbid(unsafe_code)]

//! Directory: domains, plans, accounts, aliases, lists, config and authentication.

mod auth;
mod config;
mod dir;

pub use auth::{Authenticator, LocalAuthenticator};
pub use config::AccountStatus;
pub use dir::{display_domain, normalize_address, parse_address, Directory, Resolved};
