#![forbid(unsafe_code)]

pub mod config;
mod error;
mod ids;
pub mod proto;
pub mod text;

pub use error::{Error, Result};
pub use ids::*;
