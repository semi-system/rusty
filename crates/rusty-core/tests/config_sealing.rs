//! `[sealing]` (the public key secrets are sealed to: held by whatever seals) and `[unsealing]` (the private key files only
//! the outbound service holds). The private half never appears in a configuration file: it is a path to a file read through
//! the secret source (mode 0600).

use rusty_core::config::StartupConfig;
use rusty_core::Error;

const BASE: &str = "blob_dir = \"/a/blobs\"
store = { url = \"postgres://rusty_app@db.internal:5432/rusty\", password_file = \"/run/secrets/pg\" }
";

const PUB: &str = "rusty-seal-v1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn parse(extra: &str) -> Result<StartupConfig, Error> {
    StartupConfig::from_toml_str(&format!("{BASE}{extra}"))
}

#[test]
fn both_sections_are_absent_by_default() {
    let c = parse("").unwrap();
    assert!(c.sealing.is_none() && c.unsealing.is_none());
}

#[test]
fn the_sealing_key_is_the_public_text_form() {
    let c = parse(&format!("[sealing]\npublic_key = \"{PUB}\"\n")).unwrap();
    assert_eq!(c.sealing.unwrap().public_key, PUB);
    for bad in [
        "",
        "rusty-seal-v1:",
        "rusty-seal-v1:00",
        "rusty-unseal-v1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "rusty-seal-v1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg",
        "rusty-seal-v1:0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ] {
        let r = parse(&format!("[sealing]\npublic_key = \"{bad}\"\n"));
        assert!(matches!(r, Err(Error::Invalid(_))), "{bad:?}");
    }
}

#[test]
fn a_private_key_cannot_be_put_in_the_configuration_inline() {
    // The secret key text form is refused in `[sealing]`, and `[unsealing]` has no inline key at all.
    let secret = "rusty-unseal-v1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    assert!(parse(&format!("[sealing]\npublic_key = \"{secret}\"\n")).is_err());
    assert!(parse(&format!("[unsealing]\nkey_files = [\"/etc/rusty/k\"]\nkey = \"{secret}\"\n")).is_err());
}

#[test]
fn the_unsealing_key_files_are_one_to_four_distinct_absolute_paths() {
    let c = parse("[unsealing]\nkey_files = [\"/etc/rusty/new.key\", \"/etc/rusty/old.key\"]\n").unwrap();
    let u = c.unsealing.unwrap();
    assert_eq!(u.key_files.len(), 2);
    assert_eq!(u.key_files[0], std::path::Path::new("/etc/rusty/new.key"), "the first is the current key");
    for bad in [
        "[]",
        "[\"relative.key\"]",
        "[\"/a\", \"/a\"]",
        "[\"/a\", \"/b\", \"/c\", \"/d\", \"/e\"]",
        "[\"\"]",
    ] {
        let r = parse(&format!("[unsealing]\nkey_files = {bad}\n"));
        assert!(matches!(r, Err(Error::Invalid(_))), "{bad}");
    }
}

#[test]
fn unknown_keys_in_either_section_are_refused() {
    assert!(parse(&format!("[sealing]\npublic_key = \"{PUB}\"\nextra = 1\n")).is_err());
    assert!(parse("[unsealing]\nkey_files = [\"/a\"]\nextra = 1\n").is_err());
}
