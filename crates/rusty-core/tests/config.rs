use rusty_core::config::{LogLevel, StartupConfig};
use rusty_core::Error;
use std::path::Path;

const BASE: &str = "database_path = \"/a/db.sqlite\"\nblob_dir = \"/a/blobs\"\n";

fn with(extra: &str) -> rusty_core::Result<StartupConfig> {
    StartupConfig::from_toml_str(&format!("{BASE}{extra}\n"))
}

fn invalid<T: std::fmt::Debug>(r: rusty_core::Result<T>) {
    assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
}

#[test]
fn defaults() {
    let c = StartupConfig::from_toml_str(BASE).unwrap();
    assert_eq!(c.database_path, Path::new("/a/db.sqlite"));
    assert_eq!(c.blob_dir, Path::new("/a/blobs"));
    assert_eq!(c.log_level, LogLevel::Info);
    assert_eq!(c.max_message_bytes, 52_428_800);
    assert_eq!(c.gc_grace_seconds, 86_400);
}

#[test]
fn log_levels_parse() {
    assert_eq!(with("log_level = \"trace\"").unwrap().log_level, LogLevel::Trace);
    assert_eq!(with("log_level = \"error\"").unwrap().log_level, LogLevel::Error);
    invalid(with("log_level = \"loud\""));
}

#[test]
fn unknown_key_rejected() {
    invalid(with("bogus = 1"));
}

#[test]
fn missing_required_rejected() {
    invalid(StartupConfig::from_toml_str("blob_dir = \"/b\""));
}

#[test]
fn relative_paths_rejected() {
    invalid(StartupConfig::from_toml_str("database_path = \"db.sqlite\"\nblob_dir = \"/b\""));
    invalid(StartupConfig::from_toml_str("database_path = \"/d\"\nblob_dir = \"blobs\""));
}

#[test]
fn max_message_bytes_bounds() {
    assert!(with("max_message_bytes = 1024").is_ok());
    assert!(with("max_message_bytes = 1073741824").is_ok());
    invalid(with("max_message_bytes = 1023"));
    invalid(with("max_message_bytes = 1073741825"));
    invalid(with("max_message_bytes = -1"));
}

#[test]
fn gc_grace_bounds() {
    assert!(with("gc_grace_seconds = 3600").is_ok());
    invalid(with("gc_grace_seconds = 3599"));
}

#[test]
fn invalid_toml_is_invalid_and_does_not_echo() {
    let r = StartupConfig::from_toml_str("database_path = \"SECRET-VALUE\nzzz");
    match r {
        Err(Error::Invalid(m)) => assert!(!m.contains("SECRET-VALUE"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn load_from_file_and_limits() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("c.toml");
    std::fs::write(&p, BASE).unwrap();
    assert!(StartupConfig::load(&p).is_ok());

    let big = d.path().join("big.toml");
    std::fs::write(&big, format!("{BASE}#{}\n", "x".repeat(70 * 1024))).unwrap();
    invalid(StartupConfig::load(&big));

    let exact = d.path().join("exact.toml");
    let mut s = String::from(BASE);
    s.push(char::from(35u8));
    s.push_str(&"x".repeat(64 * 1024 - s.len()));
    assert_eq!(s.len(), 64 * 1024);
    std::fs::write(&exact, s).unwrap();
    assert!(StartupConfig::load(&exact).is_ok());

    assert!(matches!(
        StartupConfig::load(&d.path().join("nope.toml")),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn example_file_parses() {
    StartupConfig::from_toml_str(include_str!("../../../rusty.example.toml")).unwrap();
}
