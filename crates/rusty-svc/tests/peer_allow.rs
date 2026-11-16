//! `PeerAllow`: only the listed networks may connect; every other peer is dropped before a byte is read.

use std::sync::Arc;

use rusty_core::cidr::Cidr;
use rusty_svc::{Accept, DirectTcp, PeerAllow};
use tokio::net::{TcpListener, TcpStream};

fn cidr(s: &str) -> Cidr {
    s.parse().unwrap()
}

/// One accepted connection from 127.0.0.1, run through the accepter.
async fn through(allowed: Vec<Cidr>) -> bool {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let client = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
    let (tcp, peer) = listener.accept().await.unwrap();
    let _client = client.await.unwrap();
    PeerAllow::new(Arc::new(DirectTcp), allowed).accept(tcp, peer).await.is_ok()
}

#[tokio::test]
async fn a_peer_in_a_listed_network_is_accepted() {
    assert!(through(vec![cidr("127.0.0.1/32")]).await);
    assert!(through(vec![cidr("10.77.0.2/32"), cidr("127.0.0.0/8")]).await, "any of the blocks is enough");
}

#[tokio::test]
async fn a_peer_outside_every_listed_network_is_dropped() {
    assert!(!through(vec![cidr("10.77.0.2/32")]).await);
    assert!(!through(vec![cidr("192.168.0.0/16"), cidr("10.0.0.0/8")]).await);
    assert!(!through(Vec::new()).await, "an empty list in the accepter allows nobody (the configuration leaves the accepter out instead)");
}

/// As `through`, with names as well as blocks.
async fn through_named(named: rusty_svc::NamedPeers, allowed: Vec<Cidr>) -> bool {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let client = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
    let (tcp, peer) = listener.accept().await.unwrap();
    let _client = client.await.unwrap();
    PeerAllow::new(Arc::new(DirectTcp), allowed).with_named(named).accept(tcp, peer).await.is_ok()
}

#[tokio::test]
async fn a_peer_may_be_allowed_by_the_name_it_resolves_from() {
    let stop = tokio_util::sync::CancellationToken::new();
    // `localhost` resolves to the address the test connects from.
    let named = rusty_svc::NamedPeers::start(vec!["localhost".into()], std::time::Duration::from_secs(60), stop.clone()).await;
    assert!(through_named(named, Vec::new()).await, "the address of an allowed name is let in");
    let other = rusty_svc::NamedPeers::start(vec!["no-such-service.invalid".into()], std::time::Duration::from_secs(60), stop.clone()).await;
    assert!(!through_named(other, Vec::new()).await, "a name that does not resolve admits no one");
    assert!(!through_named(rusty_svc::NamedPeers::fixed(["10.1.2.3".parse().unwrap()]), Vec::new()).await, "an address of a name that is not the peer's");
    assert!(through_named(rusty_svc::NamedPeers::none(), vec![cidr("127.0.0.1/32")]).await, "blocks still work beside names");
    stop.cancel();
}

#[tokio::test]
async fn the_named_set_follows_the_names_and_keeps_the_last_answer_when_a_lookup_fails() {
    let stop = tokio_util::sync::CancellationToken::new();
    let p = rusty_svc::NamedPeers::start(vec!["localhost".into(), "no-such-service.invalid".into()], std::time::Duration::from_millis(50), stop.clone()).await;
    assert!(p.contains("127.0.0.1".parse().unwrap()) || p.contains("::1".parse().unwrap()), "localhost resolved");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(p.contains("127.0.0.1".parse().unwrap()) || p.contains("::1".parse().unwrap()), "still there after several refreshes");
    assert!(!rusty_svc::NamedPeers::none().contains("127.0.0.1".parse().unwrap()));
    assert!(rusty_svc::NamedPeers::none().is_empty());
    stop.cancel();
}
