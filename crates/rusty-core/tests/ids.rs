use rusty_core::{BlobHash, Flags, FolderRole};

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
