#![forbid(unsafe_code)]

mod blob;
mod dns;
mod sealed;
mod error_map;
mod feed;
mod gc;
mod jobs;
mod mailbox;
mod migrate;
mod pg;
mod pg_queue;
mod store;
pub mod repo;
#[cfg(feature = "mutation-testing")]
pub mod mutation;
#[cfg(feature = "test-fixtures")]
pub mod fixtures;

pub use blob::{
    BlobEntry, BlobStore, FsBlobStore, GcLock, HealReport, BLOB_ROOT_MARKER, GC_LOCK_FILE,
    QUARANTINE_SUFFIX,
};
pub use gc::{
    Gc, GcReport, PurgeReport, CAPABILITY_RETENTION_SECS, DEFAULT_GC_GRACE,
    DELIVERY_RETENTION_SECS, GC_BATCH, MAX_GC_ERRORS, MIN_GC_GRACE, PURGE_BATCH,
};
pub use dns::{
    DnsStore, OwnershipToken, TokenVerifier, DNS_EVENTS_PER_DOMAIN, MAX_DNS_EVENT_PAGE, MAX_RECORDS_PER_DOMAIN,
};
pub use error_map::map_db_error;
pub use sealed::{SealedReader, SealedWriter, MAX_SEALED_BYTES, MIN_SEALED_BYTES};
pub use feed::{ChangeFeed, ChangeSink, PgChangeFeed, CHANNEL_JOBS, CHANNEL_MAILBOX};
pub use jobs::{
    ClaimedJob, JobQueue, DEFAULT_JOB_ATTEMPTS, JOB_RETENTION_SECS, MAX_CLAIM_KINDS,
    MAX_JOB_ATTEMPTS, MAX_JOB_KIND_BYTES, MAX_JOB_PAYLOAD_BYTES, MAX_LAST_ERROR_BYTES,
    MAX_LEASE_SECS, MAX_PAYLOAD_DEPTH, MAX_PENDING_JOBS_GLOBAL_NO_DOMAIN,
    MAX_PENDING_JOBS_PER_DOMAIN, MAX_RETRY_AFTER_SECS, MAX_SETTLED_PER_CLAIM,
};
pub use mailbox::{
    ChangeNotifier, normalize_keyword, validate_folder_name, BatchOutcome, Change, ChangeItem, ChangeOp,
    ChangePage, CopyBatch, DeliverItem, DeliverOutcome, Mailbox, MailboxLimits, MessageRef, MessageSummary,
    MoveBatch, DEFAULT_MAX_MESSAGE_BYTES, MAX_BATCH, MAX_DELIVER_BATCH_BYTES, MAX_FOLDER_DEPTH,
    MAX_FOLDER_NAME_BYTES, MAX_KEYWORDS_PER_MESSAGE, MAX_KEYWORD_BYTES, MAX_PAGE,
    MAX_SUPPLIED_TIME,
};
pub use migrate::{
    embedded_versions, migrate_postgres, schema_status, MigrationReport, SchemaState, SchemaStatus, APP_ROLE,
    MIGRATOR_ROLE, RO_ROLE,
};
pub use pg::{PgPassword, PgLimits, PgSecurity, PgStore};
pub use pg_queue::{MailboxPermit, MailboxQueue, STRIPES};
pub use jobs::BatchSpec;
pub use repo::{FlagUpdate, FolderCounts, FolderInfo, JobFence, MessageDetail, OwnerLimits, PurgeBatch, RawRef};
pub use store::{Repos, Store, StoreConfig};
