//! `[sealing]` and `[unsealing]`: the two halves of the sealed-secrets key (admin web design, section 14.1).
//!
//! * `[sealing] public_key`: the public key secrets are sealed to, in its text form (`rusty-seal-v1:<64 hex digits>`). Held by
//!   whatever seals (the admin service, `rustyctl`). Not a secret.
//! * `[unsealing] key_files`: the files that hold the private key (`rusty-unseal-v1:<64 hex digits>`, mode 0600, read through the
//!   secret source), the current key first and the keys a rotation still needs after it. Only the outbound service has this
//!   section. The private key is never in the configuration itself.
//!
//! This module checks the shape only; whether a key is a usable key is checked by `rusty-seal` when it is read.

use std::path::PathBuf;

use serde::Deserialize;

use crate::{Error, Result};

const SEAL_PREFIX: &str = "rusty-seal-v1:";
/// The most keys one outbound service holds (the current one and those a rotation still needs).
pub const MAX_UNSEALING_KEYS: usize = 4;

/// `[sealing]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealingConfig {
    pub public_key: String,
}

impl SealingConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        let ok = self
            .public_key
            .strip_prefix(SEAL_PREFIX)
            .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        if ok {
            Ok(())
        } else {
            Err(Error::Invalid("sealing.public_key must be rusty-seal-v1: and 64 lower-case hex digits".into()))
        }
    }
}

/// `[unsealing]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnsealingConfig {
    /// Files holding the private keys: the first seals nothing but is the current key, the others only open what an earlier key sealed.
    pub key_files: Vec<PathBuf>,
}

impl UnsealingConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        let n = self.key_files.len();
        if n == 0 || n > MAX_UNSEALING_KEYS {
            return Err(Error::Invalid("unsealing.key_files must name one to four files".into()));
        }
        for (i, p) in self.key_files.iter().enumerate() {
            if !p.is_absolute() || self.key_files.iter().take(i).any(|o| o == p) {
                return Err(Error::Invalid("unsealing.key_files must be distinct absolute paths".into()));
            }
        }
        Ok(())
    }
}
