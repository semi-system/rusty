#![forbid(unsafe_code)]

mod blob;
mod gc;
mod jobs;
mod mailbox;
mod store;
pub mod repo;
#[cfg(feature = "postgres")]
mod pg;
mod sqlite;

pub use blob::{
    BlobEntry, BlobStore, FsBlobStore, GcLock, HealReport, BLOB_ROOT_MARKER, GC_LOCK_FILE,
    QUARANTINE_SUFFIX,
};
pub use gc::{
    collect_blobs, collect_blobs_at, collect_blobs_with_db_clock, purge_capabilities, purge_capabilities_at,
    purge_capabilities_default, purge_deleted, purge_deliveries, purge_deliveries_at,
    purge_deliveries_default, GcReport, CAPABILITY_RETENTION_SECS, DEFAULT_GC_GRACE,
    DELIVERY_RETENTION_SECS, GC_BATCH, MAX_GC_ERRORS, MIN_GC_GRACE, PURGE_BATCH,
};
pub use jobs::{
    ClaimedJob, JobQueue, DEFAULT_JOB_ATTEMPTS, JOB_RETENTION_SECS, MAX_CLAIM_KINDS,
    MAX_JOB_ATTEMPTS, MAX_JOB_KIND_BYTES, MAX_JOB_PAYLOAD_BYTES, MAX_LAST_ERROR_BYTES,
    MAX_LEASE_SECS, MAX_PAYLOAD_DEPTH, MAX_PENDING_JOBS_GLOBAL_NO_DOMAIN,
    MAX_PENDING_JOBS_PER_DOMAIN, MAX_RETRY_AFTER_SECS, MAX_SETTLED_PER_CLAIM,
};
pub use mailbox::{
    normalize_keyword, validate_folder_name, BatchOutcome, Change, ChangeItem, ChangeOp,
    ChangePage, DeliverItem, DeliverOutcome, Mailbox, MailboxLimits, MessageRef, MessageSummary,
    MoveBatch, DEFAULT_MAX_MESSAGE_BYTES, MAX_BATCH, MAX_DELIVER_BATCH_BYTES, MAX_FOLDER_DEPTH,
    MAX_FOLDER_NAME_BYTES, MAX_KEYWORDS_PER_MESSAGE, MAX_KEYWORD_BYTES, MAX_PAGE,
    MAX_SUPPLIED_TIME,
};
#[cfg(feature = "postgres")]
pub use pg::{PgPoolConfig, PgStore};
pub use repo::{FolderInfo, JobFence, MessageDetail, RawRef};
pub use store::{Repos, Store, StoreConfig};
pub use sqlite::{map_db_error, SqliteStore, SqliteTx, DEFAULT_BUSY_TIMEOUT};

