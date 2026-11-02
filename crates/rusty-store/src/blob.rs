use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use rusty_core::{BlobHash, Error, Result};
use tokio::io::AsyncWriteExt;

#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn put(&self, bytes: Bytes) -> Result<BlobHash>;
    async fn get(&self, h: &BlobHash) -> Result<Bytes>;
    async fn delete(&self, h: &BlobHash) -> Result<()>;
}

/// Content-addressed blobs stored at `root/ab/cd/<sha256-hex>`.
pub struct FsBlobStore {
    root: PathBuf,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl FsBlobStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Only a validated `BlobHash` (64 lowercase hex chars) can build a path.
    fn path_for(&self, h: &BlobHash) -> PathBuf {
        let s = h.as_str();
        self.root.join(&s[0..2]).join(&s[2..4]).join(s)
    }
}

fn io_err(msg: &str, e: std::io::Error) -> Error {
    Error::storage_with(msg, e)
}

#[async_trait]
impl BlobStore for FsBlobStore {
    async fn put(&self, bytes: Bytes) -> Result<BlobHash> {
        let hash = BlobHash::of(&bytes);
        let path = self.path_for(&hash);
        if tokio::fs::try_exists(&path).await.map_err(|e| io_err("blob exists check", e))? {
            return Ok(hash);
        }
        let dir = path.parent().ok_or_else(|| Error::storage("blob path has no parent"))?;
        tokio::fs::create_dir_all(dir).await.map_err(|e| io_err("create blob dir", e))?;
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{}.{}.{}.tmp", hash.as_str(), std::process::id(), n));

        let write = async {
            let mut f = tokio::fs::File::create(&tmp).await?;
            f.write_all(&bytes).await?;
            f.flush().await?;
            f.sync_all().await?;
            drop(f);
            tokio::fs::rename(&tmp, &path).await
        };
        if let Err(e) = write.await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(io_err("write blob", e));
        }
        Ok(hash)
    }

    async fn get(&self, h: &BlobHash) -> Result<Bytes> {
        match tokio::fs::read(self.path_for(h)).await {
            Ok(v) => Ok(Bytes::from(v)),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                Err(Error::NotFound(format!("blob {}", h.as_str())))
            }
            Err(e) => Err(io_err("read blob", e)),
        }
    }

    async fn delete(&self, h: &BlobHash) -> Result<()> {
        match tokio::fs::remove_file(self.path_for(h)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err("delete blob", e)),
        }
    }
}
