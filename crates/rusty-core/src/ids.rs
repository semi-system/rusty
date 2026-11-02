use std::str::FromStr;

use sha2::{Digest, Sha256};

use crate::{Error, Result};

macro_rules! newtype_id {
    ($($(#[$m:meta])* $name:ident),+ $(,)?) => {
        $(
            $(#[$m])*
            #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
            pub struct $name(pub i64);
        )+
    };
}

newtype_id!(AccountId, MailboxId, FolderId, MessageId, DomainId, PlanId, ListId);

/// Lowercase hex SHA-256 digest (exactly 64 chars).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobHash(String);

impl BlobHash {
    pub fn of(data: &[u8]) -> BlobHash {
        BlobHash(hex::encode(Sha256::digest(data)))
    }

    pub fn parse(s: &str) -> Result<BlobHash> {
        let ok = s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if ok {
            Ok(BlobHash(s.to_owned()))
        } else {
            Err(Error::Invalid(format!("invalid blob hash: {s:?}")))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Flags(u8);

impl Flags {
    pub const SEEN: Flags = Flags(1);
    pub const FLAGGED: Flags = Flags(2);
    pub const ANSWERED: Flags = Flags(4);
    pub const DRAFT: Flags = Flags(8);
    pub const DELETED: Flags = Flags(16);

    pub fn bits(self) -> u8 {
        self.0
    }

    pub fn from_bits(bits: u8) -> Option<Flags> {
        (bits <= 31).then_some(Flags(bits))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FolderRole {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Spam,
    Archive,
    Custom,
}

impl FolderRole {
    pub fn as_str(self) -> &'static str {
        match self {
            FolderRole::Inbox => "inbox",
            FolderRole::Sent => "sent",
            FolderRole::Drafts => "drafts",
            FolderRole::Trash => "trash",
            FolderRole::Spam => "spam",
            FolderRole::Archive => "archive",
            FolderRole::Custom => "custom",
        }
    }
}

impl FromStr for FolderRole {
    type Err = Error;

    fn from_str(s: &str) -> Result<FolderRole> {
        Ok(match s {
            "inbox" => FolderRole::Inbox,
            "sent" => FolderRole::Sent,
            "drafts" => FolderRole::Drafts,
            "trash" => FolderRole::Trash,
            "spam" => FolderRole::Spam,
            "archive" => FolderRole::Archive,
            "custom" => FolderRole::Custom,
            _ => return Err(Error::Invalid(format!("unknown folder role: {s:?}"))),
        })
    }
}
