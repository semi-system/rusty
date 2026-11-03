#![forbid(unsafe_code)]

mod blob;
mod gc;
mod mailbox;
mod sqlite;
mod traits;

pub use blob::{
    BlobEntry, BlobStore, FsBlobStore, GcLock, HealReport, BLOB_ROOT_MARKER, GC_LOCK_FILE,
    QUARANTINE_SUFFIX,
};
pub use gc::{
    collect_blobs, collect_blobs_at, purge_deleted, GcReport, DEFAULT_GC_GRACE, GC_BATCH,
    MAX_GC_ERRORS, MIN_GC_GRACE, PURGE_BATCH,
};
pub use mailbox::{
    normalize_keyword, validate_folder_name, Change, ChangeItem, ChangeOp, ChangePage, Mailbox,
    MailboxLimits, MessageRef, MessageSummary, DEFAULT_MAX_MESSAGE_BYTES, MAX_FOLDER_DEPTH,
    MAX_FOLDER_NAME_BYTES, MAX_KEYWORDS_PER_MESSAGE, MAX_KEYWORD_BYTES, MAX_PAGE,
};
pub use sqlite::{map_db_error, SqliteStore};
pub use traits::{MetadataStore, SqliteTx, Tx};
