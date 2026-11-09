//! Our own command model: what the session layer understands. Library-independent: nothing in
//! here (or in any module except `codec`) names a third-party type, so the command grammar library
//! can be replaced by rewriting `codec.rs` alone. Mailbox names are already UTF-8 (the adapter
//! decodes modified UTF-7), dates are day numbers since 1970-01-01.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqNum {
    Num(u32),
    /// `*`: the largest number in use.
    Star,
}

/// `lo:hi` (or a single number as `lo == hi`), in input order, either direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeqRange {
    pub lo: SeqNum,
    pub hi: SeqNum,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqSet(pub Vec<SeqRange>);

/// A part number path (`1.2.3`), each entry at least 1.
pub type PartPath = Vec<u32>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Section {
    Part(PartPath),
    Header(Option<PartPath>),
    HeaderFields(Option<PartPath>, Vec<Vec<u8>>),
    HeaderFieldsNot(Option<PartPath>, Vec<Vec<u8>>),
    Text(Option<PartPath>),
    Mime(PartPath),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchItem {
    Flags,
    Uid,
    InternalDate,
    Rfc822Size,
    Envelope,
    Body,
    BodyStructure,
    Rfc822,
    Rfc822Header,
    Rfc822Text,
    BodyExt { section: Option<Section>, partial: Option<(u32, u32)>, peek: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusItem {
    Messages,
    Recent,
    UidNext,
    UidValidity,
    Unseen,
    /// DELETED and other items that are not offered.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagArg {
    Seen,
    Answered,
    Flagged,
    Deleted,
    Draft,
    /// Raw keyword text (validated by the store facade).
    Keyword(String),
    /// `\Recent` and unknown system flags: accepted and ignored.
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    Replace,
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchKey {
    And(Vec<SearchKey>),
    Seq(SeqSet),
    Uid(SeqSet),
    All,
    Answered,
    Unanswered,
    Deleted,
    Undeleted,
    Draft,
    Undraft,
    Flagged,
    Unflagged,
    Seen,
    Unseen,
    New,
    Old,
    Recent,
    Keyword(String),
    Unkeyword(String),
    Larger(u32),
    Smaller(u32),
    Before(i64),
    On(i64),
    Since(i64),
    SentBefore(i64),
    SentOn(i64),
    SentSince(i64),
    Bcc(Vec<u8>),
    Body(Vec<u8>),
    Cc(Vec<u8>),
    From(Vec<u8>),
    Subject(Vec<u8>),
    Text(Vec<u8>),
    To(Vec<u8>),
    Header(Vec<u8>, Vec<u8>),
    Not(Box<SearchKey>),
    Or(Box<SearchKey>, Box<SearchKey>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Capability,
    Noop,
    Logout,
    StartTls,
    Login { user: String, password: String },
    /// `initial` is the decoded SASL-IR payload.
    Authenticate { mechanism: String, initial: Option<Vec<u8>> },
    Select { name: String },
    Examine { name: String },
    Create { name: String },
    Delete { name: String },
    Rename { from: String, to: String },
    Subscribe { name: String },
    Unsubscribe { name: String },
    List { reference: String, pattern: String },
    Lsub { reference: String, pattern: String },
    Status { name: String, items: Vec<StatusItem> },
    Append { name: String, flags: Vec<FlagArg>, data: Vec<u8> },
    Check,
    Close,
    Unselect,
    Expunge,
    Search { charset: Option<String>, key: SearchKey, uid: bool },
    Fetch { set: SeqSet, items: Vec<FetchItem>, uid: bool },
    Store { set: SeqSet, kind: StoreKind, silent: bool, flags: Vec<FlagArg>, uid: bool },
    Copy { set: SeqSet, name: String, uid: bool },
    Move { set: SeqSet, name: String, uid: bool },
    Idle,
    Enable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub tag: String,
    pub body: Body,
}

/// Why a framed command did not become a [`Command`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Not valid IMAP (or not a command we understand).
    Syntax,
    /// Valid syntax but a mailbox name that is not valid modified UTF-7.
    BadName,
}
