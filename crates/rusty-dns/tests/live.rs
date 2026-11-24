//! The name server against a real database, logged in as its own role, and a real socket.

use std::net::SocketAddr;
use std::time::Duration;

use rusty_core::config::{Role, StartupConfig};
use rusty_dns::adapter::testkit::{question, reply, Reply, REFUSED};
use rusty_dns::run::{healthy, start};
use rusty_store::fixtures::pg_db::{pg_admin_url, PgTestDb};
use rusty_store::{Store, StoreConfig};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

async fn ask(addr: SocketAddr, name: &str, qtype: u16) -> Option<Reply> {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    s.send_to(&question(7, name, qtype), addr).await.unwrap();
    let mut buf = vec![0u8; 4096];
    match tokio::time::timeout(Duration::from_secs(2), s.recv_from(&mut buf)).await {
        Ok(Ok((n, _))) => Some(reply(&buf[..n])),
        _ => None,
    }
}

#[tokio::test]
async fn a_hosted_domain_is_answered_from_the_database_and_a_change_shows_at_the_next_read() {
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create_with_roles(&url).await;
    let operator = Store::open(StoreConfig::postgres(db.role_url("rusty_app"))).await.unwrap();
    let service = Store::open(StoreConfig::postgres(db.role_url("rusty_dns"))).await.unwrap();
    let hosted = operator.repos().directory.create_domain("hosted.test").await.unwrap();
    operator.repos().directory.create_domain("plain.test").await.unwrap();
    operator.repos().dns_zone.set_mode(hosted, "hosted").await.unwrap();
    let text = "[store]\nurl = \"postgres://rusty_dns@db:5432/rusty\"\npassword_file = \"/run/secrets/pg\"\n[dns]\nlisten = [\"127.0.0.1:0\"]\nname_servers = [\"ns1.platform.example\", \"ns2.platform.example\"]\nhostmaster = \"hostmaster.platform.example\"\nmail_host = \"mail.platform.example\"\npublic_ips = [\"192.0.2.25\"]\nreload_secs = 1\n";
    let cfg = StartupConfig::from_toml_str_for(Role::Dns, text).unwrap();
    let stop = CancellationToken::new();
    let running = start(&cfg, &service, stop.clone()).await.unwrap();
    let addr = running.udp[0];
    assert!(healthy(addr, Duration::from_secs(2)).await);
    let ns = ask(addr, "hosted.test.", 2).await.expect("an answer");
    assert_eq!((ns.answers, ns.authoritative), (2, true));
    assert_eq!(ask(addr, "hosted.test.", 15).await.unwrap().answers, 1, "the MX");
    assert_eq!(ask(addr, "plain.test.", 15).await.unwrap().rcode, REFUSED, "a domain that is not hosted is not answered");
    // A change in the database shows at the next read.
    assert_eq!(ask(addr, "new.hosted.test.", 1).await.unwrap().answers, 0);
    operator.repos().dns_zone.add_extra(hosted, "new", "A", "192.0.2.77", 60).await.unwrap();
    let mut seen = false;
    for _ in 0..30 {
        if ask(addr, "new.hosted.test.", 1).await.is_some_and(|r| r.answers == 1) {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(seen, "the new record is served within a few reads");
    stop.cancel();
    let _ = running.task.await;
}
