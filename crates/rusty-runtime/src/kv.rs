//! The key-value store of `[valkey]` for a service: connected at start (a wrong address or password stops the start; there is no silent fall back), with a key prefix of its own
//! kind (`<prefix>:<kind>:`), so the services that share a kind of key (submission and IMAP count authentication failures together) share a namespace and nothing else does.

use std::sync::Arc;
use std::time::Duration;

use rusty_core::config::StartupConfig;
use rusty_core::{Error, Result};
use rusty_svc::{FileSecretSource, SecretSource};
use rusty_valkey::Kv;

/// The connected store, or `None` when `[valkey]` is not configured. `kind` names the keys' namespace (`auth`, `admin`).
pub async fn connect_kv(cfg: &StartupConfig, kind: &str) -> Result<Option<Arc<dyn Kv>>> {
    let Some(v) = &cfg.valkey else { return Ok(None) };
    let password = match &v.password_file {
        Some(p) => Some(String::from_utf8(FileSecretSource::new().read(p)?).map_err(|_| Error::Invalid("valkey.password_file is not text".into()))?.trim_end().to_owned()),
        None => None,
    };
    let tls = if v.tls {
        let ca = match &v.ca_bundle {
            Some(p) => Some(std::fs::read(p).map_err(|_| Error::Invalid("valkey.ca_bundle cannot be read".into()))?),
            None => None,
        };
        Some(rusty_valkey::TlsOptions { ca_bundle_pem: ca })
    } else {
        None
    };
    let kv = rusty_valkey::ValkeyKv::connect(rusty_valkey::ValkeyOptions {
        host: v.host.clone(),
        port: v.port,
        tls,
        username: v.username.clone(),
        password,
        connect_timeout: Duration::from_millis(v.connect_timeout_ms),
        command_timeout: Duration::from_millis(v.command_timeout_ms),
        key_prefix: format!("{}:{kind}:", v.key_prefix.trim_end_matches(':')),
    })
    .await?;
    tracing::info!(host = %v.host, kind, "the shared key-value store is connected");
    Ok(Some(Arc::new(kv)))
}
