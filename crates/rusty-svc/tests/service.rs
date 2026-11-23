//! `Context` and the service runner; the seam traits are wired by default and replaceable.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use rusty_core::{Error, MailboxId};
use rusty_directory::Directory;
use rusty_store::Repos;
use rusty_svc::{
    TransferService,
    run_services, Context, GcService, QuotaPolicy, RateLimiter, Service, ServiceEvent, ServiceEvents, TlsProvider,
    Verdict,
};
use tokio_util::sync::CancellationToken;

async fn ctx(p: &Parts) -> Context {
    ctx_over(p, p.store.repos()).await
}

/// A context over the given repositories: the operator's (`p.seed_store`) for a test of the context's own seams, which no one service role is responsible for.
async fn ctx_over(p: &Parts, repos: &Repos) -> Context {
    let cfg = Arc::new(dev_config(p.dir.path()));
    let tls = TlsProvider::from_config(&cfg.tls, "localhost", Arc::new(rusty_svc::FileSecretSource::new())).unwrap();
    Context::new(repos, p.blobs.clone(), cfg, tls).await.unwrap()
}

#[tokio::test]
async fn the_default_seams_are_wired_through_their_traits() {
    let p = parts().await;
    let c = ctx_over(&p, p.seed_store.repos()).await;

    // QuotaPolicy: size cap from the configuration, reached through the trait object.
    let q: &dyn QuotaPolicy = &*c.quota;
    let acct = rusty_core::AccountId(1);
    q.check_append(acct, 1024).unwrap();
    match q.check_append(acct, c.cfg.max_message_bytes + 1) {
        Err(Error::Quota(_)) => {}
        other => panic!("{other:?}"),
    }

    // RateLimiter: the in-memory auth throttle, limits from [limits].
    let r: &dyn RateLimiter = &*c.rate;
    let ip = "192.0.2.9".parse().unwrap();
    for _ in 0..c.cfg.limits.auth_failures_max {
        assert_eq!(r.check(ip, "a@x.test").await, Verdict::Allowed);
        r.record_failure(ip, "a@x.test").await;
    }
    assert!(matches!(r.check(ip, "a@x.test").await, Verdict::Blocked { .. }));

    // ServiceEvents: replaceable; the default tracing implementation accepts every event.
    c.events.emit(ServiceEvent::AuthResult { service: "t", peer: "192.0.2.9:1".parse().unwrap(), ok: false });

    // Hub: opens a mailbox through the repository trait only.
    let seed = Directory::with_repo(p.seed_store.repos().directory.clone());
    let dom = seed.create_domain("ctx.test").await.unwrap();
    let acc = seed.create_account("alice", dom, None, None).await.unwrap();
    let mb = c.hub.open_for(acc).await.unwrap();
    assert_eq!(c.hub.open_for(rusty_core::AccountId(9999)).await.err().map(|e| matches!(e, Error::NotFound(_))), Some(true));
    // and the hub's notifications are keyed by that mailbox
    let mut rx = c.hub.subscribe(mb.id());
    c.hub.notify(mb.id(), 1);
    rx.changed().await.unwrap();
    let _ = MailboxId(0);

    // Authenticator: wired to the directory (an unknown identity fails, it does not error).
    assert_eq!(c.auth.verify("nobody@ctx.test", "pw").await.unwrap(), None);
}

#[tokio::test]
async fn seams_can_be_replaced() {
    struct Deny;
    impl QuotaPolicy for Deny {
        fn check_append(&self, _: rusty_core::AccountId, _: u64) -> rusty_core::Result<()> {
            Err(Error::Quota("denied by test policy".into()))
        }
    }
    #[derive(Default)]
    struct Rec(Mutex<usize>);
    impl ServiceEvents for Rec {
        fn emit(&self, _: ServiceEvent<'_>) {
            *self.0.lock().unwrap() += 1;
        }
    }
    let p = parts().await;
    let rec = Arc::new(Rec::default());
    let c = ctx(&p).await.with_quota(Arc::new(Deny)).with_events(rec.clone());
    assert!(c.quota.check_append(rusty_core::AccountId(1), 1).is_err());
    c.events.emit(ServiceEvent::AuthResult { service: "t", peer: "192.0.2.9:1".parse().unwrap(), ok: true });
    assert_eq!(*rec.0.lock().unwrap(), 1);
}

struct Probe {
    name: &'static str,
    outcome: Outcome,
    stopped: Arc<Mutex<Vec<&'static str>>>,
}
enum Outcome {
    WaitForShutdown,
    Fail,
    Panic,
}
#[async_trait]
impl Service for Probe {
    fn name(&self) -> &'static str {
        self.name
    }
    async fn run(self: Box<Self>, _ctx: Context, shutdown: CancellationToken) -> rusty_core::Result<()> {
        match self.outcome {
            Outcome::WaitForShutdown => {
                shutdown.cancelled().await;
                self.stopped.lock().unwrap().push(self.name);
                Ok(())
            }
            Outcome::Fail => Err(Error::storage("boom")),
            Outcome::Panic => panic!("service bug"),
        }
    }
}

#[tokio::test]
async fn the_runner_stops_everything_when_one_service_fails_or_panics() {
    for outcome in [Outcome::Fail, Outcome::Panic] {
        let p = parts().await;
        let c = ctx(&p).await;
        let stopped = Arc::new(Mutex::new(vec![]));
        let services: Vec<Box<dyn Service>> = vec![
            Box::new(Probe { name: "a", outcome: Outcome::WaitForShutdown, stopped: stopped.clone() }),
            Box::new(Probe { name: "b", outcome, stopped: stopped.clone() }),
            Box::new(Probe { name: "c", outcome: Outcome::WaitForShutdown, stopped: stopped.clone() }),
        ];
        let token = CancellationToken::new();
        let r = tokio::time::timeout(Duration::from_secs(5), run_services(services, c, token.clone())).await.unwrap();
        assert!(r.is_err(), "a failed service must fail the run");
        assert!(token.is_cancelled(), "the others were told to stop");
        let mut s = stopped.lock().unwrap().clone();
        s.sort();
        assert_eq!(s, vec!["a", "c"], "healthy services stopped cleanly");
    }
}

#[tokio::test]
async fn the_runner_returns_ok_after_a_requested_shutdown() {
    let p = parts().await;
    let c = ctx(&p).await;
    let stopped = Arc::new(Mutex::new(vec![]));
    let services: Vec<Box<dyn Service>> =
        vec![Box::new(Probe { name: "a", outcome: Outcome::WaitForShutdown, stopped: stopped.clone() })];
    let token = CancellationToken::new();
    let t2 = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        t2.cancel();
    });
    tokio::time::timeout(Duration::from_secs(5), run_services(services, c, token)).await.unwrap().unwrap();
    assert_eq!(*stopped.lock().unwrap(), vec!["a"]);
}

#[tokio::test]
async fn the_gc_service_runs_a_pass_through_the_gc_facade_and_stops_on_shutdown() {
    let p = parts().await;
    let c = ctx(&p).await;
    let report = GcService::run_once(&c).await.unwrap();
    assert_eq!(report.errors, 0);
    let token = CancellationToken::new();
    let h = tokio::spawn(Box::new(GcService::new(Duration::from_millis(50))).run(c, token.clone()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    token.cancel();
    tokio::time::timeout(Duration::from_secs(5), h).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn the_gc_pass_collects_every_blob_store_and_a_store_that_cannot_be_opened_is_counted_not_fatal() {
    let p = parts().await;
    let c = ctx(&p).await;
    let dir = Directory::with_repo(p.seed_store.repos().directory.clone());
    let d = dir.create_domain("tenant.test").await.unwrap();
    // A store row whose settings cannot be opened (a directory name that is a path): the pass skips it, counts one error and still runs the other stores.
    p.seed_store.repos().blob_stores.create_for_domain(d, "fs", r#"{"dir":"../escape"}"#).await.unwrap();
    let report = GcService::run_once(&c).await.unwrap();
    assert_eq!(report.errors, 1, "the unopenable store is counted");
    // With a good store the pass is clean.
    let d2 = dir.create_domain("good.test").await.unwrap();
    p.seed_store.repos().blob_stores.create_for_domain(d2, "fs", r#"{"dir":"good"}"#).await.unwrap();
    let again = GcService::run_once(&c).await.unwrap();
    assert_eq!(again.errors, 1, "only the unopenable store is counted; the good one collects cleanly");
}

#[tokio::test]
async fn the_upkeep_pass_makes_the_missing_copy_of_a_pool_and_skips_a_store_that_is_not_ready() {
    use rusty_store::{FsStoreOpener, KindOpener, Mailbox, PoolOpener};
    let p = parts().await;
    let cfg = Arc::new(dev_config(p.dir.path()));
    let tls = TlsProvider::from_config(&cfg.tls, "localhost", Arc::new(rusty_svc::FileSecretSource::new())).unwrap();
    let repos: &Repos = p.store.repos();
    let backends_dir = p.dir.path().join("backends");
    let fs = Arc::new(FsStoreOpener::new(backends_dir.clone()));
    let opener = KindOpener::new().with_pool(Arc::new(PoolOpener::new(repos.blob_pool.clone(), fs)));
    let c = Context::new_with_opener(repos, p.blobs.clone(), cfg, tls, Arc::new(opener)).await.unwrap();

    let dir = Directory::with_repo(p.seed_store.repos().directory.clone());
    let d = dir.create_domain("pool.test").await.unwrap();
    let store = p.seed_store.repos().blob_stores.create_for_domain(d, "pool", r#"{"copies":2,"placement":"ordered"}"#).await.unwrap();
    for n in ["a", "b"] {
        let id = p.seed_store.repos().blob_pool.add_backend(store, n, "fs", &format!(r#"{{"dir":"{n}"}}"#), 100).await.unwrap();
        p.seed_store.repos().blob_pool.record_backend_check(id, "ok", None).await.unwrap();
    }
    // A second pool that is not ready (its backend never passed a check): skipped, nothing fails.
    let d2 = dir.create_domain("notready.test").await.unwrap();
    let unready = p.seed_store.repos().blob_stores.create_for_domain(d2, "pool", "{}").await.unwrap();
    p.seed_store.repos().blob_pool.add_backend(unready, "x", "fs", r#"{"dir":"x"}"#, 100).await.unwrap();

    let acc = dir.create_account("alice", d, None, None).await.unwrap();
    let mb = Mailbox::open_with(p.seed_store.repos().mailbox.clone(), c.stores.clone(), acc).await.unwrap();
    let m = mb.append(rusty_core::FolderId(1), bytes::Bytes::from_static(b"Subject: pool\r\nMessage-ID: <p@x>\r\n\r\nbody\r\n"), Default::default()).await.unwrap();
    let blob = mb.get_raw_ref(m.id).await.unwrap().blob;
    let on_b = backends_dir.join("b").join(&blob.as_str()[0..2]).join(&blob.as_str()[2..4]).join(blob.as_str());
    assert!(on_b.exists(), "a mirror: both copies were written");
    // The copy on `b` is lost (and its record with it): the upkeep pass makes it again.
    std::fs::remove_file(&on_b).unwrap();
    let b_id = repos.blob_pool.backends(store).await.unwrap().into_iter().find(|x| x.name == "b").unwrap().id;
    p.seed_store.repos().blob_pool.remove_location(store, &blob, b_id).await.unwrap();
    assert_eq!(rusty_svc::MaintainService::run_once(&c).await.unwrap(), 1);
    assert!(on_b.exists(), "the copy was made again");
    assert_eq!(repos.blob_pool.locations(store, &blob).await.unwrap().len(), 2);
    assert_eq!(rusty_svc::MaintainService::run_once(&c).await.unwrap(), 0, "nothing left to do");
}

#[tokio::test]
async fn the_gc_pass_removes_deleted_messages_after_their_retention_and_keeps_younger_ones() {
    use rusty_store::Mailbox;
    let p = parts().await;
    let c = ctx(&p).await;
    let dir = Directory::with_repo(p.seed_store.repos().directory.clone());
    let d = dir.create_domain("keep.test").await.unwrap();
    let acc = dir.create_account("alice", d, None, None).await.unwrap();
    let mb = Mailbox::open_with(p.seed_store.repos().mailbox.clone(), c.stores.clone(), acc).await.unwrap();
    let new = |n: u8| bytes::Bytes::from(format!("Subject: m{n}\r\nMessage-ID: <{n}@x>\r\n\r\nbody {n}\r\n").into_bytes());
    let old = mb.append(rusty_core::FolderId(1), new(1), Default::default()).await.unwrap();
    let young = mb.append(rusty_core::FolderId(1), new(2), Default::default()).await.unwrap();
    let now = c.jobs.now().await.unwrap();
    // One was deleted 20 days ago, one a day ago; the retention is 14 days.
    mb.expunge_many_at(&[old.id], now - 20 * 86_400).await.unwrap();
    mb.expunge_many_at(&[young.id], now - 86_400).await.unwrap();
    assert_eq!(mb.list_deleted(0, 10).await.unwrap().len(), 2);
    let report = GcService::run_once(&c).await.unwrap();
    assert_eq!(report.deleted_purged, 1, "{report:?}");
    let kept = mb.list_deleted(0, 10).await.unwrap();
    assert_eq!(kept.iter().map(|m| m.id).collect::<Vec<_>>(), vec![young.id], "the younger one is still recoverable");
}

#[tokio::test]
async fn a_mailbox_is_exported_in_pieces_and_the_file_imports_into_another_mailbox_with_its_folders_dates_and_flags() {
    use rusty_core::Flags;
    use rusty_store::Mailbox;
    let p = parts().await;
    let c = ctx(&p).await;
    let dir = Directory::with_repo(p.seed_store.repos().directory.clone());
    let d = dir.create_domain("move.test").await.unwrap();
    let alice = dir.create_account("alice", d, None, None).await.unwrap();
    let bob = dir.create_account("bob", d, None, None).await.unwrap();
    let mb = Mailbox::open_with(p.seed_store.repos().mailbox.clone(), c.stores.clone(), alice).await.unwrap();
    let archive = mb.create_folder(None, "Archive 2025").await.unwrap();
    let seen = Flags::from_bits(Flags::SEEN.bits() | Flags::FLAGGED.bits()).unwrap();
    mb.append_dated(rusty_core::FolderId(1), bytes::Bytes::from_static(b"Subject: one\r\nMessage-ID: <1@x>\r\n\r\nFrom the top\r\n"), seen, 1_700_000_000).await.unwrap();
    mb.append_dated(archive, bytes::Bytes::from_static(b"Subject: two\r\nMessage-ID: <2@x>\r\n\r\nbody two\r\n"), Flags::empty(), 1_600_000_000).await.unwrap();

    let repo = p.seed_store.repos().transfers.clone();
    let export = repo.create("export", alice.0, "root@home.test", None).await.unwrap();
    assert!(TransferService::run_once(&c).await.unwrap(), "the export was waiting");
    let done = repo.get(export).await.unwrap().unwrap();
    assert_eq!((done.state.as_str(), done.messages), ("done", 2), "{done:?}");
    let mut file = Vec::new();
    let mut seq = 0;
    while let Some(piece) = repo.chunk(export, seq).await.unwrap() {
        file.extend_from_slice(&piece);
        seq += 1;
    }
    assert_eq!(i64::try_from(file.len()).unwrap(), done.size);
    let text = String::from_utf8_lossy(&file);
    assert!(text.contains("X-Rusty-Folder: Archive 2025") && text.contains("Status: RO") && text.contains("X-Status: F"), "{text}");
    assert!(text.contains(">From the top"), "a body line that starts with From is quoted: {text}");
    assert!(!TransferService::run_once(&c).await.unwrap(), "nothing else waits");

    // The same file imported into bob: folders are made when missing, dates and flags are kept.
    let import = repo.create("import", bob.0, "root@home.test", Some("Fallback")).await.unwrap();
    for (i, piece) in file.chunks(40).enumerate() {
        assert!(repo.add_chunk(import, i32::try_from(i).unwrap(), piece, 1 << 20).await.unwrap());
    }
    assert!(repo.queue_import(import).await.unwrap());
    assert!(TransferService::run_once(&c).await.unwrap());
    let imported = repo.get(import).await.unwrap().unwrap();
    assert_eq!((imported.state.as_str(), imported.messages, imported.skipped), ("done", 2, 0), "{imported:?}");
    let bmb = Mailbox::open_with(p.seed_store.repos().mailbox.clone(), c.stores.clone(), bob).await.unwrap();
    let folders = bmb.list_folders().await.unwrap();
    let arch = folders.iter().find(|f| f.name == "Archive 2025").expect("the folder of the file was made");
    let inbox = folders.iter().find(|f| f.role == "inbox").unwrap();
    let in_arch = bmb.list_folder(arch.id, 0, 10).await.unwrap();
    assert_eq!((in_arch.len(), in_arch[0].subject.as_str(), in_arch[0].internal_date), (1, "two", 1_600_000_000));
    let in_inbox = bmb.list_folder(inbox.id, 0, 10).await.unwrap();
    assert_eq!((in_inbox[0].subject.as_str(), in_inbox[0].flags), ("one", seen));
    assert_eq!(&bmb.get_raw(in_inbox[0].id).await.unwrap()[..], b"Subject: one\r\nMessage-ID: <1@x>\r\n\r\nFrom the top\r\n", "the message is what it was");

    // Not a mailbox file: it fails with a fixed word and imports nothing; an unknown account fails as well.
    let junk = repo.create("import", bob.0, "root@home.test", None).await.unwrap();
    repo.add_chunk(junk, 0, b"hello, not an mbox\n", 1 << 20).await.unwrap();
    repo.queue_import(junk).await.unwrap();
    TransferService::run_once(&c).await.unwrap();
    let bad = repo.get(junk).await.unwrap().unwrap();
    assert_eq!((bad.state.as_str(), bad.error.as_deref()), ("failed", Some("unreadable")));
    // The pieces and the rows go away when they expire (the test moves the clock by asking for expiry in the future through a direct purge of nothing yet).
    assert_eq!(repo.purge_expired(100).await.unwrap(), 0, "a day has not passed");
}

#[tokio::test]
async fn a_queued_search_is_run_where_the_mail_is_and_leaves_at_most_a_hundred_results_and_no_body() {
    use rusty_core::Flags;
    use rusty_store::{Mailbox, SearchCriteria};
    let p = parts().await;
    let c = ctx(&p).await;
    let dir = Directory::with_repo(p.seed_store.repos().directory.clone());
    let d = dir.create_domain("find.test").await.unwrap();
    let alice = dir.create_account("alice", d, None, None).await.unwrap();
    let mb = Mailbox::open_with(p.seed_store.repos().mailbox.clone(), c.stores.clone(), alice).await.unwrap();
    mb.append_dated(rusty_core::FolderId(1), bytes::Bytes::from_static(b"Subject: Invoice 42\r\nFrom: Bob <bob@else.test>\r\nMessage-ID: <1@x>\r\n\r\nsecret body words\r\n"), Flags::empty(), 1_700_000_000).await.unwrap();
    mb.append_dated(rusty_core::FolderId(1), bytes::Bytes::from_static(b"Subject: Lunch\r\nFrom: Carol <carol@else.test>\r\nMessage-ID: <2@x>\r\n\r\nnothing\r\n"), Flags::empty(), 1_700_000_100).await.unwrap();

    let repo = p.seed_store.repos().searches.clone();
    let id = repo.create("root@home.test", &SearchCriteria { subject_text: Some("invoice".into()), ..Default::default() }).await.unwrap();
    assert!(rusty_svc::SearchService::run_once(&c).await.unwrap(), "the search was waiting");
    let done = repo.get(id).await.unwrap().unwrap();
    assert_eq!((done.state.as_str(), done.found, done.truncated), ("done", 1, false), "{done:?}");
    assert!(done.results.contains("alice@find.test") && done.results.contains("Invoice 42") && done.results.contains("Inbox"), "{}", done.results);
    assert!(!done.results.contains("secret body words"), "a body is never part of a result");
    assert!(!rusty_svc::SearchService::run_once(&c).await.unwrap(), "nothing else waits");

    // A search with nothing to look for is refused by the worker too, with a fixed word.
    let empty = repo.create("root@home.test", &SearchCriteria::default()).await.unwrap();
    assert!(rusty_svc::SearchService::run_once(&c).await.unwrap());
    let refused = repo.get(empty).await.unwrap().unwrap();
    assert_eq!((refused.state.as_str(), refused.error.as_deref()), ("failed", Some("no_criteria")));
}

/// A server that says what an IMAP server says, from a script: the commands it is sent are kept for the test to look at.
type Scripted = Vec<(&'static str, Vec<(u32, &'static str, &'static str, &'static [u8])>)>;

async fn fake_imap(good_password: &'static str, mail: Scripted) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        let Ok((sock, _)) = listener.accept().await else { return };
        let (r, mut w) = sock.into_split();
        let mut r = BufReader::new(r);
        w.write_all(b"* OK fake ready\r\n").await.unwrap();
        let mut open: Option<usize> = None;
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let cmd = line.trim_end().to_owned();
            log.lock().unwrap().push(cmd.clone());
            let (tag, rest) = cmd.split_once(' ').unwrap_or((&cmd, ""));
            let upper = rest.to_ascii_uppercase();
            let out: Vec<u8> = if upper.starts_with("LOGIN") {
                if rest.contains(&format!("\"{good_password}\"")) {
                    format!("{tag} OK logged in\r\n").into_bytes()
                } else {
                    format!("{tag} NO [AUTHENTICATIONFAILED] nope\r\n").into_bytes()
                }
            } else if upper.starts_with("LIST") {
                let mut s = String::new();
                for (name, _) in &mail {
                    let attr = if *name == "Sent Items" { "\\Sent" } else { "\\HasNoChildren" };
                    s.push_str(&format!("* LIST ({attr}) \"/\" \"{name}\"\r\n"));
                }
                s.push_str("* LIST (\\Noselect) \"/\" \"[Gmail]\"\r\n");
                s.push_str(&format!("{tag} OK done\r\n"));
                s.into_bytes()
            } else if upper.starts_with("EXAMINE") {
                open = mail.iter().position(|(n, _)| rest.contains(&format!("\"{n}\"")));
                format!("* {} EXISTS\r\n{tag} OK [READ-ONLY] ok\r\n", open.map_or(0, |i| mail[i].1.len())).into_bytes()
            } else if upper.starts_with("UID SEARCH") {
                let ids: Vec<String> = open.map(|i| mail[i].1.iter().map(|m| m.0.to_string()).collect()).unwrap_or_default();
                format!("* SEARCH {}\r\n{tag} OK done\r\n", ids.join(" ")).into_bytes()
            } else if upper.starts_with("UID FETCH") {
                let uid: u32 = rest.split_whitespace().nth(2).and_then(|n| n.parse().ok()).unwrap_or(0);
                match open.and_then(|i| mail[i].1.iter().find(|m| m.0 == uid)) {
                    Some((uid, flags, date, body)) => {
                        let mut b = format!("* 1 FETCH (UID {uid} FLAGS ({flags}) INTERNALDATE \"{date}\" BODY[] {{{}}}\r\n", body.len()).into_bytes();
                        b.extend_from_slice(body);
                        b.extend_from_slice(format!(")\r\n{tag} OK done\r\n").as_bytes());
                        b
                    }
                    None => format!("{tag} NO no such message\r\n").into_bytes(),
                }
            } else if upper.starts_with("LOGOUT") {
                w.write_all(format!("* BYE bye\r\n{tag} OK done\r\n").as_bytes()).await.unwrap();
                return;
            } else {
                format!("{tag} BAD what\r\n").into_bytes()
            };
            w.write_all(&out).await.unwrap();
        }
    });
    (port, seen)
}

#[tokio::test]
async fn a_mailbox_is_pulled_from_another_imap_server_with_its_folders_dates_and_flags_and_nothing_there_is_changed() {
    use rusty_store::{Mailbox, NewMigration};
    let p = parts().await;
    let (unseal_key, seal_key) = rusty_seal::generate();
    let c = ctx(&p).await.with_unsealer(Some(std::sync::Arc::new(rusty_seal::Unsealer::new(vec![unseal_key]).unwrap())));
    let dir = Directory::with_repo(p.seed_store.repos().directory.clone());
    let d = dir.create_domain("pull.test").await.unwrap();
    let carol = dir.create_account("carol", d, None, None).await.unwrap();

    let (port, seen) = fake_imap(
        "hunter2",
        vec![
            ("INBOX", vec![(1, "\\Seen", "17-Jul-1996 02:44:25 -0700", b"Subject: one\r\nMessage-ID: <1@x>\r\n\r\nfirst\r\n"), (2, "\\Deleted", "18-Jul-1996 02:44:25 +0000", b"Subject: gone\r\n\r\nx\r\n")]),
            ("Sent Items", vec![(7, "\\Seen \\Answered", "01-Jan-2020 10:00:00 +0000", b"Subject: sent one\r\nMessage-ID: <7@x>\r\n\r\nsent\r\n")]),
            ("Archive/Old", vec![(3, "", "29-Feb-2024 12:00:00 +0100", b"Subject: old\r\nMessage-ID: <3@x>\r\n\r\nold\r\n")]),
        ],
    )
    .await;

    let repo = p.seed_store.repos().migrations.clone();
    let id = repo.create(&NewMigration { account: carol.0, requested_by: "root@home.test".into(), host: "127.0.0.1".into(), port: i32::from(port), security: "plain".into(), remote_user: "carol@old.test".into() }).await.unwrap();
    let sealed = seal_key.seal(&rusty_seal::Context::new("migration", id, "password").unwrap(), b"hunter2").unwrap();
    p.seed_store.repos().sealed.put("migration", id, "password", &seal_key.id().to_hex(), &sealed, None).await.unwrap();
    assert!(repo.queue(id).await.unwrap());

    let worker = rusty_svc::MigrationService::new();
    assert!(worker.run_once(&c).await.unwrap(), "the migration was waiting");
    let done = repo.get(id).await.unwrap().unwrap();
    assert_eq!((done.state.as_str(), done.messages, done.skipped, done.error.clone()), ("done", 3, 1, None), "{done:?}");
    assert_eq!((done.folders_total, done.folders_done), (3, 3));

    let mb = Mailbox::open_with(p.seed_store.repos().mailbox.clone(), c.stores.clone(), carol).await.unwrap();
    let folders = mb.list_folders().await.unwrap();
    let in_folder = |name: &str| folders.iter().find(|f| f.name == name || f.role == name).map(|f| f.id).unwrap_or_else(|| panic!("no folder {name}: {folders:?}"));
    let inbox = mb.list_folder(in_folder("inbox"), 0, 50).await.unwrap();
    assert_eq!(inbox.len(), 1, "the message marked deleted was not copied");
    assert_eq!((inbox[0].flags, inbox[0].internal_date), (rusty_core::Flags::SEEN, 837_596_665));
    let sent = mb.list_folder(in_folder("sent"), 0, 50).await.unwrap();
    assert_eq!(sent.len(), 1, "the Sent Items folder went to the Sent folder");
    assert_eq!(sent[0].flags, rusty_core::Flags::SEEN | rusty_core::Flags::ANSWERED);
    let old = mb.list_folder(in_folder("Archive.Old"), 0, 50).await.unwrap();
    assert_eq!((old.len(), old[0].internal_date), (1, 1_709_204_400), "a name with a slash is flattened, the date is kept");

    // Nothing was changed over there: it was only read.
    let commands = seen.lock().unwrap().join("\n");
    assert!(commands.contains("BODY.PEEK[]") && commands.contains("EXAMINE"), "{commands}");
    assert!(!commands.contains("STORE") && !commands.contains("SELECT") && !commands.contains("EXPUNGE") && !commands.contains("COPY"), "{commands}");
    assert!(!worker.run_once(&c).await.unwrap(), "nothing else waits");
}

#[tokio::test]
async fn a_refused_login_ends_the_migration_with_a_fixed_word_and_copies_nothing() {
    use rusty_store::NewMigration;
    let p = parts().await;
    let (unseal_key, seal_key) = rusty_seal::generate();
    let c = ctx(&p).await.with_unsealer(Some(std::sync::Arc::new(rusty_seal::Unsealer::new(vec![unseal_key]).unwrap())));
    let dir = Directory::with_repo(p.seed_store.repos().directory.clone());
    let d = dir.create_domain("pull2.test").await.unwrap();
    let dave = dir.create_account("dave", d, None, None).await.unwrap();
    let (port, _) = fake_imap("right-one", vec![("INBOX", vec![(1, "", "01-Jan-2020 10:00:00 +0000", b"Subject: x\r\n\r\nx\r\n")])]).await;
    let repo = p.seed_store.repos().migrations.clone();
    let id = repo.create(&NewMigration { account: dave.0, requested_by: "root@home.test".into(), host: "127.0.0.1".into(), port: i32::from(port), security: "plain".into(), remote_user: "dave".into() }).await.unwrap();
    let sealed = seal_key.seal(&rusty_seal::Context::new("migration", id, "password").unwrap(), b"wrong-one").unwrap();
    p.seed_store.repos().sealed.put("migration", id, "password", &seal_key.id().to_hex(), &sealed, None).await.unwrap();
    assert!(repo.queue(id).await.unwrap());
    assert!(rusty_svc::MigrationService::new().run_once(&c).await.unwrap());
    let done = repo.get(id).await.unwrap().unwrap();
    assert_eq!((done.state.as_str(), done.error.as_deref(), done.messages), ("failed", Some("auth"), 0), "{done:?}");

    // One that is finished cannot be cancelled; one still queued is cancelled at once.
    assert!(!repo.request_cancel(id).await.unwrap());
    let queued = repo.create(&NewMigration { account: dave.0, requested_by: "root@home.test".into(), host: "127.0.0.1".into(), port: 1, security: "plain".into(), remote_user: "dave".into() }).await.unwrap();
    assert!(repo.request_cancel(queued).await.unwrap());
    assert_eq!(repo.get(queued).await.unwrap().unwrap().state, "cancelled");
}
