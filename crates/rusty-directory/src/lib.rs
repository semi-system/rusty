#![forbid(unsafe_code)]

//! Directory: domains, plans, accounts, aliases, lists, config and authentication.

mod auth;
mod caps;
mod config;
mod dir;

pub use auth::{Authenticator, LocalAuthenticator};
pub use caps::{
    Capability, CapabilityInfo, CapabilityRole, MAX_ACTIVE_CAPABILITIES_PER_ACCOUNT,
    MAX_CAPABILITY_TTL_SECS,
};
pub use config::AccountStatus;
pub use dir::{
    allowed_local_char, display_domain, normalize_address, parse_address, Directory, Resolved,
    MAX_LIST_MEMBERS,
};
