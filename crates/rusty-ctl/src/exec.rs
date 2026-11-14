//! The command layer: a typed [`Command`] runs against the `Directory` facade and produces an
//! [`Outcome`] (plain text plus a JSON value). No sockets, no terminal, no database handle:
//! passwords come through the [`PasswordReader`] seam and the directory is injected, so the same
//! code runs over any backend and under test.

use std::fmt;

use rusty_core::dns::{records_for_domain, zone_line, DnsInputs};
use rusty_core::Error;
use rusty_directory::{
    parse_address, AccountInfo, AccountStatus, Directory, DomainInfo, DomainStatus,
};
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
}

/// What a command produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub text: String,
    pub json: Value,
}

impl Outcome {
    fn new(text: impl Into<String>, json: Value) -> Outcome {
        Outcome { text: text.into(), json }
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

        Command::AccountDisable { addr } | Command::AccountEnable { addr } => {
            let status = if matches!(command, Command::AccountDisable { .. }) {
                AccountStatus::Disabled
            } else {
                AccountStatus::Active
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

        Command::DkimImport { domain, selector, key_file } => dkim_import(env, domain, selector, key_file).await,

        Command::Check => check(env).await,
        Command::DbMigrate { .. } | Command::DbStatus | Command::RelayCheck => {
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

/// The text and JSON for a key that was written: where it is and the TXT record to publish. Never the private key.
fn dkim_outcome(dom: &DomainInfo, key: &rusty_smtp::dkim::GeneratedKey, note: &str) -> Outcome {
    use rusty_smtp::dkim::DkimAlgo;
    let name = format!("{}._domainkey.{}", key.selector, dom.name);
    let algorithm = match key.algo {
        DkimAlgo::Rsa => "rsa",
        DkimAlgo::Ed25519 => "ed25519",
    };
    let mut text = format!("key written to {} (mode 0600)\nPublish this TXT record:\n{name}. IN TXT \"{}\"", key.key_file.display(), key.txt_value());
    if !note.is_empty() {
        text.push('\n');
        text.push_str(note);
    }
    Outcome::new(
        text,
        json!({ "domain": dom.name, "algorithm": algorithm, "selector": key.selector, "record_name": name, "record_value": key.txt_value(), "key_file": key.key_file.display().to_string() }),
    )
}

/// `dkim generate`: an ed25519 key for an existing domain, written 0600 into the configured key directory, and
/// the DNS record to publish. Never overwrites, never prints the private key. RSA keys are imported instead
/// (see `rusty_smtp::dkim::generate_key` for why Rusty does not make them).
async fn dkim_generate(env: &mut CtlEnv<'_>, domain: &str, selector: &str) -> Result<Outcome, CtlError> {
    let (dom, dir) = dkim_target(env, domain).await?;
    let key = rusty_smtp::dkim::generate_key(dir, &dom.name, selector).map_err(|e| map_error(&e, "dkim key"))?;
    let rsa_name = format!("{}.{selector}.pem", dom.name);
    let note = if dir.join(&rsa_name).exists() {
        String::new()
    } else {
        format!(
            "Note: some large receivers verify only RSA signatures. Add an RSA key as well:\n  openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out dkim-rsa.pem\n  rustyctl dkim import {} dkim-rsa.pem --selector {selector}   (then delete dkim-rsa.pem)",
            dom.name
        )
    };
    Ok(dkim_outcome(&dom, &key, &note))
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
    Ok(dkim_outcome(&dom, &key, "The source key file is no longer needed by Rusty; delete it or keep it offline."))
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
        let Some(host) = env.hostname else { continue };
        let inputs = DnsInputs { domain: &dom.name, mail_host: host, outbound_ips: &[], dkim: None, dmarc_rua: None };
        let records = records_for_domain(&inputs).map_err(|e| map_error(&e, "dns records"))?;
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

/// Text for the terminal: the plain text, or the JSON value when `json` is set.
pub fn render(outcome: &Outcome, json: bool) -> String {
    if json {
        outcome.json.to_string()
    } else {
        outcome.text.clone()
    }
}
