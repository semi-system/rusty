//! What the webmail queues is sent by the submission service through the steps of an authenticated submission (docs/design-webmail.md, sending): the From check, the sending budget, the local copies, the
//! outbound queue for the others, and a copy in the Sent folder.

mod common;

use std::sync::Arc;

use bytes::Bytes;
use common::{for_each_backend, make_context, Backend};
use rusty_core::AccountId;
use rusty_smtp::inbound::{InboundHandler, InboundOptions, SubmissionDeps};
use rusty_smtp::limits::MemorySubmissionLimiter;
use rusty_smtp::outbound_queue::JobOutbound;
use rusty_smtp::webmail_send::WebmailSendService;
use rusty_svc::SystemClock;

fn handler(ctx: &rusty_svc::Context, per_hour: u32) -> InboundHandler {
    let deps = SubmissionDeps {
        outbound: Arc::new(JobOutbound::new(ctx.jobs.clone(), ctx.directory.clone())),
        limiter: Arc::new(MemorySubmissionLimiter::new(per_hour, Arc::new(SystemClock::new()))),
        remote_enabled: true,
        default_per_hour: per_hour,
    };
    InboundHandler::submission(ctx, InboundOptions::default(), deps)
}

async fn queue(b: &Backend, ctx: &rusty_svc::Context, account: AccountId, sender: &str, rcpts: &[&str], message: &str) -> i64 {
    let blob = ctx.blobs.put(Bytes::copy_from_slice(message.as_bytes())).await.unwrap();
    let rcpts: Vec<String> = rcpts.iter().map(|r| (*r).to_owned()).collect();
    b.seed_store.repos().outbox.enqueue(account, sender, blob.as_str(), message.len() as i64, &rcpts, "hello").await.unwrap()
}

async fn state(b: &Backend, account: AccountId, id: i64) -> (String, Option<String>) {
    let list = b.seed_store.repos().outbox.list(account, 50).await.unwrap();
    let i = list.into_iter().find(|i| i.id == id).unwrap();
    (i.state, i.error)
}

const GOOD: &str = "From: alice@example.net\r\nTo: bob@example.net\r\nSubject: hello\r\nMessage-ID: <1@example.net>\r\n\r\nbody\r\n";

#[tokio::test]
async fn a_queued_message_reaches_the_local_and_the_remote_recipients_and_the_sent_folder() {
    for_each_backend(|b| async move {
        let (alice, bob) = b.seed().await;
        let (ctx, blobs) = make_context(&b).await;
        let h = handler(&ctx, 100);
        let id = queue(&b, &ctx, alice, "alice@example.net", &["bob@example.net", "far@elsewhere.example"], GOOD).await;
        assert!(WebmailSendService::run_once(&h, &ctx).await.unwrap());
        assert_eq!(state(&b, alice, id).await, ("sent".to_owned(), None));
        let got = b.inbox(bob, &blobs).await;
        assert_eq!(got.len(), 1);
        assert!(String::from_utf8_lossy(&got[0]).contains("Subject: hello"));
        assert_eq!(b.count("jobs").await, 1, "one job for the remote domain");
        let sent = b.folder(alice, rusty_core::FolderId(2), &blobs).await;
        assert_eq!(sent.len(), 1, "a copy in the Sent folder");
        // Nothing more to do.
        assert!(!WebmailSendService::run_once(&h, &ctx).await.unwrap());
    })
    .await;
}

#[tokio::test]
async fn a_message_that_is_not_from_the_account_is_refused_and_nothing_is_delivered() {
    for_each_backend(|b| async move {
        let (alice, bob) = b.seed().await;
        let (ctx, blobs) = make_context(&b).await;
        let h = handler(&ctx, 100);
        // The From header names Bob.
        let forged = GOOD.replace("From: alice@", "From: bob@");
        let a = queue(&b, &ctx, alice, "alice@example.net", &["bob@example.net"], &forged).await;
        // The envelope sender is Bob's address.
        let c = queue(&b, &ctx, alice, "bob@example.net", &["bob@example.net"], GOOD).await;
        while WebmailSendService::run_once(&h, &ctx).await.unwrap() {}
        assert_eq!(state(&b, alice, a).await, ("failed".to_owned(), Some("not_yours".to_owned())));
        assert_eq!(state(&b, alice, c).await, ("failed".to_owned(), Some("not_yours".to_owned())));
        assert!(b.inbox(bob, &blobs).await.is_empty());
        assert_eq!(b.count("jobs").await, 0);
    })
    .await;
}

#[tokio::test]
async fn the_sending_budget_of_the_account_applies() {
    for_each_backend(|b| async move {
        let (alice, _bob) = b.seed().await;
        let (ctx, _blobs) = make_context(&b).await;
        let h = handler(&ctx, 1);
        let first = queue(&b, &ctx, alice, "alice@example.net", &["bob@example.net"], GOOD).await;
        let second = queue(&b, &ctx, alice, "alice@example.net", &["bob@example.net"], &GOOD.replace("<1@", "<2@")).await;
        while WebmailSendService::run_once(&h, &ctx).await.unwrap() {}
        assert_eq!(state(&b, alice, first).await.0, "sent");
        assert_eq!(state(&b, alice, second).await, ("failed".to_owned(), Some("limit".to_owned())));
    })
    .await;
}
