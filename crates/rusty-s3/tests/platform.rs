//! The platform store in a bucket (`[blob_s3]`): built from the configuration, keys from files read through the secret source, against RustFS.
//! Needs the S3 server of `scripts/s3-up.sh`; missing: skipped, unless `RUSTY_REQUIRE_S3=1`.

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use rusty_core::config::BlobS3Config;
use rusty_s3::{platform_client, platform_store};
use rusty_store::BlobStore;
use rusty_svc::FileSecretSource;

fn env(n: &str) -> Option<String> {
    std::env::var(n).ok().filter(|v| !v.is_empty())
}

fn unique(tag: &str) -> String {
    let n = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
    format!("rt-{tag}-{n:x}").to_lowercase().chars().take(60).collect()
}

fn write_secret(dir: &std::path::Path, name: &str, text: &str, mode: u32) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    p
}

#[tokio::test]
async fn the_platform_store_works_over_a_plain_http_private_endpoint_with_keys_from_files() {
    let (Some(endpoint), Some(key), Some(secret)) = (env("RUSTY_TEST_S3_ENDPOINT"), env("RUSTY_TEST_S3_KEY"), env("RUSTY_TEST_S3_SECRET")) else {
        assert!(std::env::var("RUSTY_REQUIRE_S3").as_deref() != Ok("1"), "RUSTY_REQUIRE_S3=1 but RUSTY_TEST_S3_* are not set");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    // A trailing newline is the file's, not the key's.
    let access = write_secret(dir.path(), "access", &format!("{key}\n"), 0o600);
    let secret_file = write_secret(dir.path(), "secret", &format!("{secret}\n"), 0o600);
    let bucket = unique("platform");
    let cfg = BlobS3Config {
        endpoint: endpoint.clone(),
        region: "us-east-1".into(),
        bucket: bucket.clone(),
        prefix: "platform".into(),
        path_style: true,
        access_key_file: access.clone(),
        secret_key_file: secret_file.clone(),
        max_concurrent: 4,
        request_timeout_secs: 20,
    };
    let secrets = FileSecretSource::new();
    platform_client(&cfg, &secrets).unwrap().create_bucket().await.expect("make the bucket");
    let store = platform_store(&cfg, &secrets).unwrap();
    let body = Bytes::from(vec![5u8; 200_000]);
    let h = store.put(body.clone()).await.unwrap();
    assert_eq!(store.get(&h).await.unwrap(), body);
    assert_eq!(store.list().await.unwrap().len(), 1);
    assert!(store.delete_if_older(&h, SystemTime::now() + Duration::from_secs(3600)).await.unwrap());

    // Keys that are wrong are refused by the server, not by us; a key file that other users can read is refused by the secret source.
    let bad = write_secret(dir.path(), "bad", "not-the-secret\n", 0o600);
    let wrong = platform_store(&BlobS3Config { secret_key_file: bad, ..cfg.clone() }, &secrets).unwrap();
    assert!(wrong.put(body.clone()).await.is_err(), "the server refuses the wrong secret key");
    let open = write_secret(dir.path(), "open", &format!("{secret}\n"), 0o644);
    assert!(platform_store(&BlobS3Config { secret_key_file: open, ..cfg.clone() }, &secrets).is_err(), "a key file readable by others is not served");
    assert!(platform_store(&BlobS3Config { access_key_file: dir.path().join("missing"), ..cfg.clone() }, &secrets).is_err());
    // A bucket name the S3 rules do not allow stops the start.
    assert!(platform_store(&BlobS3Config { bucket: "UPPER".into(), ..cfg.clone() }, &secrets).is_err());
    // An https endpoint gets TLS verification; this server does not speak it, so a call fails (it never falls back to plain text).
    let tls = platform_store(&BlobS3Config { endpoint: endpoint.replacen("http://", "https://", 1), ..cfg }, &secrets).unwrap();
    assert!(tls.put(body).await.is_err());
}
