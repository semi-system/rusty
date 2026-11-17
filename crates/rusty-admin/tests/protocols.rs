//! The protocol switches (docs/design-protocols.md): a platform administrator reads and sets them, nobody else can, a protocol never set is on, an unknown one is refused,
//! and the auto-setup answers list POP3 only while it is on.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn pop3_is_on_until_the_operator_turns_it_off_and_the_auto_setup_follows() {
    let r = rig().await;
    make_admin(&r, "root@home.test", false).await;
    let c = Client::sign_in(r.addr, "root@home.test").await;
    let listed = c.get("/api/v1/protocols").await;
    assert_eq!(listed.status, 200, "{}", listed.body);
    conforms(&listed.json(), "Protocols");
    assert_eq!(listed.json()["protocols"], json!([{ "name": "pop3", "enabled": true }]), "on by default");
    let xml = |path: &'static str| async move { send(r.addr, "GET", path, &[], "").await.body };
    assert!(xml("/mail/config-v1.1.xml?emailaddress=a@known.test").await.contains("<incomingServer type=\"pop3\">"));

    let off = c.mutate("PUT", "/api/v1/protocols", Some(json!({ "protocol": "pop3", "enabled": false }))).await;
    assert_eq!(off.status, 200, "{}", off.body);
    assert_eq!(off.json()["protocols"], json!([{ "name": "pop3", "enabled": false }]));
    assert!(!xml("/mail/config-v1.1.xml?emailaddress=a@known.test").await.contains("pop3"), "a protocol that is off is not offered to mail clients");

    let on = c.mutate("PUT", "/api/v1/protocols", Some(json!({ "protocol": "pop3", "enabled": true }))).await;
    assert_eq!(on.json()["protocols"], json!([{ "name": "pop3", "enabled": true }]));
    // The list is closed, and the body is exactly the two fields.
    assert_eq!(c.mutate("PUT", "/api/v1/protocols", Some(json!({ "protocol": "telnet", "enabled": true }))).await.status, 400);
    assert_eq!(c.mutate("PUT", "/api/v1/protocols", Some(json!({ "protocol": "pop3", "enabled": true, "x": 1 }))).await.status, 400);
}

#[tokio::test]
async fn a_domain_administrator_cannot_read_or_set_the_protocols_and_nobody_can_without_a_session() {
    let r = rig().await;
    make_admin(&r, "tenant@one.test", true).await;
    let c = Client::sign_in(r.addr, "tenant@one.test").await;
    assert_eq!(c.get("/api/v1/protocols").await.status, 403);
    assert_eq!(c.mutate("PUT", "/api/v1/protocols", Some(json!({ "protocol": "pop3", "enabled": false }))).await.status, 403);
    assert_eq!(send(r.addr, "GET", "/api/v1/protocols", &[], "").await.status, 401);
}
