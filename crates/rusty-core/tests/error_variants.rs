//! `Error::Busy` is its own class: transient, retry later. It must never be confused with
//! `Storage` (corruption / unexpected failure) by an exhaustive match.

use rusty_core::Error;

/// What a daemon does with each class. Exhaustive on purpose: adding a variant breaks this
/// test until the new class is classified.
fn class(e: &Error) -> &'static str {
    match e {
        Error::NotFound(_) => "not-found",
        Error::Conflict(_) => "conflict",
        Error::Quota(_) => "quota",
        Error::Invalid(_) => "invalid",
        Error::Busy(_) => "busy",
        Error::Storage { .. } => "storage",
    }
}

#[test]
fn busy_is_distinct_from_storage() {
    let busy = Error::Busy("database busy".into());
    let storage = Error::storage("database error");
    assert_eq!(class(&busy), "busy");
    assert_eq!(class(&storage), "storage");
    assert!(!matches!(busy, Error::Storage { .. }));
    assert!(!matches!(storage, Error::Busy(_)));
}

#[test]
fn busy_displays_its_fixed_reason_only() {
    assert_eq!(Error::Busy("database busy".into()).to_string(), "busy: database busy");
}
