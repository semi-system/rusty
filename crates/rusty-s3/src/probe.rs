//! The check of a store: write, read (whole and a range), list and delete a probe object under `<prefix>/.rusty-probe/`, and say which step failed as one of the fixed categories
//! of [`S3Fail`]. A store is usable only after this passed; it also tells the tenant, in terms they can act on, what is wrong (the name does not resolve, the credentials are
//! refused, the bucket is missing, the key may not write).

use bytes::Bytes;

use crate::client::S3Client;
use crate::error::S3Fail;

const PROBE_DIR: &str = ".rusty-probe";

/// Run the check. The probe object is removed again; a failure after it was written still tries to remove it.
pub async fn probe(client: &S3Client, nonce: u64) -> Result<(), S3Fail> {
    let key = format!("{PROBE_DIR}/{nonce:016x}");
    let body: Vec<u8> = (0..64u8).map(|i| i.wrapping_mul(7).wrapping_add(nonce as u8)).collect();
    let result = run(client, &key, &body).await;
    // Always try to leave nothing behind, whatever step failed.
    let cleanup = client.delete(&key).await;
    result.and(cleanup)
}

async fn run(client: &S3Client, key: &str, body: &[u8]) -> Result<(), S3Fail> {
    client.put(key, Bytes::copy_from_slice(body)).await?;
    let whole = client.get(key, None, 1024).await?;
    if whole[..] != body[..] {
        return Err(S3Fail::Protocol);
    }
    let part = client.get(key, Some((8, 15)), 1024).await?;
    if part[..] != body[8..16] {
        return Err(S3Fail::Protocol);
    }
    match client.head(key).await? {
        Some(m) if m.size == body.len() as u64 => {}
        _ => return Err(S3Fail::Protocol),
    }
    let listed = client.list(&format!("{PROBE_DIR}/"), None).await?;
    if !listed.entries.iter().any(|e| e.key == key) {
        return Err(S3Fail::Protocol);
    }
    Ok(())
}
