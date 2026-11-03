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

newtype_id!(
    AccountId,
    MailboxId,
    FolderId,
    MessageId,
    DomainId,
    PlanId,
    ListId,
    /// Thread of a message, scoped to its mailbox. A message with no known parent starts a
    /// thread whose id equals its own `MessageId` value.
    ThreadId,
);

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
            Err(Error::Invalid("invalid blob hash".into()))
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

    pub const fn empty() -> Flags {
        Flags(0)
    }

    /// True if every flag set in `other` is also set in `self`.
    pub fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }
}

// The union of two valid flag sets is always <= 31, so the invariant holds.
impl std::ops::BitOr for Flags {
    type Output = Flags;

    fn bitor(self, rhs: Flags) -> Flags {
        Flags(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Flags {
    fn bitor_assign(&mut self, rhs: Flags) {
        self.0 |= rhs.0;
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
            _ => return Err(Error::Invalid("unknown folder role".into())),
        })
    }
}

/// Identifier of one delivery attempt target: SHA-256 over the length-prefixed fields
/// (content hash, recipient, session). Retries of the same delivery derive the same id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeliveryId(pub [u8; 32]);

impl DeliveryId {
    /// Each field is prefixed with its length (u64 big endian), so field boundaries cannot
    /// be shifted: ("ab", "c") and ("a", "bc") derive different ids.
    pub fn derive(content_hash: &BlobHash, recipient: &str, session: &str) -> DeliveryId {
        let mut h = Sha256::new();
        h.update(b"rusty.delivery.v1");
        for field in [content_hash.as_str(), recipient, session] {
            h.update((field.len() as u64).to_be_bytes());
            h.update(field.as_bytes());
        }
        DeliveryId(h.finalize().into())
    }

    /// Lowercase hex, 64 chars.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parse exactly 64 lowercase hex chars.
    pub fn from_hex(s: &str) -> Result<DeliveryId> {
        let bad = || Error::Invalid("invalid delivery id".into());
        if s.len() != 64 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(bad());
        }
        let v = hex::decode(s).map_err(|_| bad())?;
        let arr: [u8; 32] = v.try_into().map_err(|_| bad())?;
        Ok(DeliveryId(arr))
    }
}
