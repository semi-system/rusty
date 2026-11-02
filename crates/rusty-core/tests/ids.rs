use rusty_core::{BlobHash, Flags, FolderRole, ListId};

#[test]
fn blob_hash_of_known_input() {
    assert_eq!(
        BlobHash::of(b"abc").as_str(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn blob_hash_parse_rejects_uppercase_and_wrong_length() {
    let good = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert!(BlobHash::parse(good).is_ok());
    assert!(BlobHash::parse(&good.to_uppercase()).is_err());
    assert!(BlobHash::parse(&good[..63]).is_err());
    assert!(BlobHash::parse(&format!("{good}0")).is_err());
    assert!(BlobHash::parse(&"g".repeat(64)).is_err());
}

#[test]
fn flags_from_bits_rejects_over_31() {
    assert!(Flags::from_bits(31).is_some());
    assert!(Flags::from_bits(32).is_none());
    assert_eq!(Flags::from_bits(Flags::SEEN.bits() | Flags::DELETED.bits()).unwrap().bits(), 17);
}

#[test]
fn folder_role_roundtrip() {
    for r in [FolderRole::Inbox, FolderRole::Sent, FolderRole::Drafts, FolderRole::Trash,
              FolderRole::Spam, FolderRole::Archive, FolderRole::Custom] {
        assert_eq!(r.as_str().parse::<FolderRole>().unwrap(), r);
    }
    assert!("bogus".parse::<FolderRole>().is_err());
}

#[test]
fn list_id_is_a_newtype() {
    assert_eq!(ListId(7), ListId(7));
    assert_ne!(ListId(7), ListId(8));
    assert_eq!(ListId(7).0, 7);
}

#[test]
fn flags_bitor_contains_and_empty() {
    let f = Flags::SEEN | Flags::FLAGGED;
    assert_eq!(f.bits(), 3);
    assert!(f.contains(Flags::SEEN));
    assert!(f.contains(Flags::SEEN | Flags::FLAGGED));
    assert!(!f.contains(Flags::DRAFT));
    assert!(!f.contains(Flags::SEEN | Flags::DRAFT));
    assert!(f.contains(Flags::empty()));
    assert_eq!(Flags::empty(), Flags::default());
    let mut g = Flags::empty();
    g |= Flags::DELETED;
    assert_eq!(g, Flags::DELETED);
    let all = Flags::SEEN | Flags::FLAGGED | Flags::ANSWERED | Flags::DRAFT | Flags::DELETED;
    assert_eq!(all.bits(), 31);
}
