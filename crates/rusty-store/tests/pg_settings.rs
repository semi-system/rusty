//! The protocol switches (migration 0030, docs/design-protocols.md): a protocol never set is absent from the list (the readers treat it as on), a set value is kept and replaced,
//! an unknown protocol is refused, and the roles: a service may read the table and not write it; the admin web writes it.

mod common;

use common::pg::{pg_admin_url, PgTestDb};
use rusty_store::repo::pg::PgRepo;
use rusty_store::{PgStore, SettingsRepo};

#[tokio::test]
async fn a_switch_is_stored_replaced_and_an_unknown_protocol_is_refused() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create(&url).await;
    let r = PgRepo::new(PgStore::connect(db.url()).await.unwrap());
    assert!(r.protocols().await.unwrap().is_empty(), "nothing is stored until the operator sets something");
    r.set_protocol("pop3", false, Some(100)).await.unwrap();
    assert_eq!(r.protocols().await.unwrap(), vec![("pop3".to_owned(), false)]);
    r.set_protocol("pop3", true, Some(200)).await.unwrap();
    assert_eq!(r.protocols().await.unwrap(), vec![("pop3".to_owned(), true)]);
    assert!(r.set_protocol("telnet", true, None).await.is_err(), "the list of protocols is closed");
}
