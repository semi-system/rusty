#![allow(clippy::field_reassign_with_default)]
//! A Sieve script on delivery (docs/design-sieve.md): the active script of the account decides where a message goes, end to end over real SMTP and a real store, on every backend. A script that does not parse,
//! and an account without one, change nothing.

mod common;

use common::{for_each_backend, start, Backend, Client, Opts};
use rusty_core::{AccountId, FolderId};

const LIST_MSG: &[u8] = b"From: Dev List <dev@lists.example.org>\r\nTo: alice@example.net\r\nSubject: [dev] weekly\r\nList-Id: <dev.lists.example.org>\r\n\r\nnews\r\n";
const PLAIN_MSG: &[u8] = b"From: Bob <bob@remote.example>\r\nTo: alice@example.net\r\nSubject: hello\r\n\r\nbody\r\n";

async fn install(b: &Backend, account: AccountId, script: &str) {
    let repo = b.seed_store.repos().sieve.clone();
    repo.put(account, "main", script).await.unwrap();
    repo.set_active(account, Some("main")).await.unwrap();
}

async fn folder_named(b: &Backend, account: AccountId, blobs: &std::sync::Arc<dyn rusty_store::BlobStore>, name: &str) -> Option<FolderId> {
    let mb = rusty_store::Mailbox::open_with(b.store.repos().mailbox.clone(), blobs.clone(), account).await.unwrap();
    mb.list_folders().await.unwrap().into_iter().find(|f| f.name == name).map(|f| f.id)
}

async fn send(addr: std::net::SocketAddr, body: &[u8]) -> String {
    let mut c = Client::hello(addr).await;
    let r = c.mail("sender@remote.example", &["alice@example.net"], body.strip_suffix(b"\r\n").unwrap()).await;
    let _ = c.cmd("QUIT").await;
    r
}

#[tokio::test]
async fn fileinto_puts_the_message_in_a_folder_made_for_it_and_nowhere_else() {
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        install(&b, alice, "require \"fileinto\";\nif header :contains \"list-id\" \"lists.example.org\" { fileinto \"Lists\"; }\n").await;
        let s = start(&b, Opts::default()).await;
        assert!(send(s.addr, LIST_MSG).await.starts_with("250"));
        assert!(b.inbox(alice, &s.blobs).await.is_empty(), "filed away, not kept");
        let lists = folder_named(&b, alice, &s.blobs, "Lists").await.expect("the folder was made");
        assert_eq!(b.folder(alice, lists, &s.blobs).await.len(), 1);
        // Another message goes the usual way, and the existing folder is used again.
        assert!(send(s.addr, PLAIN_MSG).await.starts_with("250"));
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 1);
        assert!(send(s.addr, &LIST_MSG.replace_subject(b"second")).await.starts_with("250"));
        assert_eq!(b.folder(alice, lists, &s.blobs).await.len(), 2);
        s.stop().await;
    })
    .await;
}

trait Subject {
    fn replace_subject(&self, s: &[u8]) -> Vec<u8>;
}

impl Subject for &[u8] {
    fn replace_subject(&self, s: &[u8]) -> Vec<u8> {
        let text = String::from_utf8_lossy(self).replace("[dev] weekly", &String::from_utf8_lossy(s));
        text.into_bytes()
    }
}

#[tokio::test]
async fn keep_with_fileinto_stores_two_copies_and_discard_stores_none() {
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        install(&b, alice, "require \"fileinto\";\nfileinto \"Archive2\";\nkeep;\n").await;
        let s = start(&b, Opts::default()).await;
        assert!(send(s.addr, PLAIN_MSG).await.starts_with("250"));
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 1);
        let f = folder_named(&b, alice, &s.blobs, "Archive2").await.unwrap();
        assert_eq!(b.folder(alice, f, &s.blobs).await.len(), 1);
        install(&b, alice, "discard;\n").await;
        assert!(send(s.addr, PLAIN_MSG.strip_suffix(b"body\r\n").map(|p| [p, b"other\r\n"].concat()).unwrap().as_slice()).await.starts_with("250"), "a discard is accepted");
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 1, "and nothing more is stored");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn reject_refuses_the_message_at_the_door_with_the_words_of_the_script() {
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        install(&b, alice, "require \"reject\";\nif header :contains \"subject\" \"hello\" { reject \"No thanks, not here\"; }\n").await;
        let s = start(&b, Opts::default()).await;
        let r = send(s.addr, PLAIN_MSG).await;
        assert!(r.starts_with("550 5.7.1") && r.contains("No thanks, not here"), "{r}");
        assert!(b.inbox(alice, &s.blobs).await.is_empty());
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_script_that_does_not_parse_changes_nothing_and_an_inactive_one_is_not_run() {
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        let repo = b.seed_store.repos().sieve.clone();
        install(&b, alice, "this is not sieve").await;
        let s = start(&b, Opts::default()).await;
        assert!(send(s.addr, PLAIN_MSG).await.starts_with("250"));
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 1, "a broken script never loses mail");
        repo.put(alice, "main", "discard;").await.unwrap();
        repo.set_active(alice, None).await.unwrap();
        assert!(send(s.addr, PLAIN_MSG.strip_suffix(b"body\r\n").map(|p| [p, b"again\r\n"].concat()).unwrap().as_slice()).await.starts_with("250"));
        assert_eq!(b.inbox(alice, &s.blobs).await.len(), 2, "not active: not run");
        s.stop().await;
    })
    .await;
}

#[tokio::test]
async fn a_redirect_queues_a_job_for_the_other_address_and_keeps_nothing_here() {
    for_each_backend(|b| async move {
        let (alice, _) = b.seed().await;
        install(&b, alice, "redirect \"elsewhere@other.example\";\n").await;
        let mut o = Opts::default();
        o.forward = true;
        let s = start(&b, o).await;
        assert!(send(s.addr, PLAIN_MSG).await.starts_with("250"));
        assert!(b.inbox(alice, &s.blobs).await.is_empty(), "a redirect cancels the implicit keep");
        assert_eq!(b.count("jobs").await, 1, "one outbound job for the new address");
        s.stop().await;
    })
    .await;
}
