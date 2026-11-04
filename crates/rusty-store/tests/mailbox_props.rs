mod common;

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use common::*;
use proptest::prelude::*;
use rusty_core::{Flags, FolderId, MessageId};
use rusty_store::{Mailbox, MAX_PAGE};

#[derive(Debug, Clone)]
enum Op {
    Append(usize),
    Move(usize, usize),
    Expunge(usize),
    SetFlags(usize, u8),
    AddKeyword(usize, u8),
    CreateFolder,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0usize..8).prop_map(Op::Append),
        2 => (0usize..64, 0usize..8).prop_map(|(m, f)| Op::Move(m, f)),
        1 => (0usize..64).prop_map(Op::Expunge),
        2 => (0usize..64, 0u8..32).prop_map(|(m, b)| Op::SetFlags(m, b)),
        1 => (0usize..64, 0u8..4).prop_map(|(m, k)| Op::AddKeyword(m, k)),
        1 => Just(Op::CreateFolder),
    ]
}

/// Run `ops`, checking after every step that message ids, per-folder uids and change
/// sequence numbers handed out are gapless and strictly increasing.
async fn run(mb: &Mailbox, ops: Vec<Op>) {
    let mut folders: Vec<FolderId> = (1..=5).map(FolderId).collect();
    let mut live: Vec<MessageId> = vec![];
    let mut msg_folder: BTreeMap<i64, FolderId> = BTreeMap::new();
    let mut next_id = 1i64;
    let mut next_uid: BTreeMap<FolderId, u32> = folders.iter().map(|f| (*f, 1)).collect();
    let mut expected_changes = 0usize;
    let mut kw_seen: HashSet<(i64, u8)> = HashSet::new();

    for (step, o) in ops.into_iter().enumerate() {
        match o {
            Op::Append(fi) => {
                let f = folders[fi % folders.len()];
                let r = mb
                    .append(f, raw(&format!("s{step}"), None), Flags::empty())
                    .await
                    .unwrap();
                assert_eq!(r.id, MessageId(next_id));
                let u = next_uid.get_mut(&f).unwrap();
                assert_eq!(r.uid, *u);
                *u += 1;
                next_id += 1;
                live.push(r.id);
                msg_folder.insert(r.id.0, f);
                expected_changes += 1;
            }
            Op::Move(mi, fi) if !live.is_empty() => {
                let id = live[mi % live.len()];
                let to = folders[fi % folders.len()];
                let r = mb.move_message(id, to).await.unwrap();
                assert_eq!(r.id, id);
                if msg_folder[&id.0] != to {
                    let u = next_uid.get_mut(&to).unwrap();
                    assert_eq!(r.uid, *u);
                    *u += 1;
                    msg_folder.insert(id.0, to);
                    expected_changes += 1;
                }
            }
            Op::Expunge(mi) if !live.is_empty() => {
                let id = live.remove(mi % live.len());
                mb.expunge(id).await.unwrap();
                expected_changes += 1;
            }
            Op::SetFlags(mi, bits) if !live.is_empty() => {
                let id = live[mi % live.len()];
                mb.set_flags(id, Flags::from_bits(bits).unwrap())
                    .await
                    .unwrap();
                expected_changes += 1;
            }
            Op::AddKeyword(mi, k) if !live.is_empty() => {
                let id = live[mi % live.len()];
                mb.add_keyword(id, &format!("k{k}")).await.unwrap();
                if kw_seen.insert((id.0, k)) {
                    expected_changes += 1;
                }
            }
            Op::CreateFolder => {
                let f = mb.create_folder(None, &format!("f{step}")).await.unwrap();
                assert_eq!(f, FolderId(folders.len() as i64 + 1));
                folders.push(f);
                next_uid.insert(f, 1);
                expected_changes += 1;
            }
            _ => {}
        }
        let changes = mb.changes_since(0, MAX_PAGE).await.unwrap().changes;
        assert_eq!(changes.len(), expected_changes);
        for (i, c) in changes.iter().enumerate() {
            assert_eq!(c.seq, i as i64 + 1, "seq gapless and monotonic");
        }
    }
    // Each folder's live uids are strictly increasing in list order and unique.
    for f in &folders {
        let list = mb.list_folder(*f, 0, MAX_PAGE).await.unwrap();
        assert!(list.windows(2).all(|w| w[0].uid < w[1].uid));
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    #[test]
    fn ids_uids_seq_are_gapless_and_monotonic(ops in proptest::collection::vec(op(), 1..40)) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(for_each_env(|e| {
            let ops = ops.clone();
            async move { run(&e.mb, ops).await }
        }));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_appends_no_duplicates() {
    for_each_env(|e| async move {
        let mb: Arc<Mailbox> = Arc::new(e.mb.clone());
        let mut tasks = vec![];
        for i in 0..50 {
            let mb = mb.clone();
            tasks.push(tokio::spawn(async move {
                mb.append(
                    INBOX,
                    raw(&format!("c{i}"), Some(&format!("c{i}@x"))),
                    Flags::empty(),
                )
                .await
                .unwrap()
            }));
        }
        let mut refs = vec![];
        for t in tasks {
            refs.push(t.await.unwrap());
        }
        let mut uids: Vec<u32> = refs.iter().map(|r| r.uid).collect();
        uids.sort_unstable();
        assert_eq!(uids, (1..=50).collect::<Vec<u32>>());
        let mut ids: Vec<i64> = refs.iter().map(|r| r.id.0).collect();
        ids.sort_unstable();
        assert_eq!(ids, (1..=50).collect::<Vec<i64>>());

        let seqs: Vec<i64> = mb
            .changes_since(0, MAX_PAGE)
            .await
            .unwrap()
            .changes
            .iter()
            .map(|c| c.seq)
            .collect();
        assert_eq!(seqs, (1..=50).collect::<Vec<i64>>());
        assert_eq!(mb.list_folder(INBOX, 0, MAX_PAGE).await.unwrap().len(), 50);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_appends_into_different_folders() {
    for_each_env(|e| async move {
        let mb = Arc::new(e.mb.clone());
        let (a, b) = tokio::join!(
            {
                let mb = mb.clone();
                async move {
                    mb.append(INBOX, raw("a", None), Flags::empty())
                        .await
                        .unwrap()
                }
            },
            {
                let mb = mb.clone();
                async move {
                    mb.append(SENT, raw("b", None), Flags::empty())
                        .await
                        .unwrap()
                }
            }
        );
        assert_ne!(a.id, b.id);
        assert_eq!((a.uid, b.uid), (1, 1));
        let mut ids = vec![a.id.0, b.id.0];
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2]);
        let seqs: Vec<i64> = mb
            .changes_since(0, MAX_PAGE)
            .await
            .unwrap()
            .changes
            .iter()
            .map(|c| c.seq)
            .collect();
        assert_eq!(seqs, vec![1, 2]);
    })
    .await;
}

// ====================================================================== PG4: both backends

/// Case count for the Postgres runs (each case creates and drops a database): `RUSTY_PG_PROP_CASES`
/// (default 64). The SQLite case count above is unchanged.
fn pg_cases() -> u32 {
    std::env::var("RUSTY_PG_PROP_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64)
}

#[cfg(feature = "postgres")]
mod pg {
    use super::*;
    use common::backends::{acct, pg_backend};

    proptest! {
        #![proptest_config(ProptestConfig { cases: pg_cases(), ..ProptestConfig::default() })]

        /// The same gapless/monotonic model as the SQLite property, on Postgres (20 ops).
        #[test]
        fn pg_ids_uids_seq_are_gapless_and_monotonic(ops in proptest::collection::vec(op(), 1..20)) {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let Some(b) = pg_backend().await else { return };
                let (_, _, mb) = acct(&b, "props").await;
                run(&mb, ops).await;
            });
        }
    }
}

// ---------------------------------------------------------------- differential model

/// Operations over the whole facade surface with small id spaces, so that missing ids, foreign
/// folders, conflicts and quota errors are exercised as often as successes.
#[derive(Debug, Clone)]
enum DOp {
    /// folder, own Message-ID number, flags, parent (0 = none, 1..=6 reply to m{p-1}, 7 = References chain)
    Append(u8, u8, u8, u8),
    AppendMany(u8, u8),
    Deliver(u8, u8, u8),
    DeliverMany(u8, u8, u8),
    SetFlags(u8, u8),
    SetFlagsMany(u8, u8, u8),
    AddKeyword(u8, u8),
    RemoveKeyword(u8, u8),
    Move(u8, u8),
    MoveMany(u8, u8, u8),
    Expunge(u8),
    ExpungeMany(u8, u8),
    ExpungeAt(u8, u8),
    Purge(bool),
    CreateFolder(u8, u8),
    RenameFolder(u8, u8),
    MoveFolder(u8, u8),
    DeleteFolder(u8),
}

fn dop() -> impl Strategy<Value = DOp> {
    prop_oneof![
        4 => (0u8..7, 0u8..6, 0u8..32, 0u8..8).prop_map(|(f, s, fl, p)| DOp::Append(f, s, fl, p)),
        2 => (0u8..7, 1u8..4).prop_map(|(f, n)| DOp::AppendMany(f, n)),
        2 => (0u8..7, 0u8..4, 0u8..4).prop_map(|(f, d, c)| DOp::Deliver(f, d, c)),
        2 => (0u8..7, 0u8..4, 1u8..4).prop_map(|(f, d, n)| DOp::DeliverMany(f, d, n)),
        2 => (0u8..4, 0u8..32).prop_map(|(m, b)| DOp::SetFlags(m, b)),
        1 => (0u8..4, 1u8..5, 0u8..32).prop_map(|(m, n, b)| DOp::SetFlagsMany(m, n, b)),
        1 => (0u8..4, 0u8..4).prop_map(|(m, k)| DOp::AddKeyword(m, k)),
        1 => (0u8..4, 0u8..4).prop_map(|(m, k)| DOp::RemoveKeyword(m, k)),
        2 => (0u8..4, 0u8..7).prop_map(|(m, f)| DOp::Move(m, f)),
        1 => (0u8..4, 1u8..5, 0u8..7).prop_map(|(m, n, f)| DOp::MoveMany(m, n, f)),
        1 => (0u8..4).prop_map(DOp::Expunge),
        1 => (0u8..4, 1u8..5).prop_map(|(m, n)| DOp::ExpungeMany(m, n)),
        1 => (0u8..4, 0u8..3).prop_map(|(m, t)| DOp::ExpungeAt(m, t)),
        1 => proptest::bool::ANY.prop_map(DOp::Purge),
        2 => (0u8..7, 0u8..4).prop_map(|(p, n)| DOp::CreateFolder(p, n)),
        1 => (0u8..7, 0u8..4).prop_map(|(f, n)| DOp::RenameFolder(f, n)),
        1 => (0u8..7, 0u8..7).prop_map(|(f, p)| DOp::MoveFolder(f, p)),
        1 => (0u8..7).prop_map(DOp::DeleteFolder),
    ]
}

fn kind<T>(r: &rusty_core::Result<T>) -> String
where
    T: std::fmt::Debug,
{
    use rusty_core::Error;
    match r {
        Ok(v) => format!("ok {v:?}"),
        Err(Error::NotFound(_)) => "NotFound".into(),
        Err(Error::Conflict(_)) => "Conflict".into(),
        Err(Error::Invalid(_)) => "Invalid".into(),
        Err(Error::Quota(_)) => "Quota".into(),
        Err(e) => format!("other {e:?}"),
    }
}

fn folder_id(f: u8) -> FolderId {
    // 0 is never a folder; 1..=5 are the system folders; 6.. the custom ones.
    FolderId(i64::from(f) + 1)
}

fn ids(m: u8, n: u8) -> Vec<MessageId> {
    (0..n)
        .map(|k| MessageId(i64::from(m) + i64::from(k) + 1))
        .collect()
}

/// A message with Message-ID `m{mid}@x` (if any) and a parent: 0 none, 1..=6 In-Reply-To
/// `m{p-1}@x`, 7 a References chain (`m0@x m1@x m2@x`, nearest ancestor last) plus an In-Reply-To.
fn threaded(subject: &str, mid: Option<u8>, parent: u8) -> bytes::Bytes {
    let id = mid.map(|m| format!("m{m}@x"));
    let irt = (1..=6)
        .contains(&parent)
        .then(|| format!("m{}@x", parent - 1));
    match parent {
        7 => reply(
            subject,
            id.as_deref(),
            Some("m5@x"),
            &["m0@x", "m1@x", "m2@x"],
        ),
        _ => reply(subject, id.as_deref(), irt.as_deref(), &[]),
    }
}

/// Content of delivery payload number `c` (the same `c` is always the same bytes).
fn content(c: u8) -> bytes::Bytes {
    threaded(&format!("content-{c}"), Some(c % 6), (c + 2) % 8)
}

fn item(d: u8, c: u8) -> rusty_store::DeliverItem {
    rusty_store::DeliverItem {
        raw: content(c),
        flags: Flags::empty(),
        delivery_id: rusty_core::DeliveryId([d; 32]),
    }
}

/// Run `ops`; return one line per operation plus a final snapshot of the observable state
/// (everything except wall-clock values).
async fn exec(mb: &Mailbox, ops: &[DOp]) -> Vec<String> {
    let mut out = vec![];
    for (step, o) in ops.iter().enumerate() {
        let line = match o {
            DOp::Append(f, s, fl, p) => {
                let r = mb
                    .append(
                        folder_id(*f),
                        threaded(&format!("subj{s}"), Some(*s), *p),
                        Flags::from_bits(*fl).unwrap(),
                    )
                    .await;
                kind(&r)
            }
            DOp::AppendMany(f, n) => {
                // Message-IDs and parents drawn from the same small space as `Append`, so replies
                // find anchors in the database AND earlier in the same batch.
                let raws = (0..*n)
                    .map(|i| {
                        let k = step as u8 + i;
                        threaded(
                            &format!("am{step}-{i}"),
                            Some(k % 6),
                            (k.wrapping_mul(3)) % 8,
                        )
                    })
                    .collect();
                kind(&mb.append_many(folder_id(*f), raws, Flags::empty()).await)
            }
            DOp::Deliver(f, d, c) => kind(
                &mb.deliver(
                    folder_id(*f),
                    content(*c),
                    Flags::empty(),
                    rusty_core::DeliveryId([*d; 32]),
                )
                .await,
            ),
            DOp::DeliverMany(f, d, n) => {
                let items = (0..*n)
                    .map(|i| item(d.wrapping_add(i) % 4, (d + i) % 3))
                    .collect();
                kind(&mb.deliver_many(folder_id(*f), items).await)
            }
            DOp::SetFlags(m, b) => kind(
                &mb.set_flags(MessageId(i64::from(*m) + 1), Flags::from_bits(*b).unwrap())
                    .await,
            ),
            DOp::SetFlagsMany(m, n, b) => kind(
                &mb.set_flags_many(&ids(*m, *n), Flags::from_bits(*b).unwrap())
                    .await,
            ),
            DOp::AddKeyword(m, k) => kind(
                &mb.add_keyword(MessageId(i64::from(*m) + 1), &format!("k{k}"))
                    .await,
            ),
            DOp::RemoveKeyword(m, k) => kind(
                &mb.remove_keyword(MessageId(i64::from(*m) + 1), &format!("k{k}"))
                    .await,
            ),
            DOp::Move(m, f) => kind(
                &mb.move_message(MessageId(i64::from(*m) + 1), folder_id(*f))
                    .await,
            ),
            DOp::MoveMany(m, n, f) => kind(&mb.move_many(&ids(*m, *n), folder_id(*f)).await),
            DOp::Expunge(m) => kind(&mb.expunge(MessageId(i64::from(*m) + 1)).await),
            DOp::ExpungeMany(m, n) => kind(&mb.expunge_many(&ids(*m, *n)).await),
            DOp::ExpungeAt(m, t) => kind(
                &mb.expunge_many_at(&[MessageId(i64::from(*m) + 1)], i64::from(*t) * 100)
                    .await,
            ),
            DOp::Purge(all) => kind(&mb.purge_deleted(if *all { i64::MAX } else { 150 }).await),
            DOp::CreateFolder(p, n) => {
                let parent = if *p == 0 { None } else { Some(folder_id(*p)) };
                kind(&mb.create_folder(parent, &format!("n{n}")).await)
            }
            DOp::RenameFolder(f, n) => {
                kind(&mb.rename_folder(folder_id(*f), &format!("n{n}")).await)
            }
            DOp::MoveFolder(f, p) => {
                let parent = if *p == 0 { None } else { Some(folder_id(*p)) };
                kind(&mb.move_folder(folder_id(*f), parent).await)
            }
            DOp::DeleteFolder(f) => kind(&mb.delete_folder(folder_id(*f)).await),
        };
        out.push(format!("{step}: {o:?} -> {line}"));
    }
    // Final snapshot.
    out.push(format!("folders: {:?}", mb.list_folders().await));
    let changes = mb.changes_since(0, MAX_PAGE).await.unwrap().changes;
    out.push(format!("changes: {changes:?}"));
    for f in mb.list_folders().await.unwrap() {
        let list = mb.list_folder(f.id, 0, MAX_PAGE).await.unwrap();
        let rows: Vec<_> = list
            .iter()
            .map(|m| {
                (
                    m.id,
                    m.uid,
                    m.size,
                    m.subject.clone(),
                    m.from_addr.clone(),
                    m.flags,
                    m.thread_id,
                )
            })
            .collect();
        out.push(format!("folder {}: {rows:?}", f.id.0));
    }
    for id in 1..=14 {
        let id = MessageId(id);
        let d = mb.get_message(id).await.map(|d| {
            (
                d.folder,
                d.blob,
                d.deleted,
                d.summary.uid,
                d.summary.flags,
                d.summary.thread_id,
            )
        });
        out.push(format!(
            "message {}: {} kw={:?}",
            id.0,
            kind(&d),
            kind(&mb.keywords(id).await)
        ));
    }
    out
}

async fn differential(ops: Vec<DOp>) {
    use common::backends::{acct, sqlite_backend};
    let sq = sqlite_backend().await;
    let (_, _, m_sq) = acct(&sq, "diff").await;
    let expected = exec(&m_sq, &ops).await;
    #[cfg(feature = "postgres")]
    if let Some(pg) = common::backends::pg_backend().await {
        let (_, _, m_pg) = acct(&pg, "diff").await;
        let got = exec(&m_pg, &ops).await;
        assert_eq!(expected.len(), got.len());
        for (e, g) in expected.iter().zip(&got) {
            assert_eq!(
                e, g,
                "SQLite (left) and Postgres (right) diverge for {ops:?}"
            );
        }
    }
    let _ = expected;
}

proptest! {
    #![proptest_config(ProptestConfig { cases: pg_cases(), ..ProptestConfig::default() })]

    /// Model-based differential test: the same random operation sequence (20 ops) produces the
    /// same results, error kinds and final observable state on SQLite and Postgres. Without a
    /// Postgres server only the SQLite half runs (it must not panic).
    // backend-neutral: differential test: drives both backends itself and compares them
    #[test]
    fn sqlite_and_postgres_are_observably_identical(ops in proptest::collection::vec(dop(), 1..20)) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(differential(ops));
    }
}
