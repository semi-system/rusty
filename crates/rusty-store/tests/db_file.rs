//! The SQLite database file is created private (0600) before sqlx opens it.

use std::os::unix::fs::PermissionsExt;

use rusty_store::SqliteStore;

fn mode(p: &std::path::Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

// backend-neutral: SQLite-only: database file permissions
#[tokio::test]
async fn new_database_file_is_0600() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta.sqlite");
    let store = SqliteStore::open(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    assert_eq!(mode(&path), 0o600);
    // Journal files SQLite creates next to it inherit the database file's mode.
    sqlx::query("CREATE TABLE t (x INTEGER)")
        .execute(store.pool())
        .await
        .unwrap();
    for e in std::fs::read_dir(dir.path()).unwrap() {
        let p = e.unwrap().path();
        assert_eq!(mode(&p), 0o600, "{p:?}");
    }
}

// backend-neutral: SQLite-only: database file permissions
#[tokio::test]
async fn existing_database_file_mode_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta.sqlite");
    let url = format!("sqlite://{}", path.display());
    drop(SqliteStore::open(&url).await.unwrap());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    drop(SqliteStore::open(&url).await.unwrap());
    assert_eq!(mode(&path), 0o640);
}
