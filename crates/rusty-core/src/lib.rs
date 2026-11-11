#![forbid(unsafe_code)]

pub mod config;
pub mod dns;
pub mod dns_state;
mod config_sections;
mod error;
mod ids;
pub mod proto;
pub mod text;

pub use error::{Error, Result};
pub use ids::*;
