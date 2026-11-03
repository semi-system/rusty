#![forbid(unsafe_code)]

pub mod config;
mod error;
mod ids;
pub mod proto;

pub use error::{Error, Result};
pub use ids::*;
