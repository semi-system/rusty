//! Error hygiene: `Display` of errors never echoes caller input (spec §8b).
//!
//! Hostile inputs (control characters, 1 MB strings, secret-looking values) go through every
//! public entry point that can reject them; the resulting error text must contain none of
//! the input. `source()` chains are not checked: they may carry raw backend text and must
//! never be serialized to clients (see `rusty_core::Error`).

use std::str::FromStr;
use std::sync::Arc;

use bytes::Bytes;
use rusty_core::{BlobHash, Error, FolderId, FolderRole, MessageId};
use rusty_directory::{normalize_address, parse_address, Directory};
use rusty_store::{BlobStore, FsBlobStore, Mailbox, SqliteStore};

const SECRET: &str = "hunter2-S3CR3T-T0KEN";

fn hostile() -> Vec<String> {
    vec![
        SECRET.to_string(),
        format!("{SECRET}\u{0}\u{1b}[31m\r\n"),
        format!("\u{202e}{SECRET}\u{200b}"),
        format!("{SECRET}{}", "x".repeat(1024 * 1024)),
        format!("{SECRET}@{SECRET}.example"),
        format!("{SECRET}@exa mple.com"),
        format!("bad\u{0}local@{SECRET}..com"),
    ]
}

fn assert_no_echo(what: &str, e: &Error) {
    let shown = e.to_string();
    let lower = shown.to_lowercase();
    assert!(
        !lower.contains(&SECRET.to_lowercase()),
        "{what}: error echoes input: {shown:.200}"
    );
    assert!(shown.len() < 200, "{what}: error text too long ({} bytes)", shown.len());
}

/// The error (if any) must not echo the input. Some hostile strings are valid for some entry
/// points (e.g. a plain secret-looking local part); `Ok` is fine there. Returns true on `Err`.
fn check<T>(what: &str, r: Result<T, Error>) -> bool {
    match r {
        Err(e) => {
            assert_no_echo(what, &e);
            true
        }
        Ok(_) => false,
    }
}

/// Like `check`, but the input must be rejected.
fn check_err<T>(what: &str, r: Result<T, Error>) {
    assert!(check(what, r), "{what}: hostile input accepted");
}

#[test]
fn core_parsers_do_not_echo() {
    for s in hostile() {
        check_err("BlobHash::parse", BlobHash::parse(&s));
        check_err("FolderRole::from_str", FolderRole::from_str(&s));
    }
}

#[test]
fn address_parsers_do_not_echo() {
    let mut rejected = 0;
    for s in hostile() {
        rejected += usize::from(check("parse_address", parse_address(&s)));
        rejected += usize::from(check(
            "normalize_address(local)",
            normalize_address(&s, "example.com"),
        ));
        rejected += usize::from(check(
            "normalize_address(domain)",
            normalize_address("alice", &s),
        ));
    }
    assert!(rejected >= 12, "only {rejected} rejections");
}

#[tokio::test]
async fn store_open_does_not_echo_url_or_path() {
    for url in [
        format!("bogus://{SECRET}"),
        format!("sqlite:///nonexistent-{SECRET}/dir/meta.sqlite?mode=ro"),
        format!("sqlite://{SECRET}?unknown_option={SECRET}"),
    ] {
        check_err("SqliteStore::open", SqliteStore::open(&url).await);
    }
}

#[tokio::test]
async fn directory_errors_do_not_echo() {
    let d = Directory::new(SqliteStore::open("sqlite::memory:").await.unwrap());
    let dom = d.create_domain("example.com").await.unwrap();
    d.create_account("alice", dom, None, None).await.unwrap();
    let mut rejected = 0;
    for s in hostile() {
        rejected += usize::from(check("create_domain", d.create_domain(&s).await));
        rejected += usize::from(check(
            "create_account",
            d.create_account(&s, dom, None, None).await,
        ));
        rejected += usize::from(check("resolve", d.resolve(&s).await));
    }
    assert!(rejected >= 12, "only {rejected} rejections");
    // Well-formed but unknown addresses: NotFound must not name the address.
    let local = format!("{}-2", SECRET.to_lowercase());
    check_err("resolve unknown", d.resolve(&format!("{local}@example.com")).await);
    check_err(
        "resolve unknown domain",
        d.resolve(&format!("alice@{local}.example")).await,
    );
    // Duplicate account: Conflict must not name the address.
    d.create_account(&local, dom, None, None).await.unwrap();
    check_err(
        "duplicate account",
        d.create_account(&local, dom, None, None).await,
    );
}

#[tokio::test]
async fn mailbox_errors_do_not_echo() {
    let d = Directory::new(SqliteStore::open("sqlite::memory:").await.unwrap());
    let dom = d.create_domain("example.com").await.unwrap();
    let acc = d.create_account("alice", dom, None, None).await.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("blobs");
    FsBlobStore::init(&root).await.unwrap();
    let blobs: Arc<dyn BlobStore> = Arc::new(FsBlobStore::open(&root).await.unwrap());
    let mb = Mailbox::open(d.store().clone(), blobs, acc).await.unwrap();
    for s in hostile() {
        check("create_folder", mb.create_folder(None, &s).await);
        check_err("add_keyword", mb.add_keyword(MessageId(1), &s).await);
    }
    // A header section over the 256 KiB cap, full of the secret.
    let mut raw = format!("Subject: {SECRET}\r\nX-Pad: ");
    while raw.len() < 300 * 1024 {
        raw.push_str(SECRET);
    }
    raw.push_str("\r\n\r\nbody");
    check_err(
        "append oversized header",
        mb.append(FolderId(1), Bytes::from(raw), Default::default())
            .await,
    );
}
