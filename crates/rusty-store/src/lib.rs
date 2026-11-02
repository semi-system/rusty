#![forbid(unsafe_code)]

mod blob;
mod mailbox;
mod sqlite;
mod traits;

pub use blob::{BlobStore, FsBlobStore};
pub use mailbox::{
    normalize_keyword, validate_folder_name, Change, ChangeKind, ChangeOp, Mailbox, MailboxLimits,
    MessageRef, MessageSummary, DEFAULT_MAX_MESSAGE_BYTES, MAX_FOLDER_DEPTH,
    MAX_FOLDER_NAME_BYTES, MAX_KEYWORDS_PER_MESSAGE, MAX_KEYWORD_BYTES,
};
pub use sqlite::{map_db_error, SqliteStore};
pub use traits::{MetadataStore, SqliteTx, Tx};
