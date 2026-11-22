#![forbid(unsafe_code)]

pub mod cidr;
pub mod config;
pub mod dns;
pub mod dns_check;
pub mod dns_state;
pub mod mbox;
pub mod relay_policy;
pub mod storage_policy;
mod config_role;
mod config_admin;
mod config_blob_s3;
mod config_filter;
mod config_metrics;
mod config_resolver;
mod config_sealing;
mod config_sections;
mod config_store;
mod config_valkey;
mod error;
mod ids;
pub mod proto;
pub mod text;

pub use error::{Error, Result};
pub use ids::*;
