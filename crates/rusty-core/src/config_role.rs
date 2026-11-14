//! Roles: one configuration file per service (image design section 4). `StartupConfig::from_toml_str_for(role, text)`
//! accepts only the sections the role uses and **refuses the others**, so a service is never handed a secret it has
//! no business with (the relay password and DKIM paths exist only in `outbound`'s file, the TLS key only where a
//! listener needs it, the S3 and database credentials only where used). The refusal names the section and the
//! service and nothing from the file.
//!
//! The matrix is written out per role and is covered by a test that states it a second time, independently.
//!
//! Known limits, to be closed with the SMTP crate split: `[smtp]` is shared by `smtp-in` and `submission`, and
//! `[outbound]` by `submission` (its submission limits) and `outbound` (the worker, the relay, the DKIM keys), so
//! submission still sees the relay settings of its section. Which listeners a service starts is the business of its
//! binary; the defaults of an absent `[smtp]` or `[imap]` section do not make a role open another role's ports.

use std::path::Path;

use toml::Table;

use crate::config::StartupConfig;
use crate::{Error, Result};

/// The services of the platform that read a configuration file of their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// Port 25: unauthenticated delivery of incoming mail.
    SmtpIn,
    /// Ports 587 and 465: authenticated submission, queueing for the outbound worker.
    Submission,
    /// Ports 143 and 993.
    Imap,
    /// The outbound worker: DKIM keys, relay credentials.
    Outbound,
    /// The garbage collector and the periodic jobs.
    Gc,
    /// The admin web.
    Admin,
}

/// The value standing in for `blob_dir` in the configuration of a role that never touches blobs: a path no service
/// could mistake for real storage. It disappears when blobs move to S3.
pub const UNUSED_BLOB_DIR: &str = "/nonexistent/rusty-no-blob-dir";

const SHARED: [&str; 5] = ["hostname", "log_level", "log_format", "store", "limits"];

impl Role {
    /// The name used in file names (`/etc/rusty/<name>.toml`), logs and images.
    pub fn name(self) -> &'static str {
        match self {
            Role::SmtpIn => "smtp-in",
            Role::Submission => "submission",
            Role::Imap => "imap",
            Role::Outbound => "outbound",
            Role::Gc => "gc",
            Role::Admin => "admin",
        }
    }

    pub fn from_name(name: &str) -> Option<Role> {
        [Role::SmtpIn, Role::Submission, Role::Imap, Role::Outbound, Role::Gc, Role::Admin].into_iter().find(|r| r.name() == name)
    }

    /// Whether the service reads or writes message blobs (and so needs `blob_dir`).
    pub fn uses_blobs(self) -> bool {
        self != Role::Admin
    }

    /// The top-level keys and sections beyond the shared ones that this role accepts.
    fn own(self) -> &'static [&'static str] {
        match self {
            Role::SmtpIn => &["blob_dir", "max_message_bytes", "tls", "smtp"],
            Role::Submission => &["blob_dir", "max_message_bytes", "tls", "smtp", "outbound", "valkey"],
            Role::Imap => &["blob_dir", "max_message_bytes", "tls", "imap", "valkey"],
            Role::Outbound => &["blob_dir", "outbound", "unsealing"],
            Role::Gc => &["blob_dir", "gc_grace_seconds", "dns_check"],
            Role::Admin => &["tls", "valkey", "sealing"],
        }
    }

    fn allows(self, key: &str) -> bool {
        SHARED.contains(&key) || self.own().contains(&key)
    }
}

/// Every top-level key of the schema. Only these are named in a refusal: a key that is not one of them is
/// reported as unknown, so nothing the operator typed (or pasted by mistake) is echoed.
const SCHEMA_KEYS: [&str; 16] = [
    "blob_dir", "log_level", "log_format", "hostname", "max_message_bytes", "gc_grace_seconds", "store", "tls", "smtp", "imap", "outbound",
    "limits", "dns_check", "valkey", "sealing", "unsealing",
];

fn shown(key: &str) -> &'static str {
    SCHEMA_KEYS.iter().copied().find(|k| *k == key).unwrap_or("an unknown section")
}

impl StartupConfig {
    /// Parse and validate the configuration file text of `role`: the sections the role does not use are refused (see
    /// the module docs), then the usual schema and bounds apply. A role that does not touch blobs gets
    /// [`UNUSED_BLOB_DIR`] in `blob_dir` and may not carry one.
    pub fn from_toml_str_for(role: Role, s: &str) -> Result<StartupConfig> {
        let invalid = || Error::Invalid("configuration is not valid (syntax or schema)".into());
        let mut table: Table = s.parse().map_err(|_| invalid())?;
        if let Some(key) = table.keys().find(|k| !role.allows(k)) {
            let section = shown(key);
            return Err(Error::Invalid(format!("[{section}] is not accepted in the configuration of the {} service", role.name())));
        }
        if !role.uses_blobs() {
            table.insert("blob_dir".to_owned(), toml::Value::String(UNUSED_BLOB_DIR.to_owned()));
        }
        let mut c: StartupConfig = table.try_into().map_err(|_| invalid())?;
        c.validate()?;
        Ok(c)
    }

    /// [`StartupConfig::from_toml_str_for`] on a file, read with the same bounds as [`StartupConfig::load`].
    pub fn load_for(role: Role, path: &Path) -> Result<StartupConfig> {
        Self::load_with(path, |text| Self::from_toml_str_for(role, text))
    }
}
