#![forbid(unsafe_code)]

mod blob;
mod gc;
mod jobs;
mod mailbox;
mod sqlite;
mod traits;

pub use blob::{
    BlobEntry, BlobStore, FsBlobStore, GcLock, HealReport, BLOB_ROOT_MARKER, GC_LOCK_FILE,
    QUARANTINE_SUFFIX,
};
pub use gc::{
    collect_blobs, collect_blobs_at, purge_deleted, purge_deliveries, purge_deliveries_at,
    purge_deliveries_default, GcReport, DEFAULT_GC_GRACE, DELIVERY_RETENTION_SECS, GC_BATCH,
    MAX_GC_ERRORS, MIN_GC_GRACE, PURGE_BATCH,
};
pub use jobs::{
    ClaimedJob, JobQueue, DEFAULT_JOB_ATTEMPTS, JOB_RETENTION_SECS, MAX_CLAIM_KINDS,
    MAX_JOB_ATTEMPTS, MAX_JOB_KIND_BYTES, MAX_JOB_PAYLOAD_BYTES, MAX_PAYLOAD_DEPTH,
    MAX_LAST_ERROR_BYTES, MAX_LEASE_SECS, MAX_PENDING_JOBS_GLOBAL_NO_DOMAIN,
    MAX_PENDING_JOBS_PER_DOMAIN, MAX_RETRY_AFTER_SECS, MAX_SETTLED_PER_CLAIM,
};
pub use mailbox::{
    normalize_keyword, validate_folder_name, Change, ChangeItem, ChangeOp, ChangePage,
    DeliverOutcome, Mailbox, MailboxLimits, MessageRef, MessageSummary, DEFAULT_MAX_MESSAGE_BYTES,
    MAX_FOLDER_DEPTH, MAX_FOLDER_NAME_BYTES, MAX_KEYWORDS_PER_MESSAGE, MAX_KEYWORD_BYTES, MAX_PAGE,
};
pub use sqlite::{map_db_error, SqliteStore};
pub use traits::{MetadataStore, SqliteTx, Tx};
