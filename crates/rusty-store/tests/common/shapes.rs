//! Statement shapes of every `MailboxRepo` operation, recorded with the sqlx statement probe.
//! Shared by the Postgres statement-count oracle (`query_shape_pg.rs`) and the lock-first probe
//! on both backends.
#![allow(dead_code)]

use rusty_core::{DeliveryId, Flags, FolderId};
use rusty_store::repo::{DeliverEntry, Delivery, NewMessage};

use super::backends::{acct, Backend, INBOX};
use super::{raw, stmts};

pub struct Shape {
    pub label: &'static str,
    /// The `MailboxRepo` method this shape measured (taken from the call expression).
    pub method: String,
    /// A write: its first data statement after BEGIN must be the mailbox lock.
    pub write: bool,
    pub stmts: Vec<String>,
    /// The repository trait the method belongs to (`MailboxRepo`, `JobRepo`, `GcRepo`).
    pub repo: &'static str,
    /// First data statement a non-mailbox write must start with: (Postgres, SQLite) substrings.
    pub first: Option<(&'static str, &'static str)>,
    /// A statement that must appear somewhere in a Postgres run (the delegated mailbox lock).
    pub must_include_pg: Option<&'static str>,
}

async fn new_msg(b: &Backend, folder: FolderId, subject: &str) -> NewMessage {
    let bytes = raw(subject, None);
    let hash = b.blobs.put(bytes.clone()).await.unwrap();
    NewMessage {
        folder,
        hash,
        size: bytes.len() as i64,
        flags: Flags::default(),
        env: rusty_mime::parse_envelope(&bytes).unwrap(),
        internal_date: None,
    }
}

async fn new_msgs(b: &Backend, folder: FolderId, tag: &str, n: usize) -> Vec<NewMessage> {
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        v.push(new_msg(b, folder, &format!("{tag}-{i}")).await);
    }
    v
}

fn entries(msgs: Vec<NewMessage>, base: u8) -> Vec<DeliverEntry> {
    msgs.into_iter()
        .enumerate()
        .map(|(i, msg)| DeliverEntry {
            msg,
            delivery_id: DeliveryId([base.wrapping_add(i as u8); 32]),
        })
        .collect()
}

/// Run every operation once against a fresh mailbox and return the statements each executed
/// (BEGIN and COMMIT included). Fixtures are built before the probe is reset for an operation.
pub async fn collect(b: &Backend, p: &stmts::Probe) -> Vec<Shape> {
    let (_, acc, mb) = acct(b, "shape").await;
    let id = mb.id();
    let r = &b.mrepo;
    let mut out: Vec<Shape> = vec![];
    macro_rules! shape {
        ($label:expr, $write:expr, $call:expr) => {{
            let fut = $call;
            p.reset();
            fut.await.unwrap();
            out.push(Shape {
                label: $label,
                method: method_of(stringify!($call)),
                write: $write,
                stmts: p.statements(),
                repo: "MailboxRepo",
                first: None,
                must_include_pg: None,
            });
        }};
    }
    // Fixtures (not measured): folders and 400 messages in the inbox.
    let f = mb.create_folder(None, "F").await.unwrap();
    let g = mb.create_folder(None, "G").await.unwrap();
    let mut ids = vec![];
    for k in 0..4 {
        let batch: Vec<_> = (0..100).map(|i| raw(&format!("fx{k}-{i}"), None)).collect();
        for r in mb.append_many(INBOX, batch, Flags::default()).await.unwrap() {
            ids.push(r.id.0);
        }
    }

    shape!("mailbox_for_account", false, r.mailbox_for_account(acc));
    shape!("folder_exists", false, r.folder_exists(id, INBOX));
    shape!("create_folder", true, r.create_folder(id, Some(g), "Child"));
    shape!("rename_folder", true, r.rename_folder(id, f, "F2"));
    shape!("move_folder", true, r.move_folder(id, f, Some(g)));
    let empty = mb.create_folder(None, "Empty").await.unwrap();
    shape!("delete_folder (empty)", true, r.delete_folder(id, empty));
    let doomed = mb.create_folder(None, "Doomed").await.unwrap();
    let mut dids = vec![];
    for i in 0..3 {
        dids.push(mb.append(doomed, raw(&format!("doom{i}"), None), Flags::default()).await.unwrap().id);
    }
    mb.expunge_many(&dids).await.unwrap();
    shape!("delete_folder (3 expunged messages)", true, r.delete_folder(id, doomed));

    let one = new_msg(b, INBOX, "one").await;
    shape!("append", true, r.append(id, &one, None, None));
    let d = Delivery { id: DeliveryId([200; 32]), created_at: None };
    let dm = new_msg(b, INBOX, "delivered").await;
    shape!("append (delivery, new)", true, r.append(id, &dm, Some(&d), None));
    shape!("append (delivery, duplicate)", true, r.append(id, &dm, Some(&d), None));
    let m1 = new_msgs(b, INBOX, "am1", 1).await;
    shape!("append_many (1)", true, r.append_many(id, &m1));
    let m10 = new_msgs(b, INBOX, "am10", 10).await;
    shape!("append_many (10)", true, r.append_many(id, &m10));
    let m100 = new_msgs(b, INBOX, "am100", 100).await;
    shape!("append_many (100)", true, r.append_many(id, &m100));
    let e10 = entries(new_msgs(b, INBOX, "dm10", 10).await, 10);
    shape!("deliver_many (10, new)", true, r.deliver_many(id, &e10, None, None));
    let e100 = entries(new_msgs(b, INBOX, "dm100", 100).await, 100);
    shape!("deliver_many (100, new)", true, r.deliver_many(id, &e100, None, None));
    shape!("deliver_many (10, all duplicates)", true, r.deliver_many(id, &e10, None, None));

    shape!("set_flags", true, r.set_flags(id, rusty_core::MessageId(ids[0]), Flags::SEEN));
    shape!("set_flags_many (10)", true, r.set_flags_many(id, &ids[0..10], Flags::SEEN));
    shape!("set_flags_many (100)", true, r.set_flags_many(id, &ids[0..100], Flags::SEEN));
    shape!("update_flags_many (10)", true, r.update_flags_many(id, &ids[0..10], Flags::FLAGGED, Flags::empty()));
    shape!("update_flags_many (100)", true, r.update_flags_many(id, &ids[0..100], Flags::FLAGGED, Flags::SEEN));
    shape!("expunge_deleted_many", true, r.expunge_deleted_many(id, &ids[390..400]));
    let m = rusty_core::MessageId(ids[1]);
    shape!("add_keyword", true, r.add_keyword(id, m, "kw"));
    shape!("remove_keyword", true, r.remove_keyword(id, m, "kw"));
    shape!("move_message", true, r.move_message(id, m, f));
    shape!("move_many (10)", true, r.move_many(id, &ids[100..110], f));
    shape!("move_many (100)", true, r.move_many(id, &ids[110..210], f));
    shape!("copy_many (10)", true, r.copy_many(id, &ids[320..330], f));
    shape!("copy_many (100)", true, r.copy_many(id, &ids[300..400], f));
    shape!("set_subscribed", true, r.set_subscribed(id, f, false));
    shape!("expunge", true, r.expunge(id, rusty_core::MessageId(ids[2])));
    shape!("expunge_many (10)", true, r.expunge_many(id, &ids[210..220], Some(100)));
    shape!("purge_deleted (11 rows)", true, r.purge_deleted(id, i64::MAX, 1000));
    shape!("expunge_many (100)", true, r.expunge_many(id, &ids[220..320], Some(100)));
    shape!("purge_deleted (100 rows)", true, r.purge_deleted(id, i64::MAX, 1000));
    shape!("purge_deleted (nothing)", true, r.purge_deleted(id, i64::MAX, 1000));

    shape!("list_folder", false, r.list_folder(id, INBOX, 0, 50));
    shape!("changes_since", false, r.changes_since(id, 0, 50));
    shape!("list_folders", false, r.list_folders(id));
    shape!("get_message", false, r.get_message(id, rusty_core::MessageId(ids[3])));
    shape!("get_raw_ref", false, r.get_raw_ref(id, rusty_core::MessageId(ids[3])));
    shape!("keywords", false, r.keywords(id, rusty_core::MessageId(ids[3])));
    shape!("keywords_many", false, r.keywords_many(id, &ids[0..10]));
    shape!("latest_change_seq", false, r.latest_change_seq(id));
    shape!("folder_counts", false, r.folder_counts(id, INBOX));

    // ---- JobRepo and GcRepo (PG5): not mailbox scoped, each write names its own first statement.
    let j = &b.jrepo;
    let g = &b.grepo;
    macro_rules! other {
        ($repo:expr, $label:expr, $write:expr, $first:expr, $include:expr, $call:expr) => {{
            let fut = $call;
            p.reset();
            fut.await.unwrap();
            out.push(Shape {
                label: $label,
                method: method_of(stringify!($call)),
                write: $write,
                stmts: p.statements(),
                repo: $repo,
                first: $first,
                must_include_pg: $include,
            });
        }};
    }
    let dom = b.dir.create_domain("jobshape.test").await.unwrap();
    let jq = b.jobs();
    let nj = |kind: &'static str, domain| rusty_store::repo::NewJob {
        kind,
        domain,
        payload: "{}",
        run_after: None,
        max_attempts: 5,
        cap: 10_000,
        now: None,
    };
    let (nj_dom, nj_none) = (nj("shape", Some(dom)), nj("shape", None));
    other!("JobRepo", "db_now (jobs)", false, None, None, j.db_now());
    other!("JobRepo", "enqueue (domain)", true,
        Some(("FROM domains WHERE id = $1 FOR NO KEY UPDATE", "unixepoch()")), None,
        j.enqueue(&nj_dom));
    other!("JobRepo", "enqueue (no domain)", true,
        Some(("pg_advisory_xact_lock", "unixepoch()")), None,
        j.enqueue(&nj_none));
    let batch = [rusty_store::repo::BatchJob { kind: "shapeb", domain: Some(dom), payload: "{}", dedupe_key: None, units: 1, bytes: 1, max_attempts: 5 }];
    let breq = rusty_store::repo::BatchRequest { jobs: &batch, owner: None, domain_cap: 10_000, no_domain_cap: 10_000, now: None };
    other!("JobRepo", "enqueue_batch (domain)", true,
        Some(("FROM domains WHERE id = $1 FOR NO KEY UPDATE", "unixepoch()")), None,
        j.enqueue_batch(&breq));
    let kinds = ["shape"];
    let creq = |worker| rusty_store::repo::ClaimRequest { worker, kinds: &kinds, lease_secs: 30, now: None };
    let (c1, c2, c3, c4) = (creq("w"), creq("w2"), creq("w3"), creq("w4"));
    let claimed = j.claim(&c1).await.unwrap().unwrap();
    // `claim` of a fresh job is measured again below; this one is the fixture for the rest.
    other!("JobRepo", "claim (job available)", true,
        Some(("FROM jobs WHERE state = 'queued'", "unixepoch()")), Some("FOR UPDATE OF j SKIP LOCKED"),
        j.claim(&c2));
    other!("JobRepo", "claim (nothing)", true,
        Some(("FROM jobs WHERE state = 'queued'", "unixepoch()")), None,
        j.claim(&c3));
    other!("JobRepo", "heartbeat", true, Some(("UPDATE jobs SET", "UPDATE jobs SET")), None,
        j.heartbeat(claimed.id, claimed.epoch, 30, None));
    other!("JobRepo", "fence_check", false, None, None,
        j.fence_check(rusty_store::JobFence { id: claimed.id, epoch: claimed.epoch }, None));
    other!("JobRepo", "fail", true, Some(("UPDATE jobs SET", "UPDATE jobs SET")), None,
        j.fail(claimed.id, claimed.epoch, "x", 0, None));
    let again = j.claim(&c4).await.unwrap().unwrap();
    other!("JobRepo", "reschedule", true, Some(("UPDATE jobs SET", "UPDATE jobs SET")), None,
        j.reschedule(again.id, again.epoch, "{}", "x", 0, None));
    let c5 = creq("w5");
    let again = j.claim(&c5).await.unwrap().unwrap();
    other!("JobRepo", "complete", true, Some(("UPDATE jobs SET", "UPDATE jobs SET")), None,
        j.complete(again.id, again.epoch, None));
    other!("JobRepo", "purge_finished_batch", true,
        Some(("DELETE FROM jobs", "DELETE FROM jobs")), None,
        j.purge_finished_batch(i64::MAX, 1000));
    let _ = jq;

    // GC: an unreferenced blob row past any cutoff, plus a message that can be purged.
    let hexes = ["aa".repeat(32), "bb".repeat(32)];
    for h in &hexes {
        insert_blob_row(b, h, 1).await;
        set_zero_since(b, h, 0).await;
    }
    let hs: Vec<_> = hexes.iter().map(|h| rusty_core::BlobHash::parse(h).unwrap()).collect();
    other!("GcRepo", "db_now (gc)", false, None, None, g.db_now());
    other!("GcRepo", "blob_candidates", false, None, None, g.blob_candidates(100, None, 1000));
    other!("GcRepo", "blob_rows_in_shard", false, None, None, g.blob_rows_in_shard(0xaa));
    other!("GcRepo", "blob_row_exists", false, None, None, g.blob_row_exists(&hs[0]));
    other!("GcRepo", "delete_blob_rows (2 rows)", true,
        Some(("FROM blobs WHERE hash = ANY($1) ORDER BY hash FOR NO KEY UPDATE", "DELETE FROM blobs WHERE hash = ?")),
        None, g.delete_blob_rows(&hs, 100));
    let gone = mb.append(INBOX, raw("gc-purge", None), Flags::default()).await.unwrap();
    mb.expunge_many_at(&[gone.id], 1).await.unwrap();
    other!("GcRepo", "purge_deleted_batch (1 message)", true,
        Some(("SELECT DISTINCT mailbox_id FROM messages", "DELETE FROM messages")),
        Some("FROM mailboxes WHERE id = $1 FOR UPDATE"), g.purge_deleted_batch(100, 1000));
    other!("GcRepo", "purge_deliveries_batch", true,
        Some(("DELETE FROM deliveries", "DELETE FROM deliveries")), None,
        g.purge_deliveries_batch(i64::MAX, 1000));
    other!("GcRepo", "purge_capabilities_batch", true,
        Some(("DELETE FROM capabilities", "DELETE FROM capabilities")), None,
        g.purge_capabilities_batch(i64::MAX, 1000));
    out
}

use super::backends::{insert_blob_row, set_zero_since};

/// The first DATA statement of the transaction (after BEGIN) must be the mailbox lock. Transaction-local
/// configuration (`set_config('lock_timeout', .., true)`) is allowed before it: it takes no lock
/// and reads no data. Panics with the statement list otherwise.
pub fn assert_lock_first(backend: &str, s: &Shape) {
    if !s.write {
        return;
    }
    // sqlx does not log BEGIN as a query event on either backend, so the recorded statements
    // start right after it (a logged `BEGIN IMMEDIATE` is skipped too).
    let first = s
        .stmts
        .iter()
        .find(|t| {
            !t.contains("set_config('lock_timeout'")
                && !t.trim_start().to_ascii_uppercase().starts_with("BEGIN")
        })
        .unwrap_or_else(|| panic!("{backend} {}: no statements: {:?}", s.label, s.stmts));
    if let Some((pg, lite)) = s.first {
        // Not mailbox scoped: the method documents what its first data statement is.
        let want = if backend == "postgres" { pg } else { lite };
        assert!(first.contains(want), "{backend} {}: first statement is not {want:?}: {first:?}
all: {:#?}", s.label, s.stmts);
        if let (true, Some(inc)) = (backend == "postgres", s.must_include_pg) {
            assert!(s.stmts.iter().any(|t| t.contains(inc)), "{}: no {inc:?} in {:#?}", s.label, s.stmts);
        }
        return;
    }
    let ok = if backend == "postgres" {
        first.contains("FROM mailboxes WHERE id = $1 FOR UPDATE")
    } else {
        first.contains("SELECT id FROM mailboxes WHERE id = ?1")
    };
    assert!(ok, "{backend} {}: first statement after BEGIN is not the mailbox lock: {first:?}\nall: {:#?}", s.label, s.stmts);
}

/// `r.create_folder(id, ..)` -> `create_folder`.
fn method_of(call: &str) -> String {
    let t = call.trim_start();
    let rest = ["r.", "j.", "g."].iter().find_map(|p| t.strip_prefix(p)).unwrap_or_else(|| panic!("shape call must start with r., j. or g.: {call}"));
    rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect()
}

/// The repository traits whose every method needs a shape: (trait name, source, read-only
/// methods). In `MailboxRepo` a method is a write when it returns `Committed<..>`; in the other
/// traits every method NOT listed as a read is a write (a new method defaults to "write", so it
/// cannot escape the lock-first probe by being forgotten).
const TRAITS: &[(&str, &str, &[&str])] = &[
    ("MailboxRepo", include_str!("../../src/repo/mailbox.rs"), &[]),
    ("JobRepo", include_str!("../../src/repo/jobs.rs"), &["db_now", "fence_check"]),
    (
        "GcRepo",
        include_str!("../../src/repo/gc.rs"),
        &["db_now", "blob_candidates", "blob_rows_in_shard", "blob_row_exists"],
    ),
];

/// `(name, is_write)` of every method of the trait, read from the trait declaration itself.
pub fn trait_methods(trait_name: &str) -> Vec<(String, bool)> {
    let (_, source, reads) = TRAITS.iter().find(|t| t.0 == trait_name).expect("known trait");
    let decl = format!("pub trait {trait_name}");
    let start = source.find(&decl).unwrap_or_else(|| panic!("{trait_name} declaration"));
    let body: String = source[start..]
        .lines()
        .skip(1)
        .take_while(|l| *l != "}")
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let methods: Vec<(String, bool)> = body
        .split(';')
        .filter_map(|d| {
            let at = d.find("fn ")?;
            let name: String = d[at + 3..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let write = if trait_name == "MailboxRepo" {
                d.contains("Committed<")
            } else {
                !reads.contains(&name.as_str())
            };
            Some((name, write))
        })
        .collect();
    let min = if trait_name == "MailboxRepo" { 24 } else { 5 };
    assert!(methods.len() >= min, "the {trait_name} parser found only {} methods: {methods:?}", methods.len());
    methods
}

/// Review PG4 M6: a new repository method that is not added to `collect` would escape the
/// lock-first probe and the statement budget. Every trait method of every repository needs a
/// shape, and a write must be recorded as one.
pub fn assert_every_trait_method_has_a_shape(all: &[Shape]) {
    for (trait_name, _, _) in TRAITS {
        let methods = trait_methods(trait_name);
        for (name, write) in &methods {
            let mine: Vec<&Shape> =
                all.iter().filter(|s| s.repo == *trait_name && s.method == *name).collect();
            assert!(
                !mine.is_empty(),
                "{trait_name}::{name} has no entry in tests/common/shapes.rs: add a shape for it \
                 (and a BUDGET line in query_shape_pg.rs) so the lock-first probe covers it"
            );
            if *write {
                assert!(
                    mine.iter().all(|s| s.write),
                    "{trait_name}::{name} is a write but its shape is not marked as a write"
                );
            }
        }
        for s in all.iter().filter(|s| s.repo == *trait_name) {
            assert!(
                methods.iter().any(|(m, _)| *m == s.method),
                "shape {:?} measures `{}` which is not a {trait_name} method",
                s.label,
                s.method
            );
        }
    }
    // Writes outside the mailbox lock rule must say what they start with.
    for s in all.iter().filter(|s| s.repo != "MailboxRepo" && s.write) {
        assert!(s.first.is_some(), "{}: a non-mailbox write needs its first-statement rule", s.label);
    }
}
