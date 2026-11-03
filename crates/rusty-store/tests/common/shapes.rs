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
    let m = rusty_core::MessageId(ids[1]);
    shape!("add_keyword", true, r.add_keyword(id, m, "kw"));
    shape!("remove_keyword", true, r.remove_keyword(id, m, "kw"));
    shape!("move_message", true, r.move_message(id, m, f));
    shape!("move_many (10)", true, r.move_many(id, &ids[100..110], f));
    shape!("move_many (100)", true, r.move_many(id, &ids[110..210], f));
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
    out
}

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
    let ok = if backend == "postgres" {
        first.contains("FROM mailboxes WHERE id = $1 FOR UPDATE")
    } else {
        first.contains("SELECT id FROM mailboxes WHERE id = ?1")
    };
    assert!(ok, "{backend} {}: first statement after BEGIN is not the mailbox lock: {first:?}\nall: {:#?}", s.label, s.stmts);
}

/// `r.create_folder(id, ..)` -> `create_folder`.
fn method_of(call: &str) -> String {
    let rest = call.trim_start().strip_prefix("r.").unwrap_or_else(|| panic!("shape call must start with `r.`: {call}"));
    rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect()
}

const TRAIT_SOURCE: &str = include_str!("../../src/repo/mailbox.rs");

/// `(name, is_write)` of every method of `MailboxRepo`, read from the trait declaration itself
/// (a method is a write when it returns `Committed<..>`).
pub fn trait_methods() -> Vec<(String, bool)> {
    let start = TRAIT_SOURCE
        .find("pub trait MailboxRepo")
        .expect("MailboxRepo declaration");
    let body: String = TRAIT_SOURCE[start..]
        .lines()
        .skip(1)
        .take_while(|l| *l != "}")
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let methods: Vec<(String, bool)> = body
        .split(';')
        .filter_map(|decl| {
            let at = decl.find("fn ")?;
            let name: String = decl[at + 3..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            Some((name, decl.contains("Committed<")))
        })
        .collect();
    assert!(
        methods.len() >= 24,
        "the MailboxRepo parser found only {} methods: {methods:?}",
        methods.len()
    );
    methods
}

/// Review PG4 M6: a new `MailboxRepo` method that is not added to `collect` would escape the
/// lock-first probe and the statement budget. Every trait method needs a shape, and a method that
/// returns `Committed` (a write) must be recorded as one.
pub fn assert_every_trait_method_has_a_shape(all: &[Shape]) {
    let methods = trait_methods();
    for (name, write) in &methods {
        let mine: Vec<&Shape> = all.iter().filter(|s| s.method == *name).collect();
        assert!(
            !mine.is_empty(),
            "MailboxRepo::{name} has no entry in tests/common/shapes.rs: add a `shape!` for it \
             (and a BUDGET line in query_shape_pg.rs) so the lock-first probe covers it"
        );
        if *write {
            assert!(
                mine.iter().all(|s| s.write),
                "MailboxRepo::{name} returns Committed but its shape is not marked as a write"
            );
        }
    }
    for s in all {
        assert!(
            methods.iter().any(|(m, _)| *m == s.method),
            "shape {:?} measures `{}` which is not a MailboxRepo method",
            s.label,
            s.method
        );
    }
}
