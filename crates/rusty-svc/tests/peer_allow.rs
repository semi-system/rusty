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
