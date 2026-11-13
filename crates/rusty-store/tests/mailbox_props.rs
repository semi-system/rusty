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
