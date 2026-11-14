#![forbid(unsafe_code)]

pub mod config;
pub mod dns;
pub mod dns_check;
pub mod dns_state;
mod config_role;
mod config_sections;
mod config_store;
mod config_valkey;
mod error;
mod ids;
pub mod proto;
pub mod text;

pub use error::{Error, Result};
pub use ids::*;
