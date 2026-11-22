//! The command layer: a typed [`Command`] runs against the `Directory` facade and produces an
//! [`Outcome`] (plain text plus a JSON value). No sockets, no terminal, no database handle:
//! passwords come through the [`PasswordReader`] seam and the directory is injected, so the same
//! code runs over any backend and under test.

use std::fmt;
use std::net::IpAddr;

use rusty_core::dns::{zone_line, DnsRecord, Requirement};
use rusty_core::dns_check::{check_records, CheckOptions, DnsSource};
use rusty_core::dns_state::RecordStatus;
use rusty_core::Error;
use rusty_directory::{
    parse_address, AccountInfo, AccountStatus, Directory, DomainInfo, DomainStatus,
};
use rusty_store::DnsStore;
use serde_json::{json, Value};

use crate::args::{Command, PasswordSource};

/// Pages of this many rows are read when a command lists everything.
const PAGE: usize = 200;
/// Shortest password `rustyctl` accepts (the directory itself only refuses empty and over-long).
pub const MIN_PASSWORD_BYTES: usize = 8;
/// Aliases every domain must have (RFC 5321 section 4.5.1: `postmaster`; `abuse` by convention).
const REQUIRED_ALIASES: [&str; 2] = ["postmaster", "abuse"];

/// A password that never prints.
pub struct Secret(String);

impl Secret {
    pub fn new(s: String) -> Secret {
        Secret(s)
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Where passwords come from (standard input, a terminal, a test). Called only for the sources
/// that need it, and before any database work, so a refused password changes nothing.
pub trait PasswordReader {
    fn read(&mut self, source: PasswordSource) -> Result<Secret, CtlError>;
}

/// What the commands need from their environment.
pub struct CtlEnv<'a> {
    pub directory: &'a Directory,
    pub passwords: &'a mut dyn PasswordReader,
    /// `hostname` of the configuration, for the DNS hints of `check`.
    pub hostname: Option<&'a str>,
    /// One line describing the configuration that was read (`check` prints it).
    pub config_note: &'a str,
    /// `[outbound] dkim_dir`: where `dkim generate` writes keys (none: the command refuses).
    pub dkim_dir: Option<&'a std::path::Path>,
    /// The DNS facade: the public half of every key made here is kept in the database, so the admin web (which never reads a
    /// private key) and `check` can show the record to publish.
    pub dns: &'a DnsStore,
    /// What the configuration says about outgoing mail, for the SPF and address records.
    pub outbound: OutboundFacts<'a>,
    /// The live DNS source of `domain check` (the composition root builds it only for that command).
    pub live_dns: Option<&'a rusty_smtp::stack::LiveDns>,
    /// `[sealing] public_key`: the key secrets are sealed to (`dkim generate --sealed`).
    pub sealing: Option<&'a rusty_seal::SealingKey>,
    /// Where sealed secrets are written.
    pub sealed: Option<&'a rusty_store::SealedWriter>,
    /// The per-domain relay store (`domain relay`).
    pub relays: Option<&'a rusty_store::RelayStore>,
    /// The blob stores of domains (`storage`).
    pub blob_stores: Option<&'a rusty_store::BlobStoreAdmin>,
    /// The pools of backends of domains (`storage pool`, `storage backend`).
    pub pools: Option<&'a rusty_store::BlobPoolAdmin>,
    /// The job queue (`domain relay check` enqueues a `relay.check` job for the outbound service).
    pub jobs: Option<&'a rusty_store::JobQueue>,
    /// Administrator grants (`admin grant`, `revoke`, `list`).
    pub admins: Option<&'a rusty_store::AdminStore>,
    /// The blob store and the garbage collector's reads (`blobs verify`); the composition root opens them only for that command.
    pub blobs: Option<&'a dyn rusty_store::BlobStore>,
    pub gc: Option<&'a rusty_store::Gc>,
    /// Every blob store of the deployment (`blobs verify` checks each one); none: the platform store only.
    pub stores: Option<&'a dyn rusty_store::BlobRouter>,
    /// What the `mailbox` commands read and write mail with; the composition root opens the blob stores only for those.
    pub mail: Option<MailEnv>,
}

/// The mailbox repository and the blob stores behind it, for the `mailbox` commands.
#[derive(Clone)]
pub struct MailEnv {
    pub repo: std::sync::Arc<dyn rusty_store::repo::MailboxRepo>,
    pub stores: std::sync::Arc<dyn rusty_store::BlobRouter>,
    /// The largest message the mailbox takes (`max_message_bytes` of the configuration).
    pub max_message_bytes: usize,
}

/// The parts of `[outbound]` the DNS records depend on.
#[derive(Debug, Clone, Copy, Default)]
pub struct OutboundFacts<'a> {
    /// `[outbound] public_ips`: the addresses mail leaves from (and the mail host resolves to).
    pub public_ips: &'a [IpAddr],
    /// Mail goes through `[outbound.relay]`: the reverse records of our own addresses are not checked.
    pub via_relay: bool,
    /// `[outbound.relay] spf_include`: the provider's SPF include.
    pub spf_include: Option<&'a str>,
}

/// What a command produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub text: String,
    pub json: Value,
    /// The command ran but found a problem (a required DNS record that is wrong): the process exits 1 after printing it.
    pub failed: bool,
}

impl Outcome {
    fn new(text: impl Into<String>, json: Value) -> Outcome {
        Outcome { text: text.into(), json, failed: false }
    }

    fn failing(mut self, failed: bool) -> Outcome {
        self.failed = failed;
        self
    }
}

/// A failure to report: the exit code (1 runtime, 2 usage) and a message that never carries a
/// password. Backend text appears only as the operator-diagnostic `detail`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtlError {
    pub exit: u8,
    pub message: String,
    pub detail: Option<String>,
}

impl CtlError {
    pub fn runtime(message: impl Into<String>) -> CtlError {
        CtlError { exit: 1, message: message.into(), detail: None }
    }

    pub fn usage(message: impl Into<String>) -> CtlError {
        CtlError { exit: 2, message: message.into(), detail: None }
    }
}

impl fmt::Display for CtlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(d) = &self.detail {
            write!(f, " ({d})")?;
        }
        Ok(())
    }
}

impl std::error::Error for CtlError {}

/// Map a store/directory error to the operator-facing message. The messages of `Error` are
/// fixed reasons, so they are safe to show; `what` says which object the command was about.
pub fn map_error(e: &Error, what: &str) -> CtlError {
    match e {
        Error::NotFound(_) => CtlError::runtime(format!("{what}: not found")),
        Error::Conflict(_) => CtlError::runtime(format!("{what}: already exists")),
        Error::Invalid(m) => CtlError::runtime(format!("{what}: invalid ({m})")),
        Error::Quota(m) => CtlError::runtime(format!("{what}: limit reached ({m})")),
        Error::Busy(_) => CtlError::runtime("the database is busy; try again"),
        Error::Storage { message, source } => {
            let mut c = CtlError::runtime(format!("storage error: {message}"));
            c.detail = source.as_ref().map(|s| s.to_string());
            c
        }
    }
}

fn status_word(s: AccountStatus) -> &'static str {
    s.as_str()
}

async fn domain_named(d: &Directory, name: &str) -> Result<DomainInfo, CtlError> {
    d.find_domain(name)
        .await
        .map_err(|e| map_error(&e, "domain"))?
        .ok_or_else(|| CtlError::runtime("domain: not found (create it with `domain add`)"))
}

async fn account_named(d: &Directory, addr: &str) -> Result<AccountInfo, CtlError> {
    d.find_account(addr)
        .await
        .map_err(|e| map_error(&e, "account"))?
        .ok_or_else(|| CtlError::runtime("account: not found"))
}

async fn all_domains(d: &Directory) -> Result<Vec<DomainInfo>, CtlError> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = d.list_domains(after, PAGE).await.map_err(|e| map_error(&e, "domains"))?;
        after = page.last().map(|x| x.id);
        let n = page.len();
        out.extend(page);
        if n < PAGE {
            return Ok(out);
        }
    }
}

async fn all_accounts(d: &Directory, dom: &DomainInfo) -> Result<Vec<AccountInfo>, CtlError> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = d.list_accounts(dom.id, after, PAGE).await.map_err(|e| map_error(&e, "accounts"))?;
        after = page.last().map(|x| x.id);
        let n = page.len();
        out.extend(page);
        if n < PAGE {
            return Ok(out);
        }
    }
}

async fn all_aliases(d: &Directory, dom: &DomainInfo) -> Result<Vec<rusty_directory::AliasInfo>, CtlError> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = d.list_aliases(dom.id, after, PAGE).await.map_err(|e| map_error(&e, "aliases"))?;
        after = page.last().map(|x| x.id);
        let n = page.len();
        out.extend(page);
        if n < PAGE {
            return Ok(out);
        }
    }
}

/// Domains a listing covers: the named one, or all.
async fn scope(d: &Directory, domain: &Option<String>) -> Result<Vec<DomainInfo>, CtlError> {
    match domain {
        Some(n) => Ok(vec![domain_named(d, n).await?]),
        None => all_domains(d).await,
    }
}

/// `sealed_secrets.purpose` of a relay password; the outbound service opens it under the same name (`DomainRoute`).
const RELAY_PASSWORD_PURPOSE: &str = rusty_smtp::outbound::domain_route::RELAY_PASSWORD_PURPOSE;

fn relay_stores<'a>(env: &CtlEnv<'a>) -> Result<(&'a rusty_store::RelayStore, &'a rusty_store::SealedWriter), CtlError> {
    match (env.relays, env.sealed) {
        (Some(r), Some(w)) => Ok((r, w)),
        _ => Err(CtlError::runtime("domain relays are not available here")),
    }
}

/// `domain relay set`: validate what was typed with the same rules the web will apply, seal the password, store both.
///
/// A login **always** needs its password in the same command: a new host must never inherit the password that was given for the old one.
async fn domain_relay_set(env: &mut CtlEnv<'_>, command: &Command) -> Result<Outcome, CtlError> {
    let Command::DomainRelaySet { name, host, port, tls, user, spf_include, password: source } = command else {
        return Err(CtlError::runtime("not a relay command"));
    };
    let (port, tls, source, user, spf_include) = (*port, *tls, *source, user.as_deref(), spf_include.as_deref());
    let relay = rusty_core::relay_policy::validate_tenant_relay(host, port, tls, user).map_err(|e| map_error(&e, "relay"))?;
    // The password is read before any database work, so a refused one changes nothing.
    let password = match (user, source) {
        (Some(_), PasswordSource::None) => return Err(CtlError::runtime("a relay with --user needs its password: use --password-stdin or --prompt")),
        (None, s) if s != PasswordSource::None => return Err(CtlError::runtime("a password needs --user (a relay without a login has none)")),
        (Some(_), s) => {
            let p = env.passwords.read(s)?;
            let t = p.expose();
            if t.is_empty() || t.len() > 1024 || t.chars().any(char::is_control) {
                return Err(CtlError::runtime("the relay password must be one printable line of at most 1024 bytes"));
            }
            Some(p)
        }
        (None, _) => None,
    };
    let (relays, writer) = relay_stores(env)?;
    let dom = find_domain(env, name).await?;
    if let Some(p) = &password {
        let Some(seal) = env.sealing else {
            return Err(CtlError::runtime("set [sealing] public_key in the configuration: the password is sealed to it (`rustyctl seal keygen` makes one)"));
        };
        let context = rusty_seal::Context::new("domain", dom.id.0, RELAY_PASSWORD_PURPOSE).map_err(|e| map_error(&e, "relay password"))?;
        let sealed = seal.seal(&context, p.expose().as_bytes()).map_err(|e| map_error(&e, "relay password"))?;
        writer.put("domain", dom.id.0, RELAY_PASSWORD_PURPOSE, &seal.id().to_hex(), &sealed).await.map_err(|e| map_error(&e, "relay password"))?;
    } else {
        // No login any more: a password kept from an earlier relay would be a secret nobody can see or use.
        writer.delete("domain", dom.id.0, RELAY_PASSWORD_PURPOSE).await.map_err(|e| map_error(&e, "relay password"))?;
    }
    relays.set(dom.id, &relay, spf_include).await.map_err(|e| map_error(&e, "relay"))?;
    let login = relay.username.as_deref().map_or_else(|| "no login".to_owned(), |u| format!("login {u}, password sealed"));
    Ok(Outcome::new(
        format!("relay of {} set: {}:{} ({}, {login}); mail sent from this domain now leaves through it", dom.name, relay.host, relay.port, rusty_store::tls_text(relay.tls)),
        json!({ "domain": dom.name, "host": relay.host, "port": relay.port, "tls": rusty_store::tls_text(relay.tls), "username": relay.username, "spf_include": spf_include, "password_set": password.is_some() }),
    ))
}

fn storage_stores<'a>(env: &CtlEnv<'a>) -> Result<(&'a rusty_store::BlobStoreAdmin, &'a rusty_store::SealedWriter), CtlError> {
    match (env.blob_stores, env.sealed) {
        (Some(a), Some(w)) => Ok((a, w)),
        _ => Err(CtlError::runtime("blob stores are not available here")),
    }
}

/// `storage set`: check the settings with the rules the web will apply, read the two credentials from standard input, seal them, store the settings.
async fn storage_set(env: &mut CtlEnv<'_>, command: &Command) -> Result<Outcome, CtlError> {
    use rusty_core::storage_policy::{validate_tenant_s3, ACCESS_KEY_PURPOSE, SECRET_KEY_PURPOSE, STORE_SECRET_OWNER};
    let Command::StorageSet { name, endpoint, region, bucket, prefix, path_style } = command else {
        return Err(CtlError::runtime("not a storage command"));
    };
    let s3 = validate_tenant_s3(endpoint, region, bucket, prefix, *path_style).map_err(|e| map_error(&e, "storage"))?;
    // The credentials are read before any database work, so a refused one changes nothing.
    let access = env.passwords.read(PasswordSource::Stdin)?;
    let secret = env.passwords.read(PasswordSource::Stdin)?;
    for (what, v) in [("access key", access.expose()), ("secret key", secret.expose())] {
        if v.is_empty() || v.len() > 256 || v.chars().any(char::is_control) {
            return Err(CtlError::runtime(match what {
                "access key" => "the access key must be one printable line of at most 256 bytes",
                _ => "the secret key must be one printable line of at most 256 bytes",
            }));
        }
    }
    let (admin, writer) = storage_stores(env)?;
    let Some(seal) = env.sealing else {
        return Err(CtlError::runtime("set [sealing] public_key in the configuration: the credentials are sealed to it (`rustyctl seal keygen` makes one)"));
    };
    let dom = find_domain(env, name).await?;
    let (row, created) = admin.set_s3(dom.id, &s3).await.map_err(|e| map_error(&e, "storage"))?;
    for (purpose, value) in [(ACCESS_KEY_PURPOSE, access.expose()), (SECRET_KEY_PURPOSE, secret.expose())] {
        let context = rusty_seal::Context::new(STORE_SECRET_OWNER, row.id.0, purpose).map_err(|e| map_error(&e, "storage credentials"))?;
        let sealed = seal.seal(&context, value.as_bytes()).map_err(|e| map_error(&e, "storage credentials"))?;
        writer.put(STORE_SECRET_OWNER, row.id.0, purpose, &seal.id().to_hex(), &sealed).await.map_err(|e| map_error(&e, "storage credentials"))?;
    }
    Ok(Outcome::new(
        format!(
            "{} {} S3 store {}: {} bucket {} (credentials sealed); mail of this domain is stored there from now on once the services have it open (the first use opens it)",
            if created { "made the" } else { "kept the" },
            dom.name,
            row.id.0,
            s3.endpoint,
            s3.bucket
        ),
        json!({ "domain": dom.name, "store": row.id.0, "created": created, "endpoint": s3.endpoint, "region": s3.region, "bucket": s3.bucket, "prefix": s3.prefix, "path_style": s3.path_style }),
    ))
}

/// `storage check`: the mail service holds the unsealing key, so it is the one that can test the store; this queues the job.
async fn storage_check(env: &mut CtlEnv<'_>, name: &str) -> Result<Outcome, CtlError> {
    use rusty_core::storage_policy::STORAGE_CHECK_JOB_KIND;
    let (admin, _) = storage_stores(env)?;
    let Some(jobs) = env.jobs else { return Err(CtlError::runtime("the job queue is not available here")) };
    let dom = find_domain(env, name).await?;
    let Some(row) = admin.for_domain(dom.id).await.map_err(|e| map_error(&e, "storage"))? else {
        return Err(CtlError::runtime("the domain has no store of its own to check"));
    };
    if row.kind != "s3" && row.kind != "pool" {
        return Err(CtlError::runtime("only an S3 store or a pool is checked"));
    }
    jobs.enqueue(STORAGE_CHECK_JOB_KIND, Some(dom.id), &json!({ "store": row.id.0 }), None).await.map_err(|e| map_error(&e, "storage check"))?;
    Ok(Outcome::new(
        format!("a check of the store of {} was queued; the mail service runs it within seconds: see `rustyctl storage show {}`", dom.name, dom.name),
        json!({ "domain": dom.name, "store": row.id.0, "queued": true }),
    ))
}

/// `storage show`: everything except the credentials, which nothing here can read.
async fn storage_show(env: &mut CtlEnv<'_>, name: &str) -> Result<Outcome, CtlError> {
    use rusty_core::storage_policy::{ACCESS_KEY_PURPOSE, SECRET_KEY_PURPOSE, STORE_SECRET_OWNER};
    let (admin, writer) = storage_stores(env)?;
    let dom = find_domain(env, name).await?;
    let Some(row) = admin.for_domain(dom.id).await.map_err(|e| map_error(&e, "storage"))? else {
        return Ok(Outcome::new(format!("{} uses the platform blob store", dom.name), json!({ "domain": dom.name, "store": null })));
    };
    if row.kind == "pool" {
        let backends = backends_json(env, row.id).await?;
        let lines: Vec<String> = backends
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|b| {
                        format!(
                            "  {} [{}] weight {} {} bucket {}; keys {}/{}; {} objects; check: {}",
                            b["name"].as_str().unwrap_or("?"),
                            b["state"].as_str().unwrap_or("?"),
                            b["weight"],
                            b.get("settings").and_then(|s| s.get("endpoint")).and_then(Value::as_str).unwrap_or("?"),
                            b.get("settings").and_then(|s| s.get("bucket")).and_then(Value::as_str).unwrap_or("?"),
                            if b["access_key_set"] == true { "sealed" } else { "MISSING" },
                            if b["secret_key_set"] == true { "sealed" } else { "MISSING" },
                            b["objects"],
                            b["check_status"].as_str().unwrap_or("?"),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let policy: Value = serde_json::from_str(&row.config).unwrap_or(Value::Null);
        let text = format!("{}: pool store {} {}; pool check: {}\n{}", dom.name, row.id.0, policy, row.check_status, lines.join("\n"));
        return Ok(Outcome::new(
            text,
            json!({ "domain": dom.name, "store": row.id.0, "kind": "pool", "policy": policy, "check_status": row.check_status, "checked_at": row.checked_at, "backends": backends }),
        ));
    }
    let meta = writer.meta(STORE_SECRET_OWNER, row.id.0).await.map_err(|e| map_error(&e, "storage credentials"))?;
    let has = |p: &str| meta.iter().any(|m| m.purpose == p);
    let (access, secret) = (has(ACCESS_KEY_PURPOSE), has(SECRET_KEY_PURPOSE));
    let settings: Value = serde_json::from_str(&row.config).unwrap_or(Value::Null);
    let setting = |name: &str, default: &'static str| settings.get(name).and_then(Value::as_str).unwrap_or(default).to_owned();
    let check = match row.checked_at {
        Some(at) => format!("last check: {} (at {at})", row.check_status),
        None => "never checked".to_owned(),
    };
    let text = format!(
        "{}: {} store {}: {} bucket {} region {} prefix {:?}; access key: {}; secret key: {}; {check}",
        dom.name,
        row.kind,
        row.id.0,
        setting("endpoint", "?"),
        setting("bucket", "?"),
        setting("region", "?"),
        setting("prefix", ""),
        if access { "sealed" } else { "MISSING" },
        if secret { "sealed" } else { "MISSING" },
    );
    Ok(Outcome::new(
        text,
        json!({ "domain": dom.name, "store": row.id.0, "kind": row.kind, "settings": settings, "access_key_set": access, "secret_key_set": secret, "check_status": row.check_status, "checked_at": row.checked_at }),
    ))
}

/// `domain relay show`: everything except the password, which nothing here can read.
async fn domain_relay_show(env: &mut CtlEnv<'_>, name: &str) -> Result<Outcome, CtlError> {
    let (relays, writer) = relay_stores(env)?;
    let dom = find_domain(env, name).await?;
    let Some(r) = relays.get(dom.id).await.map_err(|e| map_error(&e, "relay"))? else {
        return Ok(Outcome::new(
            format!("{} has no relay of its own (the platform route is used)", dom.name),
            json!({ "domain": dom.name, "relay": null }),
        ));
    };
    let has_password = writer.meta("domain", dom.id.0).await.map_err(|e| map_error(&e, "relay password"))?.iter().any(|m| m.purpose == RELAY_PASSWORD_PURPOSE);
    let check = match (&r.check_status, r.checked_at) {
        (Some(s), Some(at)) => format!("last check: {s} (at {at})"),
        _ => "never checked".to_owned(),
    };
    let text = format!(
        "{}: {}:{} {}; login: {}; password: {}; SPF include: {}; {check}",
        dom.name,
        r.host,
        r.port,
        r.tls,
        r.username.as_deref().unwrap_or("none"),
        if has_password { "set (cannot be shown)" } else { "not set" },
        r.spf_include.as_deref().unwrap_or("none"),
    );
    Ok(Outcome::new(
        text,
        json!({ "domain": dom.name, "relay": {
            "host": r.host, "port": r.port, "tls": r.tls, "username": r.username, "spf_include": r.spf_include,
            "password_set": has_password, "check_status": r.check_status, "checked_at": r.checked_at,
        } }),
    ))
}

/// `domain relay check`: the admin cannot read the password, so the outbound service (which holds the unsealing key) runs the check as a
/// `relay.check` job and records a fixed category on the relay; `domain relay show` displays it.
async fn domain_relay_check(env: &mut CtlEnv<'_>, name: &str) -> Result<Outcome, CtlError> {
    let (relays, _) = relay_stores(env)?;
    let Some(jobs) = env.jobs else { return Err(CtlError::runtime("the job queue is not available here")) };
    let dom = find_domain(env, name).await?;
    if relays.get(dom.id).await.map_err(|e| map_error(&e, "relay"))?.is_none() {
        return Err(CtlError::runtime("this domain has no relay of its own (set one with `domain relay set`)"));
    }
    rusty_smtp::outbound::relay_job::enqueue_relay_check(jobs, dom.id, &dom.name).await.map_err(|e| map_error(&e, "relay check"))?;
    Ok(Outcome::new(
        format!("check of the relay of {} queued: the outbound service runs it; see the result with `domain relay show {}`", dom.name, dom.name),
        json!({ "domain": dom.name, "queued": true }),
    ))
}

/// `domain relay remove`: the domain goes back to the platform route and its sealed password goes with the relay.
async fn domain_relay_remove(env: &mut CtlEnv<'_>, name: &str) -> Result<Outcome, CtlError> {
    let (relays, writer) = relay_stores(env)?;
    let dom = find_domain(env, name).await?;
    let removed = relays.remove(dom.id).await.map_err(|e| map_error(&e, "relay"))?;
    writer.delete("domain", dom.id.0, RELAY_PASSWORD_PURPOSE).await.map_err(|e| map_error(&e, "relay password"))?;
    Ok(Outcome::new(
        if removed { format!("relay of {} removed (the platform route is used)", dom.name) } else { format!("{} had no relay of its own", dom.name) },
        json!({ "domain": dom.name, "removed": removed }),
    ))
}

async fn mailbox_of(env: &CtlEnv<'_>, addr: &str) -> Result<(rusty_store::Mailbox, MailEnv), CtlError> {
    let Some(mail) = env.mail.clone() else { return Err(CtlError::runtime("the mailbox commands need the blob store, which is not available here")) };
    let account = account_named(env.directory, addr).await?;
    let mb = rusty_store::Mailbox::open_with(mail.repo.clone(), mail.stores.clone(), account.id).await.map_err(|e| map_error(&e, "mailbox"))?;
    // An administrator's import or recovery is not held to the account's quota (the quota is for what its owner adds).
    let mb = mb.with_limits(rusty_store::MailboxLimits { max_message_bytes: mail.max_message_bytes, quota_bytes: None });
    Ok((mb, mail))
}

fn when_text(unix: i64) -> String {
    crate::mbox::asctime(unix)
}

/// `mailbox deleted`: the deleted messages that are still kept, with the id `mailbox recover` takes.
async fn mailbox_deleted(env: &mut CtlEnv<'_>, addr: &str) -> Result<Outcome, CtlError> {
    let (mb, _) = mailbox_of(env, addr).await?;
    let folders = mb.list_folders().await.map_err(|e| map_error(&e, "mailbox"))?;
    let name_of = |id: rusty_core::FolderId| folders.iter().find(|f| f.id == id).map_or_else(|| id.0.to_string(), |f| f.name.clone());
    let (mut lines, mut items, mut after) = (Vec::new(), Vec::new(), 0i64);
    loop {
        let page = mb.list_deleted(after, 500).await.map_err(|e| map_error(&e, "mailbox"))?;
        let Some(last) = page.last() else { break };
        after = last.id.0;
        for m in &page {
            lines.push(format!("{}  deleted {}  from {}  in {}  {} bytes  {}", m.id.0, when_text(m.deleted_at), m.from, name_of(m.folder), m.size, m.subject));
            items.push(json!({ "id": m.id.0, "deleted_at": m.deleted_at, "folder": name_of(m.folder), "size": m.size, "from": m.from, "subject": m.subject }));
        }
    }
    if lines.is_empty() {
        return Ok(Outcome::new("no deleted messages are kept", json!({ "messages": [] })));
    }
    Ok(Outcome::new(lines.join("\n"), json!({ "messages": items })))
}

/// The folder named `name` (its name or its role, case does not matter), or the Inbox.
fn find_folder(folders: &[rusty_store::FolderInfo], name: Option<&str>) -> Option<rusty_core::FolderId> {
    let wanted = name.unwrap_or("inbox");
    folders.iter().find(|f| f.name.eq_ignore_ascii_case(wanted) || f.role.eq_ignore_ascii_case(wanted)).map(|f| f.id)
}

/// `mailbox recover`: a deleted message comes back as a new message in the folder.
async fn mailbox_recover(env: &mut CtlEnv<'_>, addr: &str, id: i64, folder: Option<&str>) -> Result<Outcome, CtlError> {
    let (mb, _) = mailbox_of(env, addr).await?;
    let folders = mb.list_folders().await.map_err(|e| map_error(&e, "mailbox"))?;
    let target = find_folder(&folders, folder).ok_or_else(|| CtlError::runtime("folder: not found"))?;
    let made = mb.recover(rusty_core::MessageId(id), target).await.map_err(|e| map_error(&e, "message"))?;
    Ok(Outcome::new(format!("recovered message {id} as a new message (uid {}) in {}", made.uid, folder.unwrap_or("inbox")), json!({ "recovered": id, "uid": made.uid })))
}

/// A folder name as a file name: its path separators and anything odd become `_`.
fn file_name_of(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == ' ' { c } else { '_' }).collect();
    let s = s.trim_matches(|c| c == '.' || c == ' ').to_owned();
    if s.is_empty() { "folder".to_owned() } else { s }
}

/// `mailbox export`: one mbox file per folder, in a directory that does not exist yet (nothing is overwritten).
async fn mailbox_export(env: &mut CtlEnv<'_>, addr: &str, dir: &std::path::Path) -> Result<Outcome, CtlError> {
    let (mb, _) = mailbox_of(env, addr).await?;
    if dir.exists() {
        return Err(CtlError::runtime("the directory already exists; export into a new one"));
    }
    std::fs::create_dir_all(dir).map_err(|e| CtlError::runtime(format!("cannot create the directory: {e}")))?;
    let folders = mb.list_folders().await.map_err(|e| map_error(&e, "mailbox"))?;
    let (mut total, mut report, mut used) = (0u64, Vec::new(), std::collections::HashSet::new());
    for f in &folders {
        let mut file = file_name_of(&f.name);
        while !used.insert(file.to_ascii_lowercase()) {
            file.push('_');
        }
        let mut out: Vec<u8> = Vec::new();
        let (mut count, mut after) = (0u64, 0u32);
        loop {
            let page = mb.list_folder(f.id, after, 500).await.map_err(|e| map_error(&e, "mailbox"))?;
            let Some(last) = page.last() else { break };
            after = last.uid;
            for m in &page {
                let raw = mb.get_raw(m.id).await.map_err(|e| map_error(&e, "message"))?;
                crate::mbox::write_message(&mut out, m.internal_date, &raw);
                count += 1;
            }
        }
        std::fs::write(dir.join(format!("{file}.mbox")), &out).map_err(|e| CtlError::runtime(format!("cannot write the file: {e}")))?;
        total += count;
        report.push(json!({ "folder": f.name, "file": format!("{file}.mbox"), "messages": count }));
    }
    let text = report.iter().map(|r| format!("{}: {} messages -> {}", r["folder"].as_str().unwrap_or(""), r["messages"], r["file"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n");
    Ok(Outcome::new(format!("{text}\n{total} messages exported to {}", dir.display()), json!({ "messages": total, "folders": report })))
}

/// The most an mbox file may hold for an import (it is read whole).
const MAX_IMPORT_BYTES: u64 = 1 << 30;

/// `mailbox import`: the messages of an mbox file are added to a folder (made when it is missing). A message that cannot be stored (too large, not a message) is counted and skipped.
async fn mailbox_import(env: &mut CtlEnv<'_>, addr: &str, file: &std::path::Path, folder: Option<&str>) -> Result<Outcome, CtlError> {
    let (mb, _) = mailbox_of(env, addr).await?;
    let size = std::fs::metadata(file).map_err(|e| CtlError::runtime(format!("cannot read the file: {e}")))?.len();
    if size > MAX_IMPORT_BYTES {
        return Err(CtlError::runtime("the file is larger than 1 GiB; split it"));
    }
    let bytes = std::fs::read(file).map_err(|e| CtlError::runtime(format!("cannot read the file: {e}")))?;
    let folders = mb.list_folders().await.map_err(|e| map_error(&e, "mailbox"))?;
    let target = match find_folder(&folders, folder) {
        Some(f) => f,
        None => mb.create_folder(None, folder.unwrap_or("inbox")).await.map_err(|e| map_error(&e, "folder"))?,
    };
    let (mut stored, mut skipped) = (0u64, 0u64);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    for raw in crate::mbox::split(&bytes) {
        match mb.append_dated(target, bytes::Bytes::from(raw), rusty_core::Flags::default(), now).await {
            Ok(_) => stored += 1,
            Err(_) => skipped += 1,
        }
    }
    Ok(Outcome::new(format!("{stored} messages imported, {skipped} skipped"), json!({ "imported": stored, "skipped": skipped })).failing(false))
}

/// `blobs verify`: every blob the database still references must have its file. A read only. Exit status 1 when one is missing or a shard could not be read.
async fn blobs_verify(env: &mut CtlEnv<'_>) -> Result<Outcome, CtlError> {
    let (Some(blobs), Some(gc)) = (env.blobs, env.gc) else {
        return Err(CtlError::runtime("the blob store is not available here"));
    };
    // Every blob store that has rows: the platform store, and (with a router) each tenant's store or pool.
    let mut targets: Vec<(rusty_store::repo::StoreId, Option<std::sync::Arc<dyn rusty_store::BlobStore>>)> = vec![(rusty_store::repo::PLATFORM_STORE, None)];
    if let Some(router) = env.stores {
        for id in gc.store_ids().await.map_err(|e| map_error(&e, "blob store"))? {
            if id != rusty_store::repo::PLATFORM_STORE {
                targets.push((id, router.store(id).await.ok()));
            }
        }
    }
    let many = targets.len() > 1;
    let mut lines = Vec::new();
    let mut per_store = Vec::new();
    let (mut referenced, mut stored, mut unreferenced, mut missing_total) = (0u64, 0u64, 0u64, 0u64);
    let mut clean = true;
    for (id, opened) in targets {
        let label = if id == rusty_store::repo::PLATFORM_STORE { "platform store".to_owned() } else { format!("store {}", id.0) };
        let r = match (id == rusty_store::repo::PLATFORM_STORE, &opened) {
            (true, _) => gc.verify_blobs(blobs).await,
            (false, Some(s)) => gc.for_store(id).verify_blobs(&**s).await,
            (false, None) => {
                lines.push(format!("{label}: NOT CHECKED: the store could not be opened here (unsealing key, settings, or a backend that is not ready)"));
                per_store.push(json!({ "store": id.0, "checked": false }));
                clean = false;
                continue;
            }
        }
        .map_err(|e| map_error(&e, "blob store"))?;
        lines.push(format!(
            "{}{} blobs referenced by the database, {} files in the store, {} files nothing references (garbage collection removes them)",
            if many { format!("{label}: ") } else { String::new() },
            r.referenced,
            r.stored,
            r.unreferenced_files
        ));
        if r.missing_total > 0 {
            lines.push(format!("MISSING{}: {} referenced blobs have no file (first {}):", if many { format!(" in the {label}") } else { String::new() }, r.missing_total, r.missing.len()));
            lines.extend(r.missing.iter().map(|h| format!("  {h}")));
        }
        lines.extend(r.errors.iter().map(|e| format!("could not check shard {e}{}", if many { format!(" of the {label}") } else { String::new() })));
        referenced += r.referenced;
        stored += r.stored;
        unreferenced += r.unreferenced_files;
        missing_total += r.missing_total;
        clean &= r.is_clean();
        per_store.push(json!({ "store": id.0, "checked": true, "referenced": r.referenced, "stored": r.stored, "unreferenced_files": r.unreferenced_files, "missing_total": r.missing_total, "missing": r.missing, "errors": r.errors, "ok": r.is_clean() }));
    }
    lines.push(if clean { "result: every referenced blob exists".to_owned() } else { "result: a blob store is missing data or could not be read".to_owned() });
    Ok(Outcome::new(
        lines.join("\n"),
        json!({ "referenced": referenced, "stored": stored, "unreferenced_files": unreferenced, "missing_total": missing_total, "ok": clean, "stores": per_store }),
    )
    .failing(!clean))
}

fn admin_store<'a>(
env: &CtlEnv<'a>) -> Result<&'a rusty_store::AdminStore, CtlError> {
    env.admins.ok_or_else(|| CtlError::runtime("administrator grants are not available here"))
}

/// `admin grant <addr> [domain]`: let an account use the admin web, for every domain (no domain given) or for one. This is also the recovery path
/// when no administrator can sign in. A grant needs an account that can sign in (it must have a password), and a domain administrator must be an
/// account of that domain (the database refuses anything else).
async fn admin_grant(env: &mut CtlEnv<'_>, addr: &str, domain: Option<&str>) -> Result<Outcome, CtlError> {
    let admins = admin_store(env)?;
    let account = account_named(env.directory, addr).await?;
    if !account.has_password {
        return Err(CtlError::runtime("this account has no password and cannot sign in: set one with `account set-password` first"));
    }
    let dom = match domain {
        Some(d) => Some(domain_named(env.directory, d).await?),
        None => None,
    };
    let g = admins.grant(account.id, dom.as_ref().map(|d| d.id)).await.map_err(|e| map_error(&e, "administrator"))?;
    let scope_text = dom.as_ref().map_or_else(|| "every domain (platform administrator)".to_owned(), |d| format!("the domain {} only", d.name));
    Ok(Outcome::new(
        format!("{} may now administer {scope_text}", addr.trim()),
        json!({ "account": addr.trim(), "scope": dom.as_ref().map_or("platform", |_| "domain"), "domain": dom.as_ref().map(|d| d.name.clone()), "created_at": g.created_at }),
    ))
}

/// `admin revoke <addr>`: the account stays, it is no longer an administrator (its open admin sessions end at their next request).
async fn admin_revoke(env: &mut CtlEnv<'_>, addr: &str) -> Result<Outcome, CtlError> {
    let admins = admin_store(env)?;
    let account = account_named(env.directory, addr).await?;
    let removed = admins.revoke(account.id).await.map_err(|e| map_error(&e, "administrator"))?;
    Ok(Outcome::new(
        if removed { format!("{} is no longer an administrator", addr.trim()) } else { format!("{} was not an administrator", addr.trim()) },
        json!({ "account": addr.trim(), "revoked": removed }),
    ))
}

/// `admin list`: every administrator with the domain they administer (`*` for the platform).
async fn admin_list(env: &mut CtlEnv<'_>) -> Result<Outcome, CtlError> {
    let admins = admin_store(env)?;
    let mut grants = Vec::new();
    let mut after = None;
    loop {
        let page = admins.list(after, 200).await.map_err(|e| map_error(&e, "administrators"))?;
        after = page.last().map(|g| g.account);
        let n = page.len();
        grants.extend(page);
        if n < 200 {
            break;
        }
    }
    // Address and domain names by id: one pass over the directory (a CLI listing, not a hot path).
    let mut names = std::collections::HashMap::new();
    let mut domains = std::collections::HashMap::new();
    for dom in all_domains(env.directory).await? {
        for a in all_accounts(env.directory, &dom).await? {
            names.insert(a.id, format!("{}@{}", a.local, dom.name));
        }
        domains.insert(dom.id, dom.name.clone());
    }
    let rows: Vec<(String, String)> = grants
        .iter()
        .map(|g| (names.get(&g.account).cloned().unwrap_or_else(|| "?".into()), g.domain.map_or_else(|| "*".to_owned(), |d| domains.get(&d).cloned().unwrap_or_else(|| "?".into()))))
        .collect();
    let text: Vec<String> = rows.iter().map(|(a, d)| format!("{a}\t{d}")).collect();
    let j: Vec<Value> = rows.iter().map(|(a, d)| json!({ "account": a, "domain": if d == "*" { Value::Null } else { json!(d) } })).collect();
    Ok(Outcome::new(text.join("\n"), json!({ "administrators": j })))
}

fn read_password(env: &mut CtlEnv<'_>, source: PasswordSource) -> Result<Option<Secret>, CtlError> {
    if source == PasswordSource::None {
        return Ok(None);
    }
    let s = env.passwords.read(source)?;
    if s.expose().len() < MIN_PASSWORD_BYTES {
        return Err(CtlError::runtime(format!(
            "password: too short (at least {MIN_PASSWORD_BYTES} bytes)"
        )));
    }
    Ok(Some(s))
}

/// Run one command.
pub async fn execute(command: &Command, env: &mut CtlEnv<'_>) -> Result<Outcome, CtlError> {
    match command {
        Command::Help => Ok(Outcome::new(crate::args::USAGE, json!({ "usage": crate::args::USAGE }))),

        Command::DomainAdd { name } => {
            let d = env.directory;
            d.create_domain(name).await.map_err(|e| map_error(&e, "domain"))?;
            let info = domain_named(d, name).await?;
            Ok(Outcome::new(
                format!(
                    "domain {} created\nnext: rustyctl account add postmaster@{} --prompt",
                    info.name, info.name
                ),
                json!({ "domain": info.name, "status": info.status.as_str() }),
            ))
        }

        Command::DomainList => {
            let list = all_domains(env.directory).await?;
            let text: Vec<String> =
                list.iter().map(|x| format!("{}\t{}", x.name, x.status.as_str())).collect();
            let j: Vec<Value> =
                list.iter().map(|x| json!({ "name": x.name, "status": x.status.as_str() })).collect();
            Ok(Outcome::new(text.join("\n"), json!({ "domains": j })))
        }

        Command::DomainDisable { name } | Command::DomainEnable { name } => {
            let status = if matches!(command, Command::DomainDisable { .. }) {
                DomainStatus::Disabled
            } else {
                DomainStatus::Active
            };
            let info = domain_named(env.directory, name).await?;
            env.directory
                .set_domain_status(info.id, status)
                .await
                .map_err(|e| map_error(&e, "domain"))?;
            Ok(Outcome::new(
                format!("domain {} is now {}", info.name, status.as_str()),
                json!({ "domain": info.name, "status": status.as_str() }),
            ))
        }

        Command::AccountAdd { addr, password } => {
            let (local, domain) = parse_address(addr).map_err(|e| map_error(&e, "address"))?;
            let secret = read_password(env, *password)?;
            let d = env.directory;
            let dom = domain_named(d, &domain).await?;
            let first = d
                .list_accounts(dom.id, None, 1)
                .await
                .map_err(|e| map_error(&e, "account"))?
                .is_empty();
            d.create_account(&local, dom.id, secret.as_ref().map(Secret::expose), None)
                .await
                .map_err(|e| match e {
                    Error::Conflict(_) => CtlError::runtime(
                        "account: the address is already used by an account, an alias or a list \
                         (`alias remove` frees an alias)",
                    ),
                    other => map_error(&other, "account"),
                })?;
            let acc = account_named(d, &format!("{local}@{domain}")).await?;
            // The first account of a domain also receives the mandatory role addresses, unless
            // it is one of them or they exist. Aliases need a target, so this cannot happen
            // when the domain is created.
            let mut aliased = Vec::new();
            if first {
                for name in REQUIRED_ALIASES {
                    if name == local {
                        continue;
                    }
                    match d.add_alias(name, dom.id, acc.id).await {
                        Ok(()) => aliased.push(format!("{name}@{domain}")),
                        Err(Error::Conflict(_)) => {}
                        Err(e) => return Err(map_error(&e, "alias")),
                    }
                }
            }
            let mut text = format!("account {local}@{domain} created");
            if secret.is_none() {
                text.push_str("\nno password set: the account cannot log in until `account set-password`");
            }
            if !aliased.is_empty() {
                text.push_str(&format!("\naliases created: {}", aliased.join(", ")));
            }
            Ok(Outcome::new(
                text,
                json!({
                    "account": format!("{local}@{domain}"),
                    "has_password": secret.is_some(),
                    "aliases_created": aliased,
                }),
            ))
        }

        Command::AccountList { domain } => {
            let d = env.directory;
            let mut rows = Vec::new();
            for dom in scope(d, domain).await? {
                for a in all_accounts(d, &dom).await? {
                    rows.push((format!("{}@{}", a.local, dom.name), a));
                }
            }
            let text: Vec<String> = rows
                .iter()
                .map(|(addr, a)| {
                    format!("{addr}\t{}\t{}", status_word(a.status), if a.has_password { "password" } else { "no-password" })
                })
                .collect();
            let j: Vec<Value> = rows
                .iter()
                .map(|(addr, a)| json!({ "address": addr, "status": status_word(a.status), "has_password": a.has_password }))
                .collect();
            Ok(Outcome::new(text.join("\n"), json!({ "accounts": j })))
        }

        Command::AccountSetPassword { addr, password } => {
            let secret = read_password(env, *password)?
                .ok_or_else(|| CtlError::usage("set-password needs --password-stdin or --prompt"))?;
            let acc = account_named(env.directory, addr).await?;
            env.directory
                .set_password(acc.id, secret.expose())
                .await
                .map_err(|e| map_error(&e, "password"))?;
            Ok(Outcome::new(format!("password of {} changed", addr.trim()), json!({ "account": addr.trim(), "changed": true })))
        }

        Command::AccountLock { addr } | Command::AccountDisable { addr } | Command::AccountEnable { addr } => {
            let status = match command {
                Command::AccountLock { .. } => AccountStatus::Locked,
                Command::AccountDisable { .. } => AccountStatus::Disabled,
                _ => AccountStatus::Active,
            };
            let acc = account_named(env.directory, addr).await?;
            env.directory
                .set_status(acc.id, status)
                .await
                .map_err(|e| map_error(&e, "account"))?;
            Ok(Outcome::new(
                format!("account {} is now {}", addr.trim(), status_word(status)),
                json!({ "account": addr.trim(), "status": status_word(status) }),
            ))
        }

        Command::AliasAdd { alias, target } => {
            let d = env.directory;
            let (local, domain) = parse_address(alias).map_err(|e| map_error(&e, "alias"))?;
            let dom = domain_named(d, &domain).await?;
            let acc = account_named(d, target).await?;
            // Domain = tenant: an alias may not point into another domain's account.
            if acc.domain != dom.id {
                return Err(CtlError::runtime("alias: the target account must be in the same domain"));
            }
            d.add_alias(&local, dom.id, acc.id).await.map_err(|e| match e {
                Error::Conflict(_) => CtlError::runtime("alias: the address is already used by an account, an alias or a list"),
                other => map_error(&other, "alias"),
            })?;
            Ok(Outcome::new(
                format!("alias {local}@{domain} -> {}", target.trim()),
                json!({ "alias": format!("{local}@{domain}"), "target": target.trim() }),
            ))
        }

        Command::AliasList { domain } => {
            let d = env.directory;
            let mut rows = Vec::new();
            for dom in scope(d, domain).await? {
                for a in all_aliases(d, &dom).await? {
                    rows.push((format!("{}@{}", a.local, dom.name), format!("{}@{}", a.account_local, a.account_domain)));
                }
            }
            let text: Vec<String> = rows.iter().map(|(a, t)| format!("{a}\t{t}")).collect();
            let j: Vec<Value> = rows.iter().map(|(a, t)| json!({ "alias": a, "target": t })).collect();
            Ok(Outcome::new(text.join("\n"), json!({ "aliases": j })))
        }

        Command::AliasRemove { alias } => {
            env.directory.remove_alias(alias).await.map_err(|e| map_error(&e, "alias"))?;
            Ok(Outcome::new(format!("alias {} removed", alias.trim()), json!({ "alias": alias.trim(), "removed": true })))
        }

        Command::DkimGenerate { domain, selector } => dkim_generate(env, domain, selector).await,

        Command::DkimGenerateSealed { domain, selector } => dkim_generate_sealed(env, domain, selector).await,

        Command::DkimImport { domain, selector, key_file } => dkim_import(env, domain, selector, key_file).await,

        Command::DomainRelaySet { .. } => domain_relay_set(env, command).await,

        Command::DomainRelayShow { name } => domain_relay_show(env, name).await,

        Command::StorageSet { .. } => storage_set(env, command).await,

        Command::StorageShow { name } => storage_show(env, name).await,

        Command::StorageCheck { name } => storage_check(env, name).await,

        Command::StoragePool { name, policy } => storage_pool(env, name, *policy).await,

        Command::StorageBackendAdd { name, backend, endpoint, region, bucket, prefix, path_style, weight } => {
            storage_backend_add(env, name, backend, endpoint, region, bucket, prefix, *path_style, *weight).await
        }

        Command::StorageBackendRekey { name, backend } => storage_backend_rekey(env, name, backend).await,

        Command::StorageBackendSet { name, backend, state, weight } => storage_backend_set(env, name, backend, *state, *weight).await,

        Command::StorageBackendRemove { name, backend } => storage_backend_remove(env, name, backend).await,

        Command::BlobsVerify => blobs_verify(env).await,

        Command::MailboxDeleted { addr } => mailbox_deleted(env, addr).await,

        Command::MailboxRecover { addr, id, folder } => mailbox_recover(env, addr, *id, folder.as_deref()).await,

        Command::MailboxExport { addr, dir } => mailbox_export(env, addr, dir).await,

        Command::MailboxImport { addr, file, folder } => mailbox_import(env, addr, file, folder.as_deref()).await,

        Command::AdminGrant { addr, domain } => admin_grant(env, addr, domain.as_deref()).await,

        Command::AdminRevoke { addr } => admin_revoke(env, addr).await,

        Command::AdminList => admin_list(env).await,

        Command::DomainRelayCheck { name } => domain_relay_check(env, name).await,

        Command::DomainRelayRemove { name } => domain_relay_remove(env, name).await,

        Command::SealKeygen { file } => seal_keygen(file),

        Command::SealShow { file } => seal_show(file),
        Command::DomainDns { name } => domain_dns(env, name).await,

        Command::DomainCheck { name } => match env.live_dns {
            Some(live) => check_domain(env, live, name).await,
            None => Err(CtlError::runtime("no DNS resolver is available for this command")),
        },

        Command::Check => check(env).await,
        Command::DbMigrate { .. } | Command::DbStatus | Command::RelayCheck | Command::BlobsInit => {
            Err(CtlError::runtime("db and relay commands are run by the composition root, not against the directory"))
        }
    }
}

/// The existing domain and the configured key directory (created 0700 if missing) for a DKIM command.
async fn dkim_target<'e>(env: &CtlEnv<'e>, domain: &str) -> Result<(DomainInfo, &'e std::path::Path), CtlError> {
    let dom = env.directory.find_domain(domain).await.map_err(|e| map_error(&e, "domain"))?.ok_or_else(|| CtlError::runtime("no such domain; add it first with `domain add`"))?;
    let dir = env.dkim_dir.ok_or_else(|| CtlError::runtime("set [outbound] dkim_dir in the configuration: that is where DKIM keys are kept"))?;
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(CtlError::runtime("dkim_dir is not a directory")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(dir).map_err(|_| CtlError::runtime("cannot create dkim_dir"))?;
        }
        Err(_) => return Err(CtlError::runtime("cannot inspect dkim_dir")),
    }
    Ok((dom, dir))
}

/// The text and JSON for a key: where it is and the TXT record to publish. Never the private key. `created` is false when the
/// key was already there (a repeated `dkim generate`): nothing was changed and the record is shown again.
fn dkim_outcome(dom: &DomainInfo, key: &rusty_smtp::dkim::GeneratedKey, created: bool, note: &str) -> Outcome {
    let name = format!("{}._domainkey.{}", key.selector, dom.name);
    let algorithm = dkim_algorithm(key);
    let mut text = if created {
        format!("key written to {} (mode 0600)\nPublish this TXT record:\n{name}. IN TXT \"{}\"", key.key_file.display(), key.txt_value())
    } else {
        format!(
            "a key already exists at {} (nothing was changed; a published key is never replaced, a new key needs a new --selector)\nIts TXT record:\n{name}. IN TXT \"{}\"",
            key.key_file.display(),
            key.txt_value()
        )
    };
    if !note.is_empty() {
        text.push('\n');
        text.push_str(note);
    }
    Outcome::new(
        text,
        json!({ "domain": dom.name, "algorithm": algorithm, "selector": key.selector, "record_name": name, "record_value": key.txt_value(), "key_file": key.key_file.display().to_string(), "created": created }),
    )
}

fn dkim_algorithm(key: &rusty_smtp::dkim::GeneratedKey) -> &'static str {
    use rusty_smtp::dkim::DkimAlgo;
    match key.algo {
        DkimAlgo::Rsa => "rsa",
        DkimAlgo::Ed25519 => "ed25519",
    }
}

/// Keep the public half of a key in the database (idempotent; the first write stands). A key whose file was made before
/// this existed is recorded the next time the same command is run.
async fn keep_public_half(env: &CtlEnv<'_>, dom: &DomainInfo, key: &rusty_smtp::dkim::GeneratedKey) -> Result<(), CtlError> {
    env.dns.add_dkim_key(dom.id, &key.selector, dkim_algorithm(key), &key.public_key_b64).await.map(|_| ()).map_err(|e| map_error(&e, "dkim key"))
}

/// `dkim generate`: an ed25519 key for an existing domain, written 0600 into the configured key directory, its public half
/// kept in the database, and the DNS record to publish. Never replaces a key and never prints the private key: run again
/// with the same selector it succeeds, changes nothing and shows the record of the key that is there. RSA keys are
/// imported instead (see `rusty_smtp::dkim::generate_key` for why Rusty does not make them).
async fn dkim_generate(env: &mut CtlEnv<'_>, domain: &str, selector: &str) -> Result<Outcome, CtlError> {
    use rusty_smtp::dkim::DkimAlgo;
    let (dom, dir) = dkim_target(env, domain).await?;
    let (key, created) = match rusty_smtp::dkim::generate_key(dir, &dom.name, selector) {
        Ok(k) => (k, true),
        Err(Error::Conflict(_)) => match rusty_smtp::dkim::existing_key(dir, &dom.name, selector, DkimAlgo::Ed25519).map_err(|e| map_error(&e, "dkim key"))? {
            Some(k) => (k, false),
            None => return Err(CtlError::runtime("cannot create the key")),
        },
        Err(e) => return Err(map_error(&e, "dkim key")),
    };
    keep_public_half(env, &dom, &key).await?;
    let rsa_name = format!("{}.{selector}.pem", dom.name);
    let note = if !created || dir.join(&rsa_name).exists() {
        String::new()
    } else {
        format!(
            "Note: some large receivers verify only RSA signatures. Add an RSA key as well:\n  openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out dkim-rsa.pem\n  rustyctl dkim import {} dkim-rsa.pem --selector {selector}   (then delete dkim-rsa.pem)",
            dom.name
        )
    };
    Ok(dkim_outcome(&dom, &key, created, &note))
}

/// `dkim generate --sealed`: an ed25519 key for an existing domain whose private half is **sealed** to `[sealing] public_key` and stored in the
/// database; no file is made and no key directory is needed. The public half goes to `domain_dkim_keys`. Like the file command it never replaces a
/// key: run again with the same selector it succeeds, changes nothing and shows the record of the key that is there. A key of this selector that
/// was made as a **file** (public row only) cannot be sealed from here: the private key is somewhere else.
async fn dkim_generate_sealed(env: &mut CtlEnv<'_>, domain: &str, selector: &str) -> Result<Outcome, CtlError> {
    let Some(seal) = env.sealing else {
        return Err(CtlError::runtime("set [sealing] public_key in the configuration: that is the key the private key is sealed to (`rustyctl seal keygen` makes one)"));
    };
    let Some(writer) = env.sealed else {
        return Err(CtlError::runtime("sealed secrets are not available here"));
    };
    let dom = find_domain(env, domain).await?;
    let published = format!("{selector}-ed25519");
    let existing = env.dns.dkim_keys(dom.id).await.map_err(|e| map_error(&e, "dkim keys"))?.into_iter().find(|k| k.selector.eq_ignore_ascii_case(&published));
    let purpose = rusty_smtp::dkim::sealed_purpose(&published);
    let has_sealed = writer.meta("domain", dom.id.0).await.map_err(|e| map_error(&e, "sealed secrets"))?.iter().any(|m| m.purpose == purpose);
    if let Some(k) = existing {
        if !has_sealed {
            return Err(CtlError::runtime("a key with this selector exists but is not sealed here (its private key was made as a file); use another --selector"));
        }
        return Ok(sealed_outcome(&dom, &k.selector, &k.algorithm, &k.public_key_b64, false));
    }
    // No public row yet: nothing was ever published for this selector, so a sealed secret left by an interrupted run is safely replaced.
    let key = rusty_smtp::dkim::generate_sealed(dom.id.0, selector, seal).map_err(|e| map_error(&e, "dkim key"))?;
    writer.put("domain", dom.id.0, &key.purpose, &key.key_id, &key.sealed).await.map_err(|e| map_error(&e, "sealed secrets"))?;
    env.dns.add_dkim_key(dom.id, &key.selector, "ed25519", &key.public_key_b64).await.map_err(|e| map_error(&e, "dkim key"))?;
    Ok(sealed_outcome(&dom, &key.selector, "ed25519", &key.public_key_b64, true))
}

fn sealed_outcome(dom: &DomainInfo, selector: &str, algorithm: &str, public_key_b64: &str, created: bool) -> Outcome {
    let name = format!("{selector}._domainkey.{}", dom.name);
    let value = format!("v=DKIM1; k={algorithm}; p={public_key_b64}");
    let text = if created {
        format!("key made and sealed in the database (no file); the private key was never shown\nPublish this TXT record:\n{name}. IN TXT \"{value}\"")
    } else {
        format!("a sealed key already exists (nothing was changed; a published key is never replaced, a new key needs a new --selector)\nIts TXT record:\n{name}. IN TXT \"{value}\"")
    };
    Outcome::new(
        text,
        json!({ "domain": dom.name, "algorithm": algorithm, "selector": selector, "record_name": name, "record_value": value, "created": created, "sealed": true }),
    )
}

/// The largest key file `dkim import` reads.
const MAX_KEY_FILE: u64 = 64 * 1024;

/// `dkim import`: an operator-made private key (RSA 2048-4096 bits or ed25519, unencrypted PEM) for an existing
/// domain, checked, written 0600 under the name the signer uses, and the DNS record to publish. The source
/// file is only read; the operator deletes it.
async fn dkim_import(env: &mut CtlEnv<'_>, domain: &str, selector: &str, key_file: &std::path::Path) -> Result<Outcome, CtlError> {
    use std::io::Read;
    let (dom, dir) = dkim_target(env, domain).await?;
    // A regular file only (checked before opening, so a FIFO or device never blocks or streams forever).
    let regular = |m: std::io::Result<std::fs::Metadata>| m.is_ok_and(|m| m.is_file());
    if !regular(std::fs::metadata(key_file)) {
        return Err(CtlError::runtime("the key file is not a readable regular file"));
    }
    let f = std::fs::File::open(key_file).map_err(|_| CtlError::runtime("cannot open the key file"))?;
    if !regular(f.metadata()) {
        return Err(CtlError::runtime("the key file is not a readable regular file"));
    }
    let mut pem = Vec::new();
    f.take(MAX_KEY_FILE + 1).read_to_end(&mut pem).map_err(|_| CtlError::runtime("cannot read the key file"))?;
    if pem.len() as u64 > MAX_KEY_FILE {
        return Err(CtlError::runtime("the key file is too large to be one private key"));
    }
    let key = rusty_smtp::dkim::import_key(dir, &dom.name, selector, &pem).map_err(|e| map_error(&e, "dkim key"))?;
    keep_public_half(env, &dom, &key).await?;
    Ok(dkim_outcome(&dom, &key, true, "The source key file is no longer needed by Rusty; delete it or keep it offline."))
}

/// `seal keygen <file>`: make the key pair that seals and opens secrets (admin web design, section 14.1). The private half is written to
/// `file` (an absolute path in a directory that exists), mode 0600, **never replacing** a file (everything sealed to the old key would be
/// lost); it is never printed. The public half and its id are printed with the two configuration sections to add.
/// `seal keygen` and `seal show` work on a key file only: they need no configuration and no database (an installer makes the key before anything exists).
pub fn run_seal_file_command(command: &Command) -> Option<Result<Outcome, CtlError>> {
    match command {
        Command::SealKeygen { file } => Some(seal_keygen(file)),
        Command::SealShow { file } => Some(seal_show(file)),
        _ => None,
    }
}

fn seal_keygen(file: &std::path::Path) -> Result<Outcome, CtlError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if !file.is_absolute() {
        return Err(CtlError::runtime("the key file must be an absolute path (the configuration names it the same way)"));
    }
    let (secret, public) = rusty_seal::generate();
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(file).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => CtlError::runtime("the key file already exists; it is never replaced (sealed secrets would become unreadable)"),
        std::io::ErrorKind::NotFound => CtlError::runtime("the directory of the key file does not exist"),
        _ => CtlError::runtime("cannot create the key file"),
    })?;
    let mut text = secret.to_text().as_str().to_owned();
    text.push('\n');
    let wrote = f.write_all(text.as_bytes()).and_then(|()| f.sync_all());
    zeroize_string(&mut text);
    if wrote.is_err() {
        let _ = std::fs::remove_file(file);
        return Err(CtlError::runtime("cannot write the key file"));
    }
    Ok(seal_outcome(file, &public, true))
}

/// `seal show <file>`: the public half and id of the key in `file`. The file must be a regular file (never a symlink) that only its owner
/// can read, like every key file.
fn seal_show(file: &std::path::Path) -> Result<Outcome, CtlError> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(file).map_err(|_| CtlError::runtime("cannot read the key file"))?;
    if !meta.is_file() {
        return Err(CtlError::runtime("the key file is not a regular file"));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(CtlError::runtime("the key file must be readable by its owner only (mode 0600)"));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(file).and_then(|f| f.take(4096).read_to_end(&mut bytes)).map_err(|_| CtlError::runtime("cannot read the key file"))?;
    let key = rusty_seal::UnsealingKey::from_file_bytes(bytes).map_err(|_| CtlError::runtime("the file does not hold an unsealing key"))?;
    Ok(seal_outcome(file, &key.public(), false))
}

fn zeroize_string(s: &mut String) {
    // Overwrite the text before it is freed (best effort: the allocator may still hold copies, see rusty-seal).
    let mut bytes = std::mem::take(s).into_bytes();
    bytes.fill(0);
}

fn seal_outcome(file: &std::path::Path, public: &rusty_seal::SealingKey, created: bool) -> Outcome {
    let key_id = public.id().to_hex();
    let text = format!(
        "{} {}\nkey id: {key_id}\npublic key: {}\n\nAdd to the configuration of the service that SEALS (the admin service, rustyctl):\n[sealing]\npublic_key = \"{}\"\n\nAdd to the configuration of the OUTBOUND service (it opens what was sealed):\n[unsealing]\nkey_files = [\"{}\"]\n\nKeep the key file safe and backed up: without it, what was sealed to it cannot be opened.",
        if created { "private key written to" } else { "key file:" },
        file.display(),
        public.to_text(),
        public.to_text(),
        file.display(),
    );
    Outcome::new(text, json!({ "key_file": file.display().to_string(), "key_id": key_id, "public_key": public.to_text(), "created": created }))
}

/// `check`: the configuration that was read and the DNS records each active domain needs.
/// Nothing is queried: these are hints to publish, not a verification.
async fn check(env: &mut CtlEnv<'_>) -> Result<Outcome, CtlError> {
    let domains = all_domains(env.directory).await?;
    let mut lines = vec![format!("configuration: {}", env.config_note)];
    let mut jd = Vec::new();
    match env.hostname {
        Some(h) => lines.push(format!("hostname: {h}")),
        None => lines.push("hostname: not set (required outside the dev profile)".to_owned()),
    }
    for dom in domains.iter().filter(|d| d.status == DomainStatus::Active) {
        // The records come from the pure generator (rusty_core::dns), the same one the web setup page will
        // use; this command only formats them. Without a mail host name there is nothing to point them at.
        if env.hostname.is_none() {
            continue;
        }
        let (records, _) = records_of(env, dom).await?;
        lines.push(format!("domain {}:", dom.name));
        lines.extend(records.iter().map(|r| format!("  {}", zone_line(r))));
        let j: Vec<Value> = records
            .iter()
            .map(|r| {
                json!({
                    "name": r.name, "type": r.rtype.as_str(), "value": r.value, "ttl": r.ttl_hint,
                    "purpose": r.purpose, "required": format!("{:?}", r.required).to_lowercase(),
                    "zone_line": zone_line(r),
                })
            })
            .collect();
        jd.push(json!({ "domain": dom.name, "records": j }));
    }
    lines.push("reverse DNS (PTR) of the sending address must match the hostname; it is set at your provider".to_owned());
    Ok(Outcome::new(
        lines.join("\n"),
        json!({ "configuration": env.config_note, "hostname": env.hostname, "domains": jd }),
    ))
}

/// Failed checks of a domain trigger an alert after this many in a row (the drift rule of `rusty_core::dns_state`).
const ALERT_AFTER: u32 = 3;

/// The records a domain needs, and the warnings about what the configuration leaves out. The keys come from the database
/// (their public halves; the private ones are never read here), the addresses and the relay's SPF include from `[outbound]`.
async fn records_of(env: &CtlEnv<'_>, dom: &DomainInfo) -> Result<(Vec<DnsRecord>, Vec<&'static str>), CtlError> {
    let Some(host) = env.hostname else {
        return Err(CtlError::runtime("set hostname in the configuration: the DNS records point at it"));
    };
    let stored = env.dns.dkim_keys(dom.id).await.map_err(|e| map_error(&e, "dkim keys"))?;
    let keys: Vec<(&str, &str, &str)> = stored.iter().map(|k| (k.selector.as_str(), k.algorithm.as_str(), k.public_key_b64.as_str())).collect();
    let facts = rusty_core::dns::MailFacts { mail_host: host, public_ips: env.outbound.public_ips, spf_include: env.outbound.spf_include };
    rusty_core::dns::records_with_facts(&dom.name, &facts, &keys).map_err(|e| map_error(&e, "dns records"))
}

fn warning_text(code: &str) -> &'static str {
    match code {
        "no_public_ips" => "[outbound] public_ips is not set: the address records and the SPF record do not name the addresses mail leaves from",
        _ => "see the documentation",
    }
}

async fn find_domain(env: &CtlEnv<'_>, name: &str) -> Result<DomainInfo, CtlError> {
    env.directory.find_domain(name).await.map_err(|e| map_error(&e, "domain"))?.ok_or_else(|| CtlError::runtime("no such domain"))
}

fn record_json(r: &DnsRecord) -> Value {
    json!({
        "name": r.name, "type": r.rtype.as_str(), "value": r.value, "ttl": r.ttl_hint,
        "purpose": r.purpose, "required": requirement(r.required), "zone_line": zone_line(r),
    })
}

fn requirement(r: Requirement) -> &'static str {
    match r {
        Requirement::Required => "required",
        Requirement::Recommended => "recommended",
        Requirement::Optional => "optional",
    }
}

/// `domain dns`: the records to publish for one domain (nothing is queried).
async fn domain_dns(env: &CtlEnv<'_>, name: &str) -> Result<Outcome, CtlError> {
    let dom = find_domain(env, name).await?;
    let (records, warnings) = records_of(env, &dom).await?;
    let mut lines: Vec<String> = records.iter().map(zone_line).collect();
    lines.extend(warnings.iter().map(|w| format!("warning: {}", warning_text(w))));
    Ok(Outcome::new(lines.join("\n"), json!({ "domain": dom.name, "records": records.iter().map(record_json).collect::<Vec<_>>(), "warnings": warnings })))
}

fn record_status_word(s: RecordStatus) -> &'static str {
    s.as_str()
}

/// `domain check`: the records of `domain` against what `src` says DNS publishes, recorded as drift state (a record that was
/// `ok` and now is not raises an alert after three checks in a row). The outcome is `failed` when a **required** record is
/// missing or wrong; a recommended or optional one that is wrong, and anything that could not be decided, is reported without
/// failing the run. The placeholder for a key that was not generated yet is shown but not recorded (it is no DNS record).
pub async fn check_domain<S: DnsSource>(env: &CtlEnv<'_>, src: &S, domain: &str) -> Result<Outcome, CtlError> {
    let dom = find_domain(env, domain).await?;
    let (expected, warnings) = records_of(env, &dom).await?;
    let opts = CheckOptions { via_relay: env.outbound.via_relay, ..CheckOptions::default() };
    let checks = check_records(src, &expected, &opts).await;
    let recorded: Vec<_> = checks.iter().filter(|c| !c.record.name.starts_with('<')).map(|c| c.observation()).collect();
    let steps = env.dns.record_check(dom.id, &recorded, ALERT_AFTER).await.map_err(|e| map_error(&e, "dns state"))?;
    let change_of = |c: &rusty_core::dns_check::RecordCheck| -> Option<String> {
        let key = c.key();
        steps.iter().find(|s| s.row.key == key).and_then(|s| s.change.as_ref()).and_then(|ch| ch.from.map(|f| format!("{} -> {}", record_status_word(f), record_status_word(ch.to))))
    };
    let alerts: Vec<String> = steps.iter().filter(|s| s.alert == rusty_core::dns_state::AlertChange::Raised).map(|s| s.row.key.name.clone()).collect();
    let failed = checks.iter().any(|c| c.record.required == Requirement::Required && matches!(c.status, RecordStatus::Missing | RecordStatus::Mismatch));
    let mut lines = vec![format!("{}: {} records checked against live DNS", dom.name, checks.len())];
    for c in &checks {
        let mut line = format!("  {:<9} {:<5} {}", record_status_word(c.status), c.record.rtype.as_str(), c.record.name);
        if !c.findings.is_empty() {
            line.push_str(&format!("  [{}]", c.findings.join(", ")));
        }
        if let Some(ch) = change_of(c) {
            line.push_str(&format!("  ({ch})"));
        }
        lines.push(line);
        if c.status != RecordStatus::Ok {
            if let Some(seen) = &c.seen {
                lines.push(format!("            DNS shows: {}", rusty_core::dns_state::clean_seen(seen)));
            }
        }
    }
    lines.extend(warnings.iter().map(|w| format!("warning: {}", warning_text(w))));
    lines.extend(alerts.iter().map(|n| format!("ALERT: {n} was ok and has failed {ALERT_AFTER} checks in a row")));
    lines.push(if failed { "result: a required record is missing or wrong".to_owned() } else { "result: every required record is correct".to_owned() });
    let records: Vec<Value> = checks
        .iter()
        .map(|c| {
            let mut v = record_json(&c.record);
            if let Some(o) = v.as_object_mut() {
                o.insert("status".into(), json!(record_status_word(c.status)));
                o.insert("seen".into(), json!(c.seen.as_deref().map(rusty_core::dns_state::clean_seen)));
                o.insert("findings".into(), json!(c.findings));
                o.insert("change".into(), json!(change_of(c)));
            }
            v
        })
        .collect();
    Ok(Outcome::new(lines.join("\n"), json!({ "domain": dom.name, "ok": !failed, "records": records, "warnings": warnings, "alerts": alerts })).failing(failed))
}

/// Text for the terminal: the plain text, or the JSON value when `json` is set.
pub fn render(outcome: &Outcome, json: bool) -> String {
    if json {
        outcome.json.to_string()
    } else {
        outcome.text.clone()
    }
}

// ------------------------------------------------------------------------------------------------ pools of backends (docs/design-s3-per-tenant.md, section 12)

fn pool_stores<'a>(env: &CtlEnv<'a>) -> Result<&'a rusty_store::BlobPoolAdmin, CtlError> {
    env.pools.ok_or_else(|| CtlError::runtime("blob pools are not available here"))
}

/// The two keys, one per line on standard input, each one printable line of at most 256 bytes.
fn read_keys(env: &mut CtlEnv<'_>) -> Result<(Secret, Secret), CtlError> {
    let access = env.passwords.read(PasswordSource::Stdin)?;
    let secret = env.passwords.read(PasswordSource::Stdin)?;
    for (what, v) in [("access key", access.expose()), ("secret key", secret.expose())] {
        if v.is_empty() || v.len() > 256 || v.chars().any(char::is_control) {
            return Err(CtlError::runtime(match what {
                "access key" => "the access key must be one printable line of at most 256 bytes",
                _ => "the secret key must be one printable line of at most 256 bytes",
            }));
        }
    }
    Ok((access, secret))
}

/// Seal both keys to the backend (owner kind `blobbackend`, the backend's id).
async fn seal_backend_keys(env: &CtlEnv<'_>, backend: i64, access: &Secret, secret: &Secret) -> Result<(), CtlError> {
    use rusty_core::storage_policy::{ACCESS_KEY_PURPOSE, BACKEND_SECRET_OWNER, SECRET_KEY_PURPOSE};
    let Some(seal) = env.sealing else {
        return Err(CtlError::runtime("set [sealing] public_key in the configuration: the credentials are sealed to it (`rustyctl seal keygen` makes one)"));
    };
    let Some(writer) = env.sealed else { return Err(CtlError::runtime("sealed secrets are not available here")) };
    for (purpose, value) in [(ACCESS_KEY_PURPOSE, access.expose()), (SECRET_KEY_PURPOSE, secret.expose())] {
        let context = rusty_seal::Context::new(BACKEND_SECRET_OWNER, backend, purpose).map_err(|e| map_error(&e, "backend credentials"))?;
        let sealed = seal.seal(&context, value.as_bytes()).map_err(|e| map_error(&e, "backend credentials"))?;
        writer.put(BACKEND_SECRET_OWNER, backend, purpose, &seal.id().to_hex(), &sealed).await.map_err(|e| map_error(&e, "backend credentials"))?;
    }
    Ok(())
}

async fn pool_of_domain(env: &CtlEnv<'_>, name: &str) -> Result<(rusty_directory::DomainInfo, rusty_store::repo::BlobStoreRow), CtlError> {
    let pools = pool_stores(env)?;
    let dom = find_domain(env, name).await?;
    let Some(row) = pools.pool_of(dom.id).await.map_err(|e| map_error(&e, "storage"))? else {
        return Err(CtlError::runtime("the domain has no pool: run `storage pool` first"));
    };
    Ok((dom, row))
}

async fn backend_of(env: &CtlEnv<'_>, store: rusty_store::repo::StoreId, backend: &str) -> Result<rusty_store::repo::BackendRow, CtlError> {
    let pools = pool_stores(env)?;
    pools.backends(store).await.map_err(|e| map_error(&e, "storage"))?.into_iter().find(|b| b.name == backend).ok_or_else(|| CtlError::runtime("the pool has no backend of that name"))
}

async fn storage_pool(env: &mut CtlEnv<'_>, name: &str, policy: rusty_store::PoolPolicy) -> Result<Outcome, CtlError> {
    let pools = pool_stores(env)?;
    let dom = find_domain(env, name).await?;
    let row = pools.set_policy(dom.id, policy).await.map_err(|e| map_error(&e, "storage"))?;
    Ok(Outcome::new(
        format!(
            "{} uses a pool (store {}): copies {}, sync copies {}; add backends with `storage backend add`, then `storage check` (the pool is used once every backend passed)",
            dom.name, row.id.0, policy.copies, policy.sync_copies
        ),
        json!({ "domain": dom.name, "store": row.id.0, "policy": serde_json::from_str::<Value>(&policy.to_json()).unwrap_or(Value::Null) }),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn storage_backend_add(env: &mut CtlEnv<'_>, name: &str, backend: &str, endpoint: &str, region: &str, bucket: &str, prefix: &str, path_style: bool, weight: u32) -> Result<Outcome, CtlError> {
    let s3 = rusty_core::storage_policy::validate_tenant_s3(endpoint, region, bucket, prefix, path_style).map_err(|e| map_error(&e, "storage"))?;
    let (access, secret) = read_keys(env)?;
    let pools = pool_stores(env)?;
    if env.sealing.is_none() {
        return Err(CtlError::runtime("set [sealing] public_key in the configuration: the credentials are sealed to it (`rustyctl seal keygen` makes one)"));
    }
    let (dom, pool) = pool_of_domain(env, name).await?;
    let row = pools.add_s3(pool.id, backend, &s3, weight).await.map_err(|e| map_error(&e, "storage"))?;
    seal_backend_keys(env, row.id.0, &access, &secret).await?;
    Ok(Outcome::new(
        format!("added backend {} ({} bucket {}) to the pool of {}; run `storage check {}` before it is used", row.name, s3.endpoint, s3.bucket, dom.name, dom.name),
        json!({ "domain": dom.name, "store": pool.id.0, "backend": row.name, "id": row.id.0, "endpoint": s3.endpoint, "region": s3.region, "bucket": s3.bucket, "weight": row.weight }),
    ))
}

async fn storage_backend_rekey(env: &mut CtlEnv<'_>, name: &str, backend: &str) -> Result<Outcome, CtlError> {
    let (access, secret) = read_keys(env)?;
    let pools = pool_stores(env)?;
    let (dom, pool) = pool_of_domain(env, name).await?;
    let b = backend_of(env, pool.id, backend).await?;
    let settings: Value = serde_json::from_str(&b.config).unwrap_or(Value::Null);
    let text = |n: &str| settings.get(n).and_then(Value::as_str).unwrap_or_default().to_owned();
    let s3 = rusty_core::storage_policy::validate_tenant_s3(&text("endpoint"), &text("region"), &text("bucket"), &text("prefix"), settings.get("path_style").and_then(Value::as_bool).unwrap_or(false))
        .map_err(|e| map_error(&e, "storage"))?;
    seal_backend_keys(env, b.id.0, &access, &secret).await?;
    pools.set_s3(b.id, &s3).await.map_err(|e| map_error(&e, "storage"))?;
    Ok(Outcome::new(
        format!("replaced the keys of backend {} of {}; run `storage check {}` before it is used again", b.name, dom.name, dom.name),
        json!({ "domain": dom.name, "backend": b.name, "rekeyed": true }),
    ))
}

async fn storage_backend_set(env: &mut CtlEnv<'_>, name: &str, backend: &str, state: Option<rusty_store::repo::BackendState>, weight: Option<u32>) -> Result<Outcome, CtlError> {
    let pools = pool_stores(env)?;
    let (dom, pool) = pool_of_domain(env, name).await?;
    let b = backend_of(env, pool.id, backend).await?;
    if let Some(w) = weight {
        pools.set_weight(b.id, w).await.map_err(|e| map_error(&e, "storage"))?;
    }
    if let Some(s) = state {
        pools.set_state(b.id, s).await.map_err(|e| map_error(&e, "storage"))?;
    }
    Ok(Outcome::new(
        format!("backend {} of {} updated (the services pick it up within a minute)", b.name, dom.name),
        json!({ "domain": dom.name, "backend": b.name, "state": state.map(rusty_store::repo::BackendState::name), "weight": weight }),
    ))
}

async fn storage_backend_remove(env: &mut CtlEnv<'_>, name: &str, backend: &str) -> Result<Outcome, CtlError> {
    let pools = pool_stores(env)?;
    let (dom, pool) = pool_of_domain(env, name).await?;
    let b = backend_of(env, pool.id, backend).await?;
    pools.remove(b.id).await.map_err(|e| map_error(&e, "storage"))?;
    Ok(Outcome::new(format!("removed backend {} from the pool of {}", b.name, dom.name), json!({ "domain": dom.name, "backend": b.name, "removed": true })))
}

/// The backends of a pool for `storage show`.
async fn backends_json(env: &CtlEnv<'_>, store: rusty_store::repo::StoreId) -> Result<Value, CtlError> {
    use rusty_core::storage_policy::{ACCESS_KEY_PURPOSE, BACKEND_SECRET_OWNER, SECRET_KEY_PURPOSE};
    let pools = pool_stores(env)?;
    let mut out = Vec::new();
    for b in pools.backends(store).await.map_err(|e| map_error(&e, "storage"))? {
        let meta = match env.sealed {
            Some(w) => w.meta(BACKEND_SECRET_OWNER, b.id.0).await.map_err(|e| map_error(&e, "backend credentials"))?,
            None => Vec::new(),
        };
        let has = |p: &str| meta.iter().any(|m| m.purpose == p);
        let held = pools.count_held(b.id).await.map_err(|e| map_error(&e, "storage"))?;
        out.push(json!({
            "name": b.name, "id": b.id.0, "kind": b.kind, "state": b.state.name(), "weight": b.weight, "settings": serde_json::from_str::<Value>(&b.config).unwrap_or(Value::Null),
            "access_key_set": has(ACCESS_KEY_PURPOSE), "secret_key_set": has(SECRET_KEY_PURPOSE), "check_status": b.check_status, "checked_at": b.checked_at, "objects": held,
        }));
    }
    Ok(Value::Array(out))
}
