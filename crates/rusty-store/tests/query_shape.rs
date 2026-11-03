//! Query shape: thread and ancestor lookups cost a constant number of statements and behave
//! exactly like the original step-by-step implementations (kept here as oracles).

mod common;

use std::collections::HashMap;

use common::*;
use proptest::prelude::*;
use rusty_core::{Error, Flags, FolderId, MessageId};
use rusty_store::MAX_FOLDER_DEPTH;

// ---------- find_thread ----------

/// The ORIGINAL find_thread: one query per candidate (references last to first, then
/// In-Reply-To, at most 32), the smallest message id per header, soft-deleted rows included.
async fn oracle_thread(e: &Env, refs: &[String], irt: Option<&str>) -> Option<i64> {
    let mb = e.mb.id().0;
    for mid in refs.iter().rev().map(String::as_str).chain(irt).take(32) {
        let t: Option<i64> = sqlx::query_scalar(
            "SELECT thread_id FROM messages WHERE mailbox_id = ? AND message_id_hdr = ? \
             ORDER BY id LIMIT 1",
        )
        .bind(mb)
        .bind(mid)
        .fetch_optional(e.store.pool())
        .await
        .unwrap();
        if t.is_some() {
            return t;
        }
    }
    None
}

#[derive(Debug, Clone)]
struct Msg {
    own: Option<u8>,
    refs: Vec<u8>,
    irt: Option<u8>,
    delete_after: bool,
}

fn msg() -> impl Strategy<Value = Msg> {
    (
        proptest::option::of(0u8..12),
        proptest::collection::vec(0u8..16, 0..40),
        proptest::option::of(0u8..16),
        proptest::bool::weighted(0.2),
    )
        .prop_map(|(own, refs, irt, delete_after)| Msg {
            own,
            refs,
            irt,
            delete_after,
        })
}

fn id_str(n: u8) -> String {
    format!("id{n}@t.test")
}

async fn thread_equivalence(msgs: Vec<Msg>) {
    let _p = stmts::probe();
    let e = env().await;
    for m in msgs {
        let refs: Vec<String> = m.refs.iter().map(|&r| id_str(r)).collect();
        let irt = m.irt.map(id_str);
        let own = m.own.map(id_str);
        let ref_strs: Vec<&str> = refs.iter().map(String::as_str).collect();
        let raw = reply("s", own.as_deref(), irt.as_deref(), &ref_strs);
        // The envelope parser caps and filters; compute the oracle from what it keeps.
        let env = rusty_mime::parse_envelope(&raw).unwrap();
        let expect = oracle_thread(&e, &env.references, env.in_reply_to.as_deref()).await;
        let r = e.mb.append(INBOX, raw, Flags::empty()).await.unwrap();
        let got: i64 = e
            .scalar(&format!(
                "SELECT thread_id FROM messages WHERE id = {}",
                r.id.0
            ))
            .await;
        assert_eq!(got, expect.unwrap_or(r.id.0));
        if m.delete_after {
            e.mb.expunge(r.id).await.unwrap();
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    #[test]
    fn find_thread_matches_per_candidate_oracle(msgs in proptest::collection::vec(msg(), 1..30)) {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
            .block_on(thread_equivalence(msgs));
    }
}

#[tokio::test]
async fn duplicate_message_ids_choose_the_smallest_id_and_deleted_anchor_counts() {
    let _p = stmts::probe();
    let e = env().await;
    let a =
        e.mb.append(INBOX, raw("a", Some("dup@t")), Flags::empty())
            .await
            .unwrap();
    let b =
        e.mb.append(INBOX, raw("b", Some("dup@t")), Flags::empty())
            .await
            .unwrap();
    assert_ne!(a.id, b.id);
    e.mb.expunge(a.id).await.unwrap(); // a soft-deleted anchor still anchors
    let c =
        e.mb.append(INBOX, reply("c", None, Some("dup@t"), &[]), Flags::empty())
            .await
            .unwrap();
    let t = |id: MessageId| format!("SELECT thread_id FROM messages WHERE id = {}", id.0);
    assert_eq!(e.scalar(&t(c.id)).await, e.scalar(&t(a.id)).await);
}

#[tokio::test]
async fn find_thread_uses_one_query() {
    let p = stmts::probe(); // first: serializes this test, setup included
    let e = env().await;
    let refs: Vec<String> = (0..40).map(|i| format!("ref{i}@t.test")).collect();
    e.mb.append(INBOX, raw("anchor", Some(&refs[20])), Flags::empty())
        .await
        .unwrap();
    let refs_s: Vec<&str> = refs.iter().map(String::as_str).collect();
    let many = reply("x", None, Some("irt@t.test"), &refs_s);
    let none = raw("y", None);

    p.reset();
    e.mb.append(INBOX, none, Flags::empty()).await.unwrap();
    let base = p.count();
    p.reset();
    e.mb.append(INBOX, many, Flags::empty()).await.unwrap();
    assert_eq!(
        p.count(),
        base + 1,
        "32 candidates must cost exactly one extra statement"
    );
}

// ---------- ancestor_depth (via create_folder / move_folder) ----------

/// The ORIGINAL ancestor walk over a model of the tree (id -> parent): the same checks in
/// the same order as the step-by-step implementation.
fn oracle_depth(
    tree: &HashMap<i64, Option<i64>>,
    start: i64,
    forbidden: Option<i64>,
) -> Result<usize, Error> {
    let mut cur = Some(start);
    let mut depth = 0usize;
    while let Some(id) = cur {
        if forbidden == Some(id) {
            return Err(Error::Conflict("x".into()));
        }
        depth += 1;
        if depth > MAX_FOLDER_DEPTH {
            return Err(Error::Invalid("x".into()));
        }
        cur = *tree.get(&id).ok_or_else(|| Error::NotFound("x".into()))?;
    }
    Ok(depth)
}

fn height(tree: &HashMap<i64, Option<i64>>, id: i64) -> usize {
    1 + tree
        .iter()
        .filter(|(_, p)| **p == Some(id))
        .map(|(c, _)| height(tree, *c))
        .max()
        .unwrap_or(0)
}

fn kind<T>(r: &Result<T, Error>) -> &'static str {
    match r {
        Ok(_) => "ok",
        Err(Error::Conflict(_)) => "conflict",
        Err(Error::Invalid(_)) => "invalid",
        Err(Error::NotFound(_)) => "notfound",
        Err(_) => "other",
    }
}

#[derive(Debug, Clone)]
enum FOp {
    /// Create under the newest folder (builds deep chains), under a random one, or at the root.
    CreateDeep,
    CreateUnder(usize),
    CreateRoot,
    Move(usize, Option<usize>),
}

fn fop() -> impl Strategy<Value = FOp> {
    prop_oneof![
        6 => Just(FOp::CreateDeep),
        2 => (0usize..80).prop_map(FOp::CreateUnder),
        1 => Just(FOp::CreateRoot),
        3 => (0usize..80, proptest::option::of(0usize..80)).prop_map(|(a, b)| FOp::Move(a, b)),
    ]
}

fn too_deep(d: usize, extra: usize) -> Result<(), Error> {
    if d + extra > MAX_FOLDER_DEPTH {
        Err(Error::Invalid("x".into()))
    } else {
        Ok(())
    }
}

async fn folder_equivalence(ops: Vec<FOp>) {
    let _p = stmts::probe();
    let e = env().await;
    let mut tree: HashMap<i64, Option<i64>> = HashMap::new();
    let mut order: Vec<i64> = vec![];
    for op in ops {
        match op {
            FOp::CreateDeep | FOp::CreateUnder(_) | FOp::CreateRoot => {
                let parent = match op {
                    FOp::CreateDeep => order.last().copied(),
                    FOp::CreateUnder(i) if !order.is_empty() => Some(order[i % order.len()]),
                    _ => None,
                };
                let expect = match parent {
                    Some(p) => oracle_depth(&tree, p, None).and_then(|d| too_deep(d, 1)),
                    None => Ok(()),
                };
                let got =
                    e.mb.create_folder(parent.map(FolderId), &format!("f{}", order.len()))
                        .await;
                assert_eq!(kind(&got), kind(&expect), "create under {parent:?}");
                if let Ok(id) = got {
                    tree.insert(id.0, parent);
                    order.push(id.0);
                }
            }
            FOp::Move(a, b) => {
                if order.is_empty() {
                    continue;
                }
                let id = order[a % order.len()];
                let np = b.map(|i| order[i % order.len()]);
                let expect = if tree[&id] == np {
                    Ok(())
                } else if let Some(p) = np {
                    oracle_depth(&tree, p, Some(id)).and_then(|d| too_deep(d, height(&tree, id)))
                } else {
                    Ok(())
                };
                let got = e.mb.move_folder(FolderId(id), np.map(FolderId)).await;
                assert_eq!(kind(&got), kind(&expect), "move {id} under {np:?}");
                if got.is_ok() {
                    tree.insert(id, np);
                }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]
    #[test]
    fn ancestor_depth_matches_step_by_step_oracle(ops in proptest::collection::vec(fop(), 20..140)) {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
            .block_on(folder_equivalence(ops));
    }
}

#[tokio::test]
async fn depth_64_is_allowed_65_is_invalid_and_descendant_move_conflicts() {
    let _p = stmts::probe();
    let e = env().await;
    let mut cur = e.mb.create_folder(None, "d1").await.unwrap();
    let first = cur;
    for i in 2..=MAX_FOLDER_DEPTH {
        cur =
            e.mb.create_folder(Some(cur), &format!("d{i}"))
                .await
                .unwrap();
    }
    assert!(matches!(
        e.mb.create_folder(Some(cur), "d65").await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        e.mb.move_folder(first, Some(cur)).await,
        Err(Error::Conflict(_))
    ));
}

#[tokio::test]
async fn ancestor_depth_uses_one_query() {
    let p = stmts::probe(); // first: serializes this test, setup included
    let e = env().await;
    let shallow = e.mb.create_folder(None, "s").await.unwrap();
    let mut deep = e.mb.create_folder(None, "d1").await.unwrap();
    for i in 2..=60 {
        deep =
            e.mb.create_folder(Some(deep), &format!("d{i}"))
                .await
                .unwrap();
    }
    p.reset();
    e.mb.create_folder(None, "r").await.unwrap(); // no ancestor walk at all
    let root = p.count();
    p.reset();
    e.mb.create_folder(Some(shallow), "a").await.unwrap();
    let one = p.count();
    p.reset();
    e.mb.create_folder(Some(deep), "b").await.unwrap(); // a 60-level walk
    let sixty = p.count();
    assert_eq!(
        one,
        root + 1,
        "an ancestor walk costs exactly one statement"
    );
    assert_eq!(sixty, one, "and does not grow with depth");
}

#[tokio::test]
async fn cyclic_folder_data_is_invalid_not_a_hang() {
    let _p = stmts::probe();
    let e = env().await;
    let a = e.mb.create_folder(None, "a").await.unwrap();
    let b = e.mb.create_folder(Some(a), "b").await.unwrap();
    let c = e.mb.create_folder(None, "c").await.unwrap();
    // Corrupt data: drop the guard trigger and close a cycle a -> b -> a by raw SQL.
    sqlx::query("DROP TRIGGER folders_no_cycle")
        .execute(e.store.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE folders SET parent_id = ? WHERE id = ?")
        .bind(b.0)
        .bind(a.0)
        .execute(e.store.pool())
        .await
        .unwrap();
    assert!(matches!(
        e.mb.create_folder(Some(b), "x").await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        e.mb.move_folder(c, Some(a)).await,
        Err(Error::Invalid(_))
    ));
}

// ---------- lock first (every MailboxRepo write) ----------

/// Review risk 6.3.1 on SQLite: the first data statement of every write transaction is the read
/// of the mailbox row (after `BEGIN IMMEDIATE`). The Postgres twin lives in `query_shape_pg.rs`.
#[tokio::test]
async fn every_write_method_takes_the_mailbox_row_first_on_sqlite() {
    let p = stmts::probe();
    let b = common::backends::sqlite_backend().await;
    let all = common::shapes::collect(&b, &p).await;
    assert!(all.iter().filter(|s| s.write).count() >= 25);
    for s in &all {
        common::shapes::assert_lock_first("sqlite", s);
    }
}
