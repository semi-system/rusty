//! From the `[store]` section to `rusty_store::StoreConfig`: the one place that reads the database
//! password and the CA bundle and applies the policy of the daemon (verified TLS, runtime-role
//! check). The daemon and `rustyctl` both open their store through [`store_config`]; the migrator
//! (`rustyctl db migrate`) gets its own URL and credentials from [`migrator_target`].

use std::io::Read;
use std::path::Path;

use rusty_core::config::{parse_pg_url, StartupConfig, StoreBackend};
use rusty_core::{Error, Result};
use rusty_store::{PgPassword, PgLimits, PgSecurity, StoreConfig};
use rusty_svc::{FileSecretSource, SecretSource};

/// A CA bundle is public data; anything larger than this is not one.
const MAX_CA_BYTES: u64 = 1024 * 1024;

fn read_password(source: &dyn SecretSource, path: &Path) -> Result<PgPassword> {
    let raw = source.read(path)?;
    let text = String::from_utf8(raw).map_err(|_| Error::Invalid("the database password file is not UTF-8".into()))?;
    let pw = text.trim_end_matches(['\n', '\r']);
    if pw.is_empty() {
        return Err(Error::Invalid("the database password file is empty".into()));
    }
    Ok(PgPassword::new(pw.to_owned()))
}

fn read_ca(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path).map_err(|_| Error::Invalid("cannot read [store] ca_bundle".into()))?;
    let meta = file.metadata().map_err(|_| Error::Invalid("cannot read [store] ca_bundle".into()))?;
    if !meta.is_file() || meta.len() > MAX_CA_BYTES {
        return Err(Error::Invalid("[store] ca_bundle must be a regular file of at most 1 MiB".into()));
    }
    let mut buf = Vec::new();
    file.take(MAX_CA_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|_| Error::Invalid("cannot read [store] ca_bundle".into()))?;
    if !buf.windows(11).any(|w| w == b"-----BEGIN ") {
        return Err(Error::Invalid("[store] ca_bundle holds no PEM certificate".into()));
    }
    Ok(buf)
}

fn security(cfg: &StartupConfig, password_file: Option<&Path>, runtime: bool) -> Result<PgSecurity> {
    let s = &cfg.store;
    let password = match password_file {
        Some(p) => Some(read_password(&FileSecretSource::new(), p)?),
        None => None,
    };
    let ca_bundle_pem = match &s.ca_bundle {
        Some(p) => Some(read_ca(p)?),
        None => None,
    };
    Ok(PgSecurity {
        ssl_mode: Some(s.sslmode),
        ca_bundle_pem,
        allow_unverified_tls: s.insecure_allow_unverified_tls,
        password,
        verify_role: runtime,
    })
}

fn pool(cfg: &StartupConfig) -> PgLimits {
    let s = &cfg.store;
    PgLimits {
        max_connections: s.pool_size,
        acquire_timeout: s.acquire_timeout(),
        statement_timeout: s.statement_timeout(),
        idle_in_transaction_timeout: s.idle_in_transaction_timeout(),
        lock_timeout: s.lock_timeout(),
        // A writer waits for its mailbox's turn no longer than it would wait for the row lock.
        queue_timeout: s.lock_timeout(),
        ..PgLimits::default()
    }
}

/// The store the daemon and `rustyctl` use. `sqlite_url` is the already-validated SQLite URL (only
/// used by the sqlite backend).
pub fn store_config(cfg: &StartupConfig, sqlite_url: impl FnOnce() -> Result<String>) -> Result<StoreConfig> {
    match cfg.store.backend {
        StoreBackend::Sqlite => Ok(StoreConfig::sqlite(sqlite_url()?)),
        StoreBackend::Postgres => {
            let url = cfg.store.url.clone().ok_or_else(|| Error::Invalid("[store] url is required".into()))?;
            parse_pg_url(&url)?;
            Ok(StoreConfig::Postgres {
                url,
                pool: pool(cfg),
                security: security(cfg, cfg.store.password_file.as_deref(), true)?,
            })
        }
    }
}

/// URL and credentials of the migrator role (DDL), for `rustyctl db migrate` / `db status`.
pub fn migrator_target(cfg: &StartupConfig) -> Result<(String, PgSecurity)> {
    let s = &cfg.store;
    let user = s.migrator_user.as_deref().ok_or_else(|| Error::Invalid("[store] migrator_user is required for db commands".into()))?;
    let ep = parse_pg_url(s.url.as_deref().unwrap_or_default())?;
    let host = if ep.host.contains(':') { format!("[{}]", ep.host) } else { ep.host.clone() };
    let url = format!("postgres://{user}@{host}:{}/{}", ep.port, ep.database);
    Ok((url, security(cfg, s.migrator_password_file.as_deref(), false)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(extra: &str) -> StartupConfig {
        StartupConfig::from_toml_str(&format!(
            "blob_dir = \"/b\"\nhostname = \"m.example.test\"\n[tls]\ncert_file = \"/c\"\nkey_file = \"/k\"\n[store]\nbackend = \"postgres\"\nurl = \"postgres://rusty_app@db.internal:5433/rusty\"\n{extra}"
        ))
        .unwrap()
    }

    #[test]
    fn migrator_url_swaps_the_user_and_keeps_host_port_database() {
        let c = cfg("migrator_user = \"rusty_migrator\"\n");
        let (url, sec) = migrator_target(&c).unwrap();
        assert_eq!(url, "postgres://rusty_migrator@db.internal:5433/rusty");
        assert!(!sec.verify_role && sec.password.is_none());
        assert!(migrator_target(&cfg("")).is_err());
    }

    #[test]
    fn sqlite_backend_uses_the_given_url() {
        let c = StartupConfig::dev(Path::new("/tmp/x"));
        let s = store_config(&c, || Ok("sqlite:///tmp/x/m.sqlite".into())).unwrap();
        assert!(matches!(s, StoreConfig::Sqlite { .. }));
    }

    #[test]
    fn a_missing_password_file_is_refused_without_echoing_the_path_contents() {
        let c = cfg("password_file = \"/nonexistent/pw\"\n");
        assert!(store_config(&c, || Ok(String::new())).is_err());
    }

    #[test]
    fn debug_of_the_store_config_hides_url_and_password() {
        let dir = tempfile::tempdir().unwrap();
        let pw = dir.path().join("pw");
        std::fs::write(&pw, "s3cret-pass\n").unwrap();
        std::fs::set_permissions(&pw, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        let c = cfg(&format!("password_file = \"{}\"\n", pw.display()));
        let s = store_config(&c, || Ok(String::new())).unwrap();
        let shown = format!("{s:?}");
        assert!(!shown.contains("s3cret-pass") && !shown.contains("db.internal"), "{shown}");
        // A world-readable password file is refused.
        std::fs::set_permissions(&pw, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        assert!(store_config(&c, || Ok(String::new())).is_err());
    }
}
