//! The hosted zones of the name server (migration 0055): a domain is `external` until its administrator chooses; the name server, logged in as its own role, reads the hosted domains with the public half of their
//! DKIM keys, the SPF include of their relay and the extra records, and can change nothing.

mod common;

use common::pg::{pg_admin_url, PgTestDb};
use rusty_store::{Store, StoreConfig, MAX_EXTRA_RECORDS};

#[tokio::test]
async fn the_name_server_reads_the_hosted_domains_with_their_keys_and_extra_records_and_nothing_else() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&url).await;
    let operator = Store::open(StoreConfig::postgres(db.role_url("rusty_app"))).await.unwrap();
    let dns_server = Store::open(StoreConfig::postgres(db.role_url("rusty_dns"))).await.unwrap();
    let dir = operator.repos().directory.clone();
    let hosted = dir.create_domain("hosted.test").await.unwrap();
    let plain = dir.create_domain("plain.test").await.unwrap();
    let repo = operator.repos().dns_zone.clone();
    // Every domain starts as it is today.
    assert_eq!(repo.mode(hosted).await.unwrap(), "external");
    assert!(dns_server.repos().dns_zone.hosted().await.unwrap().is_empty());
    assert!(repo.set_mode(hosted, "sideways").await.is_err());
    repo.set_mode(hosted, "hosted").await.unwrap();
    operator.repos().dns.add_dkim_key(hosted, "s1", "ed25519", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", None).await.unwrap();
    operator.repos().dns.add_dkim_key(plain, "s9", "ed25519", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=", None).await.unwrap();
    let a = repo.add_extra(hosted, "www", "CNAME", "hosted.test", 300).await.unwrap();
    let b = repo.add_extra(hosted, "@", "TXT", "google-site-verification=abc", 3600).await.unwrap();
    repo.add_extra(plain, "www", "A", "192.0.2.1", 300).await.unwrap();
    assert!(repo.add_extra(hosted, "Bad Name", "A", "192.0.2.1", 300).await.is_err(), "the database checks the name");
    assert!(repo.add_extra(hosted, "x", "AXFR", "1", 300).await.is_err(), "and the type");
    assert!(repo.add_extra(hosted, "x", "A", "1\n2", 300).await.is_err(), "and the value");

    // The name server sees only the hosted domain, with its key and its two records.
    let seen = dns_server.repos().dns_zone.hosted().await.unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].name, "hosted.test");
    assert_eq!(seen[0].dkim.len(), 1);
    assert_eq!(seen[0].dkim[0].0, "s1");
    assert_eq!(seen[0].extra.iter().map(|e| e.id).collect::<Vec<_>>(), [a, b]);
    assert_eq!((seen[0].extra[0].name.as_str(), seen[0].extra[0].rtype.as_str(), seen[0].extra[0].ttl), ("www", "CNAME", 300));

    // It can change nothing and read nothing of the mail side.
    assert!(dns_server.repos().dns_zone.set_mode(plain, "hosted").await.is_err());
    assert!(dns_server.repos().dns_zone.add_extra(hosted, "y", "A", "192.0.2.2", 60).await.is_err());
    assert!(dns_server.repos().directory.create_domain("evil.test").await.is_err());

    // Removing a record, and one of another domain is not found.
    assert!(!repo.remove_extra(plain, a).await.unwrap());
    assert!(repo.remove_extra(hosted, a).await.unwrap());
    assert_eq!(repo.extras(hosted).await.unwrap().len(), 1);
    // The limit.
    for i in 0..(MAX_EXTRA_RECORDS - 1) {
        repo.add_extra(hosted, &format!("h{i}"), "A", "192.0.2.3", 60).await.unwrap();
    }
    assert!(repo.add_extra(hosted, "over", "A", "192.0.2.4", 60).await.is_err());
    // Back to external: the name server stops answering for it at its next look.
    repo.set_mode(hosted, "external").await.unwrap();
    assert!(dns_server.repos().dns_zone.hosted().await.unwrap().is_empty());
}
