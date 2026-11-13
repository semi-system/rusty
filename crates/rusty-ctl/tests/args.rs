//! The command-line layer of `rustyctl`: bytes in, a typed command out, no I/O.

use rusty_ctl::{parse_args, Command, PasswordSource, Source};

fn p(args: &[&str]) -> Result<rusty_ctl::Parsed, rusty_ctl::UsageError> {
    let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    parse_args(&v)
}

fn cmd(args: &[&str]) -> Command {
    p(args).unwrap().command
}

// backend-neutral: argument parsing only, no database
#[test]
fn domain_commands() {
    assert_eq!(cmd(&["domain", "add", "example.com"]), Command::DomainAdd { name: "example.com".into() });
    assert_eq!(cmd(&["domain", "list"]), Command::DomainList);
    assert_eq!(cmd(&["domain", "disable", "example.com"]), Command::DomainDisable { name: "example.com".into() });
    assert_eq!(cmd(&["domain", "enable", "example.com"]), Command::DomainEnable { name: "example.com".into() });
}

// backend-neutral: argument parsing only, no database
#[test]
fn account_commands_and_password_sources() {
    assert_eq!(
        cmd(&["account", "add", "a@example.com"]),
        Command::AccountAdd { addr: "a@example.com".into(), password: PasswordSource::None }
    );
    assert_eq!(
        cmd(&["account", "add", "a@example.com", "--password-stdin"]),
        Command::AccountAdd { addr: "a@example.com".into(), password: PasswordSource::Stdin }
    );
    assert_eq!(
        cmd(&["account", "add", "--prompt", "a@example.com"]),
        Command::AccountAdd { addr: "a@example.com".into(), password: PasswordSource::Prompt }
    );
    assert_eq!(
        cmd(&["account", "set-password", "a@example.com", "--prompt"]),
        Command::AccountSetPassword { addr: "a@example.com".into(), password: PasswordSource::Prompt }
    );
    assert_eq!(cmd(&["account", "list"]), Command::AccountList { domain: None });
    assert_eq!(cmd(&["account", "list", "example.com"]), Command::AccountList { domain: Some("example.com".into()) });
    assert_eq!(cmd(&["account", "disable", "a@example.com"]), Command::AccountDisable { addr: "a@example.com".into() });
    assert_eq!(cmd(&["account", "enable", "a@example.com"]), Command::AccountEnable { addr: "a@example.com".into() });
}

// backend-neutral: argument parsing only, no database
#[test]
fn set_password_needs_a_source_and_two_sources_conflict() {
    assert!(p(&["account", "set-password", "a@example.com"]).is_err());
    assert!(p(&["account", "add", "a@example.com", "--prompt", "--password-stdin"]).is_err());
}

// backend-neutral: argument parsing only, no database
#[test]
fn a_password_on_the_command_line_is_refused_and_never_echoed() {
    for args in [
        vec!["account", "add", "a@example.com", "--password", "hunter2-secret"],
        vec!["account", "add", "a@example.com", "--password=hunter2-secret"],
        vec!["account", "set-password", "a@example.com", "hunter2-secret"],
        vec!["account", "add", "a@example.com", "-p", "hunter2-secret"],
    ] {
        let e = p(&args).expect_err("a password in argv must be refused");
        let shown = format!("{e} {e:?}");
        assert!(!shown.contains("hunter2-secret"), "echoed: {shown}");
    }
}

// backend-neutral: argument parsing only, no database
#[test]
fn alias_commands() {
    assert_eq!(
        cmd(&["alias", "add", "info@example.com", "a@example.com"]),
        Command::AliasAdd { alias: "info@example.com".into(), target: "a@example.com".into() }
    );
    assert_eq!(cmd(&["alias", "list"]), Command::AliasList { domain: None });
    assert_eq!(cmd(&["alias", "list", "example.com"]), Command::AliasList { domain: Some("example.com".into()) });
    assert_eq!(cmd(&["alias", "remove", "info@example.com"]), Command::AliasRemove { alias: "info@example.com".into() });
}

// backend-neutral: argument parsing only, no database
#[test]
fn dkim_and_check() {
    assert_eq!(
        cmd(&["dkim", "generate", "example.com"]),
        Command::DkimGenerate { domain: "example.com".into(), selector: "rusty".into() }
    );
    assert_eq!(
        cmd(&["dkim", "generate", "example.com", "--selector", "s1"]),
        Command::DkimGenerate { domain: "example.com".into(), selector: "s1".into() }
    );
    assert_eq!(
        cmd(&["dkim", "import", "example.com", "/root/dkim.pem"]),
        Command::DkimImport { domain: "example.com".into(), selector: "rusty".into(), key_file: "/root/dkim.pem".into() }
    );
    assert_eq!(
        cmd(&["dkim", "import", "example.com", "k.pem", "--selector", "s2"]),
        Command::DkimImport { domain: "example.com".into(), selector: "s2".into(), key_file: "k.pem".into() }
    );
    // generate makes ed25519 keys only; the old switch is gone rather than silently ignored.
    assert!(p(&["dkim", "generate", "example.com", "--ed25519"]).is_err());
    assert!(p(&["dkim", "import", "example.com"]).is_err(), "import needs a key file");
    assert!(p(&["dkim", "import", "example.com", "k.pem", "--selector", "-x"]).is_err());
    assert!(p(&["dkim", "import", "example.com", "k.pem", "--prompt"]).is_err());
    assert!(p(&["domain", "list", "--selector", "s"]).is_err(), "--selector only applies to dkim");
    assert_eq!(cmd(&["check"]), Command::Check);
}

// backend-neutral: argument parsing only, no database
#[test]
fn global_options_anywhere() {
    let a = p(&["--json", "domain", "list"]).unwrap();
    let b = p(&["domain", "list", "--json"]).unwrap();
    assert!(a.json && b.json);
    assert_eq!(a.source, Source::Config(None));
    let c = p(&["--config", "/etc/x.toml", "domain", "list"]).unwrap();
    assert_eq!(c.source, Source::Config(Some("/etc/x.toml".into())));
    let d = p(&["--dev", "--data-dir", "/tmp/d", "domain", "list"]).unwrap();
    assert_eq!(d.source, Source::Dev { data_dir: Some("/tmp/d".into()) });
    assert_eq!(p(&["--dev", "check"]).unwrap().source, Source::Dev { data_dir: None });
    assert!(!p(&["domain", "list"]).unwrap().json);
}

// backend-neutral: argument parsing only, no database
#[test]
fn bad_invocations_are_usage_errors_that_do_not_echo_input() {
    for args in [
        vec![],
        vec!["domain"],
        vec!["domain", "add"],
        vec!["domain", "add", "a.test", "extra"],
        vec!["domain", "frobnicate-token-xyz"],
        vec!["frobnicate-token-xyz"],
        vec!["--frobnicate-token-xyz", "domain", "list"],
        vec!["--data-dir", "/x", "domain", "list"],
        vec!["--dev", "--config", "/x", "domain", "list"],
        vec!["--config"],
        vec!["account", "add", "a@b.c", "--unknown-token-xyz"],
        vec!["dkim", "generate", "a.test", "--selector"],
        vec!["domain", "list", "--json", "extra"],
    ] {
        match p(&args) {
            Ok(x) => panic!("accepted {args:?}: {:?}", x.command),
            Err(e) => assert!(!format!("{e}").contains("token-xyz"), "echoed input: {e}"),
        }
    }
}

// backend-neutral: help is text, no database
#[test]
fn help_is_a_command() {
    assert_eq!(cmd(&["help"]), Command::Help);
    assert_eq!(cmd(&["--help"]), Command::Help);
    assert_eq!(cmd(&["domain", "--help"]), Command::Help);
}

proptest::proptest! {
    // backend-neutral: the parser must never panic on arbitrary argument vectors
    #[test]
    fn parser_never_panics(args in proptest::collection::vec(".{0,20}", 0..8)) {
        let _ = parse_args(&args);
    }
}

// backend-neutral: argument parsing only, no database
#[test]
fn dkim_selectors_are_dns_labels() {
    for ok in ["s1", "rusty", "mail-2026", "a.b", "A1"] {
        assert!(p(&["dkim", "generate", "a.test", "--selector", ok]).is_ok(), "{ok}");
    }
    let long = "a".repeat(64);
    for bad in ["", "-x", "x-", "a b", "a/b", "a;b", "..", "é", long.as_str()] {
        assert!(p(&["dkim", "generate", "a.test", "--selector", bad]).is_err(), "{bad:?}");
    }
}

// backend-neutral: argument parsing only, no database
#[test]
fn db_commands() {
    assert_eq!(cmd(&["db", "migrate"]), Command::DbMigrate { require_roles: true });
    assert_eq!(cmd(&["db", "migrate", "--without-roles"]), Command::DbMigrate { require_roles: false });
    assert_eq!(cmd(&["--config", "/etc/rusty.toml", "db", "status"]), Command::DbStatus);
    // Options of other commands do not apply, and the db commands take no arguments.
    for bad in [
        &["db"][..],
        &["db", "migrate", "extra"],
        &["db", "status", "--without-roles"],
        &["db", "migrate", "--prompt"],
        &["db", "migrate", "--selector", "x"],
        &["db", "drop"],
        &["domain", "list", "--without-roles"],
    ] {
        assert!(p(bad).is_err(), "{bad:?}");
    }
}
