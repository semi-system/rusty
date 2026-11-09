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
    AccountAdd { addr: String, password: PasswordSource },
    AccountList { domain: Option<String> },
    AccountSetPassword { addr: String, password: PasswordSource },
    AccountDisable { addr: String },
    AccountEnable { addr: String },
    AliasAdd { alias: String, target: String },
    AliasList { domain: Option<String> },
    AliasRemove { alias: String },
    DkimGenerate { domain: String, selector: String, ed25519: bool },
    Check,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub source: Source,
    pub json: bool,
    /// `--create`: the database may be created if it does not exist (never implicit).
    pub create: bool,
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

pub const USAGE: &str = "usage: rustyctl [--config FILE | --dev [--data-dir DIR]] [--json] [--create] <command>\n\
\n\
  domain add <name> | list | disable <name> | enable <name>\n\
  account add <addr> [--password-stdin | --prompt]\n\
  account list [domain]\n\
  account set-password <addr> (--password-stdin | --prompt)\n\
  account disable <addr> | enable <addr>\n\
  alias add <alias-addr> <account-addr> | list [domain] | remove <alias-addr>\n\
  dkim generate <domain> [--selector S] [--ed25519]   create a DKIM key (0600) and print its DNS record\n\
  check                                      configuration summary and DNS records to publish\n\
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
    let mut ed25519 = false;
    let mut help = false;
    let mut create = false;
    let mut words: Vec<&str> = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json = true,
            "--create" => create = true,
            "--ed25519" => ed25519 = true,
            "--dev" => dev = true,
            "--prompt" => prompt = true,
            "--password-stdin" => stdin = true,
            "--help" | "-h" => help = true,
            "--config" => config = Some(PathBuf::from(it.next().ok_or(err("--config needs a value"))?)),
            "--data-dir" => data_dir = Some(PathBuf::from(it.next().ok_or(err("--data-dir needs a value"))?)),
            "--selector" => selector = Some(it.next().ok_or(err("--selector needs a value"))?.clone()),
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

    let done = |command| Ok(Parsed { source: source.clone(), json, create, command });
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
    let no_selector = || -> Result<(), UsageError> {
        if ed25519 {
            return Err(err("--ed25519 does not apply to this command"));
        }
        if selector.is_some() {
            return Err(err("--selector does not apply to this command"));
        }
        Ok(())
    };
    let plain = || -> Result<(), UsageError> {
        no_password()?;
        no_selector()
    };

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
            done(Command::DkimGenerate {
                domain: (*domain).to_owned(),
                ed25519,
                selector: match selector.as_deref() {
                    None => DEFAULT_SELECTOR.to_owned(),
                    Some(s) if valid_selector(s) => s.to_owned(),
                    Some(_) => return Err(err("--selector must be a DNS label (letters, digits, - and .)")),
                },
            })
        }
        ["check"] => {
            plain()?;
            done(Command::Check)
        }
        [] => Err(err("a command is required")),
        ["domain" | "account" | "alias" | "dkim"] => Err(err("a subcommand is required")),
        ["domain" | "account" | "alias" | "dkim", _, ..] | ["check", ..] => {
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
