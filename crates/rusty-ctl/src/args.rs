//! The command-line layer: arguments in, a typed [`Command`] out. No I/O, no state, no
//! database. Error texts are fixed reasons that never echo the offending argument (an argument
//! can be an address, a path or, by mistake, a password).

use std::fmt;
use std::path::PathBuf;

/// Where `rustyctl` finds the database: the production configuration file or the dev profile
/// (same meaning as for `rusty d`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// `--config FILE` (`None`: the default path, decided by the composition root).
    Config(Option<PathBuf>),
    /// `--dev [--data-dir DIR]`.
    Dev { data_dir: Option<PathBuf> },
}

/// How an account password reaches the program. Never argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordSource {
    /// No password: the account cannot authenticate until one is set.
    None,
    /// The first line of standard input.
    Stdin,
    /// Asked on the terminal without echo, twice.
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    DomainAdd { name: String },
    DomainList,
    DomainDisable { name: String },
    DomainEnable { name: String },
    /// `seal keygen`: make the unsealing key pair of the outbound service; the private half goes to `file` (0600, never replaced).
    SealKeygen { file: PathBuf },
    /// `seal show`: the public half and id of the key in `file`.
    SealShow { file: PathBuf },
    /// `domain dns`: the DNS records to publish for the domain (configuration and stored keys); nothing is queried.
    DomainDns { name: String },
    /// `domain check`: those records against what DNS publishes, recorded as drift state. Needs the live resolver.
    DomainCheck { name: String },
    AccountAdd { addr: String, password: PasswordSource },
    AccountList { domain: Option<String> },
    AccountSetPassword { addr: String, password: PasswordSource },
    AccountLock { addr: String },
    AccountDisable { addr: String },
    AccountEnable { addr: String },
    AliasAdd { alias: String, target: String },
    AliasList { domain: Option<String> },
    AliasRemove { alias: String },
    DkimGenerate { domain: String, selector: String },
    /// `dkim generate --sealed`: make the key and seal its private half in the database; no file, no key directory.
    DkimGenerateSealed { domain: String, selector: String },
    DkimImport { domain: String, selector: String, key_file: PathBuf },
    /// `domain relay set`: the relay of one domain (host, port, TLS, login; the password is read from stdin or the terminal and sealed).
    DomainRelaySet { name: String, host: String, port: u16, tls: rusty_core::config::RelayTls, user: Option<String>, spf_include: Option<String>, password: PasswordSource },
    /// `domain relay show`: the relay of the domain, without its password.
    DomainRelayShow { name: String },
    /// `storage set`: the S3 store of one domain's bodies (settings here, the access key and secret key read from standard input, two lines, and sealed).
    StorageSet { name: String, endpoint: String, region: String, bucket: String, prefix: String, path_style: bool },
    /// `storage show`: the store of the domain, without its credentials.
    StorageShow { name: String },
    /// `storage pool`: make the domain's store a pool of backends with this policy (or change the policy of its pool while it holds no body).
    StoragePool { name: String, policy: rusty_store::PoolPolicy },
    /// `storage backend add`: add an S3 backend (settings here, the keys from standard input) to the domain's pool.
    StorageBackendAdd { name: String, backend: String, endpoint: String, region: String, bucket: String, prefix: String, path_style: bool, weight: u32 },
    /// `storage backend rekey`: replace the keys of a backend (same settings; it has to pass a check again).
    StorageBackendRekey { name: String, backend: String },
    /// `storage backend set`: the state (`active`, `readonly`, `draining`, `disabled`) and/or the weight of a backend.
    StorageBackendSet { name: String, backend: String, state: Option<rusty_store::repo::BackendState>, weight: Option<u32> },
    /// `storage backend remove`: remove a backend that holds no object (drain it first).
    StorageBackendRemove { name: String, backend: String },
    /// `storage check`: ask the mail service to test the store (a `storage.check` job); the result shows in `storage show`.
    StorageCheck { name: String },
    /// `domain relay check`: ask the outbound service to test the relay (a `relay.check` job); the result shows in `domain relay show`.
    DomainRelayCheck { name: String },
    /// `mailbox deleted`: the messages the person deleted that can still be recovered.
    MailboxDeleted { addr: String },
    /// `mailbox recover`: bring a deleted message back as a new message in a folder (default: the Inbox).
    MailboxRecover { addr: String, id: i64, folder: Option<String> },
    /// `mailbox export`: write every folder of the mailbox into a new directory, as an mbox file each or (with `maildir`) as a Maildir each.
    MailboxExport { addr: String, dir: PathBuf, maildir: bool },
    /// `mailbox import`: add the messages of an mbox file, or of a Maildir when the path is a directory, to a folder (default: the Inbox; created when it does not exist).
    MailboxImport { addr: String, file: PathBuf, folder: Option<String> },
    /// `blobs init`: make the blob directory of the configuration (mode 0700, with its marker) once, before the first start, or, with `[blob_s3]`, the bucket; it needs no database. Safe to repeat.
    BlobsInit,
    /// `blobs verify`: check that every blob the database references exists in the blob store (after a restore); exit 1 when one is missing.
    BlobsVerify,
    /// `admin grant`: let an account use the admin web (every domain, or the one named).
    AdminGrant { addr: String, domain: Option<String> },
    /// `admin revoke`: the account stays, it is no longer an administrator.
    AdminRevoke { addr: String },
    /// `admin list`: the administrators and what they administer.
    AdminList,
    /// `domain relay remove`: the domain goes back to the platform route; its sealed password is removed.
    DomainRelayRemove { name: String },
    Check,
    /// `db migrate`: apply pending Postgres migrations with the migrator role (never the daemon).
    DbMigrate { require_roles: bool },
    /// `relay check`: log in to the configured relay and stop (no mail is sent). Run by the composition root.
    RelayCheck,
    /// `db status`: the schema state of the configured database.
    DbStatus,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub source: Source,
    pub json: bool,
    pub command: Command,
}

/// A command-line mistake (exit code 2). The text is fixed and never echoes input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(&'static str);

impl UsageError {
    pub fn message(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for UsageError {}

pub const DEFAULT_SELECTOR: &str = "rusty";

pub const USAGE: &str = "usage: rustyctl [--config FILE | --dev [--data-dir DIR]] [--json] <command>\n\
\n\
  domain add <name> | list | disable <name> | enable <name>\n\
  domain dns <name>                          the DNS records to publish (addresses, SPF, DKIM, DMARC, SRV)\n\
  domain check <name>                        check them against live DNS and record the result (exit 1 if a required one is wrong)\n\
  account add <addr> [--password-stdin | --prompt]\n\
  account list [domain]\n\
  account set-password <addr> (--password-stdin | --prompt)\n\
  account lock <addr> | disable <addr> | enable <addr>\n\
  mailbox deleted <addr>                     the messages the person deleted that can still be recovered\n\
  mailbox recover <addr> <id> [folder]       bring one back as a new message (default folder: inbox)\n\
  mailbox export <addr> <dir>                write every folder as an mbox file into the new directory <dir>\n\
  mailbox import <addr> <file> [folder]      add the messages of an mbox file to a folder (default: inbox; made if missing)\n\
  alias add <alias-addr> <account-addr> | list [domain] | remove <alias-addr>\n\
  dkim generate <domain> [--selector S] [--sealed]\n\
                                             create an ed25519 DKIM key and print its DNS record: the private key is a\n\
                                             file (0600), or with --sealed sealed in the database (no file)\n\
  dkim import <domain> <key.pem> [--selector S]\n\
                                             keep an RSA (2048-4096 bit) or ed25519 private key (0600)\n\
                                             and print its DNS record; make an RSA key with\n\
                                             openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048\n\
  domain relay set <domain> --host H [--port P] [--tls starttls|implicit|none] [--user U] [--spf-include NAME] [--password-stdin | --prompt]
                                             the relay this domain's mail leaves through (a login needs the password, which is sealed)
  domain relay show <domain> | remove <domain> | check <domain>
  storage set <domain> --endpoint https://HOST[:PORT] --region R --bucket B [--prefix P] [--path-style] --credentials-stdin
                                             give the domain its own S3 store for message bodies (before it holds any message); the access key and the
                                             secret key are read from standard input, one per line, and sealed
  storage show <domain>                      the domain's store, without its credentials
  storage pool <domain> [--copies N] [--sync-copies N] [--placement ordered|weighted] [--read located|ordered|round_robin|fastest]
                                             spread the domain's bodies over several S3 backends: JBOD (copies 1), mirror (copies N), failover (copies 2, sync-copies 1,
                                             ordered); add backends with storage backend add
  storage backend add <domain> <name> --endpoint https://HOST[:PORT] --region R --bucket B [--prefix P] [--path-style] [--weight W] --credentials-stdin
  storage backend rekey <domain> <name> --credentials-stdin | set <domain> <name> [--state active|readonly|draining|disabled] [--weight W] | remove <domain> <name>
  storage check <domain>                     ask the mail service to test the store (write, read, list, delete a probe object); the result shows in storage show.
                                             A store is used only after a check passed
  admin grant <addr> [domain]                let an account use the admin web (all domains, or one); also the recovery path
  admin revoke <addr> | list
  blobs init                                 make the blob directory (0700, with its marker) once, before the first start (with [blob_s3]: the bucket) (no database needed)
  blobs verify                               check that every blob the database references exists in the blob store (use after a restore; exit 1 if one is missing)
  relay check                                log in to the configured relay (resolve, TLS, AUTH) without sending mail
  seal keygen <file>                         make the key that opens sealed secrets (private half to <file>, 0600), print the public half
  seal show <file>                           print the public half and the id of the key in <file>
  check                                     configuration summary and DNS records to publish\n\
\n\
Passwords are read from standard input or the terminal, never from the command line.";

fn err(msg: &'static str) -> UsageError {
    UsageError(msg)
}

/// Parse the arguments that follow `rustyctl` (argv[0] excluded).
pub fn parse_args(args: &[String]) -> Result<Parsed, UsageError> {
    let mut json = false;
    let mut dev = false;
    let mut config: Option<PathBuf> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut prompt = false;
    let mut stdin = false;
    let mut selector: Option<String> = None;
    let mut help = false;
    let mut without_roles = false;
    let mut sealed = false;
    let mut relay_host: Option<String> = None;
    let mut relay_port: Option<String> = None;
    let mut relay_tls: Option<String> = None;
    let mut relay_user: Option<String> = None;
    let mut relay_spf: Option<String> = None;
    let mut s3_endpoint: Option<String> = None;
    let mut s3_region: Option<String> = None;
    let mut s3_bucket: Option<String> = None;
    let mut s3_prefix: Option<String> = None;
    let mut s3_path_style = false;
    let mut s3_credentials_stdin = false;
    let mut pool_copies: Option<String> = None;
    let mut pool_sync: Option<String> = None;
    let mut pool_placement: Option<String> = None;
    let mut pool_read: Option<String> = None;
    let mut weight: Option<String> = None;
    let mut state: Option<String> = None;
    let mut words: Vec<&str> = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json = true,
            "--without-roles" => without_roles = true,
            "--sealed" => sealed = true,
            "--dev" => dev = true,
            "--prompt" => prompt = true,
            "--password-stdin" => stdin = true,
            "--help" | "-h" => help = true,
            "--config" => config = Some(PathBuf::from(it.next().ok_or(err("--config needs a value"))?)),
            "--data-dir" => data_dir = Some(PathBuf::from(it.next().ok_or(err("--data-dir needs a value"))?)),
            "--selector" => selector = Some(it.next().ok_or(err("--selector needs a value"))?.clone()),
            "--host" => relay_host = Some(it.next().ok_or(err("--host needs a value"))?.clone()),
            "--port" => relay_port = Some(it.next().ok_or(err("--port needs a value"))?.clone()),
            "--tls" => relay_tls = Some(it.next().ok_or(err("--tls needs a value"))?.clone()),
            "--user" => relay_user = Some(it.next().ok_or(err("--user needs a value"))?.clone()),
            "--spf-include" => relay_spf = Some(it.next().ok_or(err("--spf-include needs a value"))?.clone()),
            "--endpoint" => s3_endpoint = Some(it.next().ok_or(err("--endpoint needs a value"))?.clone()),
            "--region" => s3_region = Some(it.next().ok_or(err("--region needs a value"))?.clone()),
            "--bucket" => s3_bucket = Some(it.next().ok_or(err("--bucket needs a value"))?.clone()),
            "--prefix" => s3_prefix = Some(it.next().ok_or(err("--prefix needs a value"))?.clone()),
            "--path-style" => s3_path_style = true,
            "--credentials-stdin" => s3_credentials_stdin = true,
            "--copies" => pool_copies = Some(it.next().ok_or(err("--copies needs a value"))?.clone()),
            "--sync-copies" => pool_sync = Some(it.next().ok_or(err("--sync-copies needs a value"))?.clone()),
            "--placement" => pool_placement = Some(it.next().ok_or(err("--placement needs a value"))?.clone()),
            "--read" => pool_read = Some(it.next().ok_or(err("--read needs a value"))?.clone()),
            "--weight" => weight = Some(it.next().ok_or(err("--weight needs a value"))?.clone()),
            "--state" => state = Some(it.next().ok_or(err("--state needs a value"))?.clone()),
            s if s == "-p" || s == "--password" || s.starts_with("--password=") => {
                return Err(err(
                    "a password is never accepted on the command line; use --password-stdin or --prompt",
                ));
            }
            s if s.starts_with('-') => return Err(err("unknown option")),
            s => words.push(s),
        }
    }

    if dev && config.is_some() {
        return Err(err("--dev and --config exclude each other"));
    }
    if !dev && data_dir.is_some() {
        return Err(err("--data-dir needs --dev"));
    }
    if prompt && stdin {
        return Err(err("--prompt and --password-stdin exclude each other"));
    }
    let source = if dev { Source::Dev { data_dir } } else { Source::Config(config) };
    let password = match (prompt, stdin) {
        (true, _) => PasswordSource::Prompt,
        (_, true) => PasswordSource::Stdin,
        _ => PasswordSource::None,
    };

    let done = |command| Ok(Parsed { source: source.clone(), json, command });
    if help || words.first() == Some(&"help") {
        return done(Command::Help);
    }
    // Options that only some commands take must not be silently ignored by the others.
    let no_password = || -> Result<(), UsageError> {
        if password != PasswordSource::None {
            return Err(err("--prompt and --password-stdin do not apply to this command"));
        }
        Ok(())
    };
    let no_without_roles = || -> Result<(), UsageError> {
        if without_roles {
            return Err(err("--without-roles applies to db migrate only"));
        }
        Ok(())
    };
    let no_selector = || -> Result<(), UsageError> {
        if selector.is_some() {
            return Err(err("--selector does not apply to this command"));
        }
        Ok(())
    };
    let plain = || -> Result<(), UsageError> {
        no_password()?;
        no_selector()?;
        no_without_roles()
    };
    let dkim_selector = || -> Result<String, UsageError> {
        match selector.as_deref() {
            None => Ok(DEFAULT_SELECTOR.to_owned()),
            Some(s) if valid_selector(s) => Ok(s.to_owned()),
            Some(_) => Err(err("--selector must be a DNS label (letters, digits, - and .)")),
        }
    };

    let s3_opts = s3_endpoint.is_some() || s3_region.is_some() || s3_bucket.is_some() || s3_prefix.is_some() || s3_path_style || s3_credentials_stdin;
    let s3_target = matches!(words.as_slice(), ["storage", "set", _] | ["storage", "backend", "add", _, _]);
    let rekey = matches!(words.as_slice(), ["storage", "backend", "rekey", _, _]);
    if s3_opts && !s3_target && !(rekey && s3_credentials_stdin && s3_endpoint.is_none() && s3_region.is_none() && s3_bucket.is_none() && s3_prefix.is_none() && !s3_path_style) {
        return Err(err("--endpoint, --region, --bucket, --prefix, --path-style and --credentials-stdin apply to storage set, storage backend add and (--credentials-stdin) storage backend rekey only"));
    }
    if (pool_copies.is_some() || pool_sync.is_some() || pool_placement.is_some() || pool_read.is_some()) && !matches!(words.as_slice(), ["storage", "pool", _]) {
        return Err(err("--copies, --sync-copies, --placement and --read apply to storage pool only"));
    }
    if weight.is_some() && !matches!(words.as_slice(), ["storage", "backend", "add" | "set", _, _]) {
        return Err(err("--weight applies to storage backend add and set only"));
    }
    if state.is_some() && !matches!(words.as_slice(), ["storage", "backend", "set", _, _]) {
        return Err(err("--state applies to storage backend set only"));
    }
    let relay_opts = relay_host.is_some() || relay_port.is_some() || relay_tls.is_some() || relay_user.is_some() || relay_spf.is_some();
    if relay_opts && !matches!(words.as_slice(), ["domain", "relay", "set", _]) {
        return Err(err("--host, --port, --tls, --user and --spf-include apply to domain relay set only"));
    }
    if sealed && !matches!(words.as_slice(), ["dkim", "generate", _]) {
        return Err(err("--sealed applies to dkim generate only"));
    }

    match words.as_slice() {
        ["domain", "add", name] => {
            plain()?;
            done(Command::DomainAdd { name: (*name).to_owned() })
        }
        ["domain", "list"] => {
            plain()?;
            done(Command::DomainList)
        }
        ["domain", "disable", name] => {
            plain()?;
            done(Command::DomainDisable { name: (*name).to_owned() })
        }
        ["domain", "enable", name] => {
            plain()?;
            done(Command::DomainEnable { name: (*name).to_owned() })
        }
        ["domain", "dns", name] => {
            plain()?;
            done(Command::DomainDns { name: (*name).to_owned() })
        }
        ["domain", "check", name] => {
            plain()?;
            done(Command::DomainCheck { name: (*name).to_owned() })
        }
        ["account", "add", addr] => {
            no_selector()?;
            done(Command::AccountAdd { addr: (*addr).to_owned(), password })
        }
        ["account", "set-password", addr] => {
            no_selector()?;
            if password == PasswordSource::None {
                return Err(err("set-password needs --password-stdin or --prompt"));
            }
            done(Command::AccountSetPassword { addr: (*addr).to_owned(), password })
        }
        ["account", "list"] => {
            plain()?;
            done(Command::AccountList { domain: None })
        }
        ["account", "list", d] => {
            plain()?;
            done(Command::AccountList { domain: Some((*d).to_owned()) })
        }
        ["account", "lock", addr] => {
            plain()?;
            done(Command::AccountLock { addr: (*addr).to_owned() })
        }
        ["account", "disable", addr] => {
            plain()?;
            done(Command::AccountDisable { addr: (*addr).to_owned() })
        }
        ["account", "enable", addr] => {
            plain()?;
            done(Command::AccountEnable { addr: (*addr).to_owned() })
        }
        ["alias", "add", alias, target] => {
            plain()?;
            done(Command::AliasAdd { alias: (*alias).to_owned(), target: (*target).to_owned() })
        }
        ["alias", "list"] => {
            plain()?;
            done(Command::AliasList { domain: None })
        }
        ["alias", "list", d] => {
            plain()?;
            done(Command::AliasList { domain: Some((*d).to_owned()) })
        }
        ["alias", "remove", alias] => {
            plain()?;
            done(Command::AliasRemove { alias: (*alias).to_owned() })
        }
        ["dkim", "generate", domain] => {
            no_password()?;
            no_without_roles()?;
            if sealed {
                done(Command::DkimGenerateSealed { domain: (*domain).to_owned(), selector: dkim_selector()? })
            } else {
                done(Command::DkimGenerate { domain: (*domain).to_owned(), selector: dkim_selector()? })
            }
        }
        ["dkim", "import", domain, key_file] => {
            no_password()?;
            no_without_roles()?;
            done(Command::DkimImport { domain: (*domain).to_owned(), selector: dkim_selector()?, key_file: PathBuf::from(key_file) })
        }
        ["domain", "relay", "set", name] => {
            no_selector()?;
            no_without_roles()?;
            let host = relay_host.clone().ok_or(err("domain relay set needs --host"))?;
            let tls = match relay_tls.as_deref() {
                None | Some("starttls") => rusty_core::config::RelayTls::Starttls,
                Some("implicit") => rusty_core::config::RelayTls::Implicit,
                Some("none") => rusty_core::config::RelayTls::None,
                Some(_) => return Err(err("--tls must be starttls, implicit or none")),
            };
            let port = match relay_port.as_deref() {
                None => if tls == rusty_core::config::RelayTls::Implicit { 465 } else { 587 },
                Some(p) => p.parse::<u16>().map_err(|_| err("--port must be a number"))?,
            };
            done(Command::DomainRelaySet { name: (*name).to_owned(), host, port, tls, user: relay_user.clone(), spf_include: relay_spf.clone(), password })
        }
        ["storage", "set", name] => {
            no_selector()?;
            no_without_roles()?;
            no_password()?;
            if !s3_credentials_stdin {
                return Err(err("storage set reads the access key and the secret key from standard input (two lines): add --credentials-stdin"));
            }
            let endpoint = s3_endpoint.clone().ok_or(err("storage set needs --endpoint"))?;
            let region = s3_region.clone().ok_or(err("storage set needs --region"))?;
            let bucket = s3_bucket.clone().ok_or(err("storage set needs --bucket"))?;
            done(Command::StorageSet { name: (*name).to_owned(), endpoint, region, bucket, prefix: s3_prefix.clone().unwrap_or_default(), path_style: s3_path_style })
        }
        ["storage", "pool", name] => {
            plain()?;
            let mut m = serde_json::Map::new();
            let num = |v: &str, what: &'static str| v.parse::<u32>().map_err(|_| err(what));
            if let Some(v) = &pool_copies {
                m.insert("copies".into(), num(v, "--copies must be a number")?.into());
            }
            if let Some(v) = &pool_sync {
                m.insert("sync_copies".into(), num(v, "--sync-copies must be a number")?.into());
            }
            if let Some(v) = &pool_placement {
                m.insert("placement".into(), v.clone().into());
            }
            if let Some(v) = &pool_read {
                m.insert("read".into(), v.clone().into());
            }
            let policy = rusty_store::PoolPolicy::parse(&serde_json::Value::Object(m).to_string())
                .map_err(|_| err("the pool settings are not valid: 1 <= sync-copies <= copies <= 8, placement ordered or weighted, read located, ordered, round_robin or fastest"))?;
            done(Command::StoragePool { name: (*name).to_owned(), policy })
        }
        ["storage", "backend", "add", name, backend] => {
            no_selector()?;
            no_without_roles()?;
            no_password()?;
            if !s3_credentials_stdin {
                return Err(err("storage backend add reads the access key and the secret key from standard input (two lines): add --credentials-stdin"));
            }
            let endpoint = s3_endpoint.clone().ok_or(err("storage backend add needs --endpoint"))?;
            let region = s3_region.clone().ok_or(err("storage backend add needs --region"))?;
            let bucket = s3_bucket.clone().ok_or(err("storage backend add needs --bucket"))?;
            let weight = match &weight {
                None => 100,
                Some(w) => w.parse::<u32>().map_err(|_| err("--weight must be a number from 1 to 1000"))?,
            };
            done(Command::StorageBackendAdd { name: (*name).to_owned(), backend: (*backend).to_owned(), endpoint, region, bucket, prefix: s3_prefix.clone().unwrap_or_default(), path_style: s3_path_style, weight })
        }
        ["storage", "backend", "rekey", name, backend] => {
            no_selector()?;
            no_without_roles()?;
            no_password()?;
            if !s3_credentials_stdin {
                return Err(err("storage backend rekey reads the access key and the secret key from standard input (two lines): add --credentials-stdin"));
            }
            done(Command::StorageBackendRekey { name: (*name).to_owned(), backend: (*backend).to_owned() })
        }
        ["storage", "backend", "set", name, backend] => {
            plain()?;
            if state.is_none() && weight.is_none() {
                return Err(err("storage backend set needs --state and/or --weight"));
            }
            let st = match &state {
                None => None,
                Some(v) => Some(rusty_store::repo::BackendState::parse(v).ok_or(err("--state must be active, readonly, draining or disabled"))?),
            };
            let w = match &weight {
                None => None,
                Some(w) => Some(w.parse::<u32>().map_err(|_| err("--weight must be a number from 1 to 1000"))?),
            };
            done(Command::StorageBackendSet { name: (*name).to_owned(), backend: (*backend).to_owned(), state: st, weight: w })
        }
        ["storage", "backend", "remove", name, backend] => {
            plain()?;
            done(Command::StorageBackendRemove { name: (*name).to_owned(), backend: (*backend).to_owned() })
        }
        ["storage", "show", name] => {
            plain()?;
            done(Command::StorageShow { name: (*name).to_owned() })
        }
        ["storage", "check", name] => {
            plain()?;
            done(Command::StorageCheck { name: (*name).to_owned() })
        }
        ["domain", "relay", "show", name] => {
            plain()?;
            done(Command::DomainRelayShow { name: (*name).to_owned() })
        }
        ["domain", "relay", "check", name] => {
            plain()?;
            done(Command::DomainRelayCheck { name: (*name).to_owned() })
        }
        ["domain", "relay", "remove", name] => {
            plain()?;
            done(Command::DomainRelayRemove { name: (*name).to_owned() })
        }
        ["admin", "grant", addr] => {
            plain()?;
            done(Command::AdminGrant { addr: (*addr).to_owned(), domain: None })
        }
        ["admin", "grant", addr, domain] => {
            plain()?;
            done(Command::AdminGrant { addr: (*addr).to_owned(), domain: Some((*domain).to_owned()) })
        }
        ["admin", "revoke", addr] => {
            plain()?;
            done(Command::AdminRevoke { addr: (*addr).to_owned() })
        }
        ["mailbox", "deleted", addr] => {
            plain()?;
            done(Command::MailboxDeleted { addr: (*addr).to_owned() })
        }
        ["mailbox", "recover", addr, id] | ["mailbox", "recover", addr, id, _] => {
            plain()?;
            let id: i64 = id.parse().map_err(|_| err("the message id must be a number"))?;
            let folder = match words.as_slice() {
                [_, _, _, _, f] => Some((*f).to_owned()),
                _ => None,
            };
            done(Command::MailboxRecover { addr: (*addr).to_owned(), id, folder })
        }
        ["mailbox", "export", addr, dir] | ["mailbox", "export", addr, dir, "maildir"] => {
            plain()?;
            done(Command::MailboxExport { addr: (*addr).to_owned(), dir: PathBuf::from(dir), maildir: words.len() == 5 })
        }
        ["mailbox", "import", addr, file] | ["mailbox", "import", addr, file, _] => {
            plain()?;
            let folder = match words.as_slice() {
                [_, _, _, _, f] => Some((*f).to_owned()),
                _ => None,
            };
            done(Command::MailboxImport { addr: (*addr).to_owned(), file: PathBuf::from(file), folder })
        }
        ["blobs", "init"] => {
            plain()?;
            done(Command::BlobsInit)
        }
        ["blobs", "verify"] => {
            plain()?;
            done(Command::BlobsVerify)
        }
        ["admin", "list"] => {
            plain()?;
            done(Command::AdminList)
        }
        ["db", "migrate"] => {
            no_password()?;
            no_selector()?;
            done(Command::DbMigrate { require_roles: !without_roles })
        }
        ["db", "status"] => {
            no_password()?;
            no_selector()?;
            no_without_roles()?;
            done(Command::DbStatus)
        }
        ["seal", "keygen", file] => {
            plain()?;
            done(Command::SealKeygen { file: PathBuf::from(file) })
        }
        ["seal", "show", file] => {
            plain()?;
            done(Command::SealShow { file: PathBuf::from(file) })
        }
        ["relay", "check"] => {
            plain()?;
            done(Command::RelayCheck)
        }
        ["check"] => {
            plain()?;
            done(Command::Check)
        }
        [] => Err(err("a command is required")),
        ["domain" | "account" | "alias" | "dkim" | "db" | "relay" | "seal" | "admin" | "blobs" | "mailbox"] => Err(err("a subcommand is required")),
        ["domain" | "account" | "alias" | "dkim" | "db" | "relay" | "seal" | "admin" | "blobs" | "mailbox", _, ..] | ["check", ..] => {
            Err(err("unknown subcommand or wrong number of arguments"))
        }
        _ => Err(err("unknown command")),
    }
}

/// A DKIM selector is published as a DNS name part: 1..=63 of letters, digits, `-` and `.`,
/// not starting or ending with `-` or `.`.
fn valid_selector(s: &str) -> bool {
    let edge_ok = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    (1..=63).contains(&s.len())
        && edge_ok(s.chars().next())
        && edge_ok(s.chars().last())
        && !s.contains("..")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}
