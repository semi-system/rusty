//! The calendars and address books (migration 0058, docs/design-caldav-carddav.md): collections per account with their quotas, objects kept as uploaded with an entity tag, the preconditions of a write, the UID
//! rule, the range query, and the sync report with its tombstones.

mod common;

use common::pg::{pg_admin_url, PgTestDb};
use rusty_core::AccountId;
use rusty_store::{DavDeleteResult, DavPrecondition, DavPutResult, NewDavObject, Store, StoreConfig, MAX_COLLECTIONS_PER_ACCOUNT};

fn event(name: &str, uid: &str, start: Option<i64>, end: Option<i64>) -> NewDavObject {
    NewDavObject {
        name: name.to_owned(),
        uid: uid.to_owned(),
        body: format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"),
        component: "VEVENT".to_owned(),
        starts_at: start,
        ends_at: end,
        recurring: false,
    }
}

async fn account(store: &Store, addr: &str) -> AccountId {
    let (local, domain) = addr.split_once('@').unwrap();
    let dir = store.repos().directory.clone();
    let dom = match dir.find_domain(domain).await.unwrap() {
        Some(d) => d.id,
        None => dir.create_domain(domain).await.unwrap(),
    };
    dir.create_account(&rusty_store::repo::NewAccount { local: local.to_owned(), domain: dom, plan: None, password_hash: None }).await.unwrap()
}

#[tokio::test]
async fn collections_are_per_account_made_once_with_defaults_and_bounded() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&url).await;
    let store = Store::open(StoreConfig::postgres(db.role_url("rusty_app"))).await.unwrap();
    let dav = store.repos().dav.clone();
    let alice = account(&store, "alice@one.test").await;
    let bob = account(&store, "bob@one.test").await;
    // A new account gets a calendar and an address book, once; they are not made again after the person removed them.
    dav.ensure_defaults(alice).await.unwrap();
    dav.ensure_defaults(alice).await.unwrap();
    assert_eq!(dav.collections(alice, "calendar").await.unwrap().len(), 1);
    assert_eq!(dav.collections(alice, "addressbook").await.unwrap()[0].display_name, "Contacts");
    assert!(dav.delete_collection(alice, "calendar", "default").await.unwrap());
    dav.ensure_defaults(alice).await.unwrap();
    assert!(dav.collections(alice, "calendar").await.unwrap().is_empty(), "the person's removal stands");
    // Another account sees none of it.
    assert!(dav.collections(bob, "addressbook").await.unwrap().is_empty());
    assert!(dav.collection(bob, "addressbook", "default").await.unwrap().is_none());
    // Names are the table's; a taken name and the number of collections are refused.
    let work = dav.create_collection(alice, "calendar", "work", "Work", "").await.unwrap();
    assert_eq!((work.revision, work.display_name.as_str()), (0, "Work"));
    assert!(matches!(dav.create_collection(alice, "calendar", "work", "again", "").await, Err(rusty_core::Error::Conflict(_))));
    for bad in ["", "a b", "a/b", &"x".repeat(129)] {
        assert!(matches!(dav.create_collection(alice, "calendar", bad, "", "").await, Err(rusty_core::Error::Invalid(_))), "{bad:?}");
    }
    assert!(dav.create_collection(alice, "tasks", "x", "", "").await.is_err());
    for i in 0..MAX_COLLECTIONS_PER_ACCOUNT - 2 {
        dav.create_collection(alice, "calendar", &format!("c{i}"), "", "").await.unwrap();
    }
    assert!(matches!(dav.create_collection(alice, "calendar", "one-more", "", "").await, Err(rusty_core::Error::Quota(_))));
    // Properties change by field; a colour that is not one is refused.
    assert!(dav.update_collection(alice, "calendar", "work", Some("Job"), None, Some("#112233")).await.unwrap());
    let got = dav.collection(alice, "calendar", "work").await.unwrap().unwrap();
    assert_eq!((got.display_name.as_str(), got.color.as_str()), ("Job", "#112233"));
    assert!(dav.update_collection(alice, "calendar", "work", None, None, Some("red")).await.is_err());
    assert!(!dav.update_collection(bob, "calendar", "work", Some("mine"), None, None).await.unwrap());
}

#[tokio::test]
async fn an_object_is_kept_as_uploaded_with_an_entity_tag_and_the_preconditions_and_the_uid_rule_hold() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&url).await;
    let store = Store::open(StoreConfig::postgres(db.role_url("rusty_app"))).await.unwrap();
    let dav = store.repos().dav.clone();
    let alice = account(&store, "alice@one.test").await;
    let cal = dav.create_collection(alice, "calendar", "main", "", "").await.unwrap().id;
    let other = dav.create_collection(alice, "calendar", "other", "", "").await.unwrap().id;
    let a = event("a.ics", "uid-a", Some(1000), Some(2000));
    let DavPutResult::Created { etag, revision } = dav.put(cal, &a, &DavPrecondition::Absent).await.unwrap() else { panic!() };
    assert_eq!(revision, 1);
    let got = dav.object(cal, "a.ics").await.unwrap().unwrap();
    assert_eq!((got.body.as_str(), got.etag.as_str(), got.size), (a.body.as_str(), etag.as_str(), a.body.len() as i64));
    // The same text has the same tag in another collection; another text has another.
    let DavPutResult::Created { etag: same, .. } = dav.put(other, &a, &DavPrecondition::None).await.unwrap() else { panic!() };
    assert_eq!(same, etag);
    // If-None-Match: * on an object that is there; If-Match with a wrong or a right tag.
    assert_eq!(dav.put(cal, &a, &DavPrecondition::Absent).await.unwrap(), DavPutResult::PreconditionFailed);
    let changed = NewDavObject { body: a.body.replace("uid-a", "uid-a\r\nSUMMARY:x"), ..a.clone() };
    assert_eq!(dav.put(cal, &changed, &DavPrecondition::Etag("stale".into())).await.unwrap(), DavPutResult::PreconditionFailed);
    let DavPutResult::Updated { etag: new_tag, revision } = dav.put(cal, &changed, &DavPrecondition::Etag(etag.clone())).await.unwrap() else { panic!() };
    assert_eq!(revision, 2);
    assert_ne!(new_tag, etag);
    assert_eq!(dav.put(cal, &changed, &DavPrecondition::Etag("x".into())).await.unwrap(), DavPutResult::PreconditionFailed, "a missing object does not match If-Match");
    assert_eq!(dav.put(cal, &event("new.ics", "uid-new", None, None), &DavPrecondition::Etag("x".into())).await.unwrap(), DavPutResult::PreconditionFailed);
    // A second object with the UID of the first is refused, naming the first; in another collection it is fine.
    assert_eq!(dav.put(cal, &event("b.ics", "uid-a", None, None), &DavPrecondition::None).await.unwrap(), DavPutResult::UidConflict("a.ics".into()));
    assert!(matches!(dav.put(other, &event("b.ics", "uid-a-other", None, None), &DavPrecondition::None).await.unwrap(), DavPutResult::Created { .. }));
    // The table's checks: a body over the limit and a component that is not one.
    assert!(dav.put(cal, &NewDavObject { body: "x".repeat(1024 * 1024 + 1), ..event("big.ics", "big", None, None) }, &DavPrecondition::None).await.is_err());
    assert!(dav.put(cal, &NewDavObject { component: "VALARM".into(), ..event("c.ics", "c", None, None) }, &DavPrecondition::None).await.is_err());
    assert!(dav.put(cal, &event("bad name.ics", "d", None, None), &DavPrecondition::None).await.is_err());
    // A collection that is gone ends the write; a delete honours its tag.
    assert!(dav.put(9_999_999, &a, &DavPrecondition::None).await.is_err());
    assert_eq!(dav.delete(cal, "a.ics", &DavPrecondition::Etag("stale".into())).await.unwrap(), DavDeleteResult::PreconditionFailed);
    assert_eq!(dav.delete(cal, "a.ics", &DavPrecondition::Etag(new_tag)).await.unwrap(), DavDeleteResult::Deleted { revision: 3 });
    assert_eq!(dav.delete(cal, "a.ics", &DavPrecondition::None).await.unwrap(), DavDeleteResult::NotFound);
    assert!(dav.object(cal, "a.ics").await.unwrap().is_none());
    // Taking the collection takes its objects with it.
    assert!(dav.delete_collection(alice, "calendar", "other").await.unwrap());
    assert!(dav.object(other, "b.ics").await.unwrap().is_none());
}

#[tokio::test]
async fn the_range_query_finds_what_meets_the_range_and_what_has_no_bounds() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&url).await;
    let store = Store::open(StoreConfig::postgres(db.role_url("rusty_app"))).await.unwrap();
    let dav = store.repos().dav.clone();
    let alice = account(&store, "alice@one.test").await;
    let cal = dav.create_collection(alice, "calendar", "main", "", "").await.unwrap().id;
    for (name, s, e) in [("early", Some(100), Some(200)), ("inside", Some(1100), Some(1200)), ("across", Some(900), Some(1500)), ("late", Some(5000), Some(6000)), ("open", Some(50), None), ("nobounds", None, None), ("instant", Some(1000), Some(1000))] {
        dav.put(cal, &event(&format!("{name}.ics"), name, s, e), &DavPrecondition::None).await.unwrap();
    }
    let names = |v: Vec<rusty_store::DavObject>| v.into_iter().map(|o| o.name.trim_end_matches(".ics").to_owned()).collect::<Vec<_>>();
    // [1000, 2000): what ends after 1000 and starts before 2000; an event with no end meets it (it may go on), one without bounds always does.
    assert_eq!(names(dav.objects_in_range(cal, Some("VEVENT"), Some(1000), Some(2000), 100).await.unwrap()), ["across", "inside", "instant", "nobounds", "open"]);
    assert_eq!(names(dav.objects_in_range(cal, None, None, Some(150), 100).await.unwrap()), ["early", "nobounds", "open"]);
    assert_eq!(names(dav.objects_in_range(cal, None, Some(5500), None, 100).await.unwrap()), ["late", "nobounds", "open"]);
    assert!(dav.objects_in_range(cal, Some("VTODO"), None, None, 100).await.unwrap().is_empty());
    assert_eq!(dav.objects_in_range(cal, None, None, None, 3).await.unwrap().len(), 3, "a limit is a limit");
    assert_eq!(dav.objects(cal, 100).await.unwrap().len(), 7);
    let some = dav.objects_named(cal, &["early.ics".to_owned(), "missing.ics".to_owned(), "late.ics".to_owned()]).await.unwrap();
    assert_eq!(names(some), ["early", "late"]);
}

#[tokio::test]
async fn the_sync_report_gives_what_changed_and_what_was_deleted_in_pages_and_a_token_older_than_the_collected_tombstones_needs_a_full_sync() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&url).await;
    let store = Store::open(StoreConfig::postgres(db.role_url("rusty_app"))).await.unwrap();
    let admin = sqlx::PgPool::connect(db.admin_db_url()).await.unwrap();
    let dav = store.repos().dav.clone();
    let alice = account(&store, "alice@one.test").await;
    let cal = dav.create_collection(alice, "calendar", "main", "", "").await.unwrap().id;
    for n in ["a", "b", "c"] {
        dav.put(cal, &event(&format!("{n}.ics"), n, None, None), &DavPrecondition::None).await.unwrap();
    }
    // A first sync (token 0) gets everything.
    let first = dav.changes_since(cal, 0, 100).await.unwrap().unwrap();
    assert_eq!((first.changed.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), first.deleted.is_empty(), first.revision, first.truncated), (vec!["a.ics", "b.ics", "c.ics"], true, 3, false));
    // Then a change, a delete and a create again of the deleted name.
    dav.put(cal, &event("a.ics", "a", Some(1), Some(2)), &DavPrecondition::None).await.unwrap();
    dav.delete(cal, "b.ics", &DavPrecondition::None).await.unwrap();
    let second = dav.changes_since(cal, first.revision, 100).await.unwrap().unwrap();
    assert_eq!((second.changed.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), second.deleted.clone(), second.revision), (vec!["a.ics"], vec!["b.ics".to_owned()], 5));
    dav.put(cal, &event("b.ics", "b", None, None), &DavPrecondition::None).await.unwrap();
    let third = dav.changes_since(cal, second.revision, 100).await.unwrap().unwrap();
    assert_eq!((third.changed.len(), third.deleted.is_empty()), (1, true), "a name that is back is not reported as deleted");
    // Nothing changed: the same token back.
    let none = dav.changes_since(cal, third.revision, 100).await.unwrap().unwrap();
    assert_eq!((none.changed.is_empty(), none.deleted.is_empty(), none.revision), (true, true, third.revision));
    // Pages: a limit of two out of three, and the token is where the page ended.
    let page = dav.changes_since(cal, 0, 2).await.unwrap().unwrap();
    assert_eq!((page.changed.len(), page.truncated), (2, true));
    let rest = dav.changes_since(cal, page.revision, 2).await.unwrap().unwrap();
    assert!(!rest.truncated && rest.revision >= page.revision);
    // Tombstones older than the keep time are dropped and the floor goes up; fresh ones stay.
    dav.delete(cal, "c.ics", &DavPrecondition::None).await.unwrap();
    assert_eq!(dav.purge_tombstones().await.unwrap(), 0);
    sqlx::query("UPDATE dav_tombstones SET deleted_at = deleted_at - 40 * 24 * 3600").execute(&admin).await.unwrap();
    assert_eq!(dav.purge_tombstones().await.unwrap(), 1);
    assert!(dav.changes_since(cal, 2, 100).await.unwrap().is_none(), "a token below the floor needs a full sync");
    assert!(dav.changes_since(cal, 0, 100).await.unwrap().is_some(), "a first sync never does");
    assert!(dav.changes_since(9_999_999, 0, 10).await.is_err());
}

#[tokio::test]
async fn the_collector_drops_old_tombstones_logged_in_as_its_own_role_and_the_dav_service_as_its_own() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&url).await;
    let operator = Store::open(StoreConfig::postgres(db.role_url("rusty_app"))).await.unwrap();
    let service = Store::open(StoreConfig::postgres(db.role_url("rusty_dav"))).await.unwrap();
    let collector = Store::open(StoreConfig::postgres(db.role_url("rusty_gc"))).await.unwrap();
    let admin = sqlx::PgPool::connect(db.admin_db_url()).await.unwrap();
    let alice = account(&operator, "alice@one.test").await;
    // The service makes and removes objects as its own role.
    let dav = service.repos().dav.clone();
    let cal = dav.create_collection(alice, "calendar", "main", "", "").await.unwrap().id;
    dav.put(cal, &event("a.ics", "a", None, None), &DavPrecondition::None).await.unwrap();
    assert_eq!(dav.delete(cal, "a.ics", &DavPrecondition::None).await.unwrap(), DavDeleteResult::Deleted { revision: 2 });
    // The collector keeps a fresh tombstone and drops an old one, as its own role.
    assert_eq!(collector.repos().dav.purge_tombstones().await.unwrap(), 0);
    sqlx::query("UPDATE dav_tombstones SET deleted_at = deleted_at - 40 * 24 * 3600").execute(&admin).await.unwrap();
    assert_eq!(collector.repos().dav.purge_tombstones().await.unwrap(), 1);
    assert!(dav.changes_since(cal, 1, 10).await.unwrap().is_none());
}
