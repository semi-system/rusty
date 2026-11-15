//! `S3BlobStore`: message bodies as objects of one bucket, named by their content (`<prefix>/ab/cd/<sha256 hex>`, two shard levels as on disk).
//!
//! * A put is idempotent and refreshes the object's modification time, which is what starts garbage collection's grace window again.
//! * A get is verified against the name before any byte is used: a swapped or damaged object is an error, never somebody's mail.
//! * Deletion is "head, compare the modification time with the cutoff, delete". Unlike the filesystem store there is no quarantine rename to make it atomic against a concurrent put
//!   of the same bytes; the window is one round trip, and it needs byte-identical content to arrive in that instant after the previous copy was unreferenced for the whole grace
//!   window (docs/design-s3-per-tenant.md, section 9).
//! * The GC lock is in-process only: a second collector on the same bucket is not excluded yet (the per-store lease in the database replaces it).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use rusty_core::{BlobHash, Error, Result};
use rusty_store::{BlobEntry, BlobStore, GcLock, HealReport};
use tokio::sync::Mutex;

use crate::client::S3Client;

/// The largest body read back: the configuration's own ceiling for a message (1 GiB) with some room.
const MAX_BODY: usize = 1_100_000_000;

pub struct S3BlobStore {
    client: S3Client,
    gc: Arc<Mutex<()>>,
}

impl S3BlobStore {
    pub fn new(client: S3Client) -> S3BlobStore {
        S3BlobStore { client, gc: Arc::new(Mutex::new(())) }
    }

    pub fn client(&self) -> &S3Client {
        &self.client
    }

    fn key(h: &BlobHash) -> String {
        let s = h.as_str();
        format!("{}/{}/{s}", &s[0..2], &s[2..4])
    }
}

/// A listed key that is one of ours: `ab/cd/<64 hex digits>` with the shard digits matching.
fn entry_hash(relative_key: &str) -> Option<BlobHash> {
    let mut parts = relative_key.split('/');
    let (a, b, hash) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let h = BlobHash::parse(hash).ok()?;
    (&hash[0..2] == a && &hash[2..4] == b).then_some(h)
}

fn secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[async_trait]
impl BlobStore for S3BlobStore {
    async fn put(&self, bytes: Bytes) -> Result<BlobHash> {
        let hash = BlobHash::of(&bytes);
        self.client.put(&Self::key(&hash), bytes).await?;
        Ok(hash)
    }

    async fn get(&self, h: &BlobHash) -> Result<Bytes> {
        let body = self.client.get(&Self::key(h), None, MAX_BODY).await?;
        if BlobHash::of(&body) != *h {
            return Err(Error::storage("a stored blob does not match its name"));
        }
        Ok(body)
    }

    async fn list_shard(&self, shard: u8) -> Result<Vec<BlobEntry>> {
        let prefix = format!("{shard:02x}/");
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = self.client.list(&prefix, token.as_deref()).await?;
            for e in page.entries {
                // Anything that is not a well-formed blob of this shard is skipped, as on the filesystem.
                if let Some(hash) = entry_hash(&e.key) {
                    if hash.as_str().starts_with(&prefix[..2]) {
                        out.push(BlobEntry { hash, size: e.size, modified_unix: e.modified_unix });
                    }
                }
            }
            match page.next {
                Some(t) => token = Some(t),
                None => return Ok(out),
            }
        }
    }

    async fn delete_if_older(&self, h: &BlobHash, cutoff: SystemTime) -> Result<bool> {
        let key = Self::key(h);
        match self.client.head(&key).await? {
            None => Ok(false),
            Some(meta) if meta.modified_unix >= secs(cutoff) => Ok(false),
            Some(_) => {
                self.client.delete(&key).await?;
                Ok(true)
            }
        }
    }

    async fn sweep_temp_before(&self, _cutoff: SystemTime) -> Result<u64> {
        // An object appears whole or not at all: there are no temporary files to sweep.
        Ok(0)
    }

    async fn heal_quarantine(&self, _cutoff: SystemTime) -> Result<HealReport> {
        Ok(HealReport::default())
    }

    async fn try_lock_gc(&self) -> Result<GcLock> {
        match self.gc.clone().try_lock_owned() {
            Ok(guard) => Ok(GcLock::held(guard)),
            Err(_) => Err(Error::Conflict("gc already running".into())),
        }
    }

    fn fs_now(&self) -> SystemTime {
        self.client.server_now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_well_formed_keys_of_ours_are_blobs() {
        let h = BlobHash::of(b"x");
        let s = h.as_str();
        let good = format!("{}/{}/{s}", &s[0..2], &s[2..4]);
        assert_eq!(entry_hash(&good), Some(h.clone()));
        assert_eq!(S3BlobStore::key(&h), good);
        for bad in [
            s.to_owned(),
            format!("{}/{s}", &s[0..2]),
            format!("{}/{}/{s}/extra", &s[0..2], &s[2..4]),
            format!("zz/{}/{s}", &s[2..4]),
            format!("{}/zz/{s}", &s[0..2]),
            format!("{}/{}/{}", &s[0..2], &s[2..4], s.to_uppercase()),
            format!("{}/{}/{}", &s[0..2], &s[2..4], &s[..63]),
            ".rusty-probe/x".to_owned(),
            String::new(),
        ] {
            assert_eq!(entry_hash(&bad), None, "{bad}");
        }
    }
}
