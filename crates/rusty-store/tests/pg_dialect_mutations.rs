//! backend-neutral: needs the `mutation-testing` feature (passed by scripts/pg-mutate.sh, never enabled by a manifest); the hook is identity unless RUSTY_SQL_MUTATION is set
//!
//! Dialect mutation tests (plan D5). The table of mutations lives in `src/mutation.rs`; this file
//! checks the table and the hook, and gives the runner a probe that proves a mutation is not
//! vacuous. The kill evidence comes from running the REAL conformance suites with
//! `RUSTY_SQL_MUTATION=<id>` (see `scripts/pg-mutate.sh` and the committed mutation table).
#![cfg(feature = "mutation-testing")]

mod common;

use common::backends::{acct, backends};
use rusty_core::Flags;
use rusty_store::mutation::{self, Dialect, HOOK_MARKER, TABLE};

fn repo_sources() -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/repo");
    let mut out = String::new();
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
                out.push_str(&std::fs::read_to_string(&p).unwrap());
            }
        }
    }
    // The rendered clock tokens exist only after `{NOW}` rendering.
    out.push_str("\nrs_now() unixepoch()\n");
    // A string continuation (backslash, newline, indentation) joins the pieces.
    let mut joined = String::with_capacity(out.len());
    let mut it = out.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' && it.peek() == Some(&'\n') {
            it.next();
            while it.peek() == Some(&' ') {
                it.next();
            }
            continue;
        }
        joined.push(c);
    }
    joined
}

// backend-neutral: reads sources, no database
#[test]
fn every_mutation_can_bite_ids_are_unique_and_the_marker_exists() {
    let src = repo_sources();
    let mut ids = std::collections::HashSet::new();
    for m in TABLE {
        assert!(ids.insert(m.id), "duplicate id {}", m.id);
        assert!(!m.what.is_empty());
        if m.flag.is_empty() {
            assert!(!m.find.is_empty());
            // Literal text of the statement as written in the source (escapes aside).
            let probe = m.find.replace("rs_now()", "{NOW}");
            let probe = probe.as_str();
            assert!(
                src.contains(probe),
                "{}: `find` text does not occur in any repository source: {probe}",
                m.id
            );
            assert_ne!(m.find, m.replace, "{}: a no-op mutation", m.id);
        }
    }
    assert!(HOOK_MARKER.contains("mutation-hook"));
}

// backend-neutral: unit test of the hook, no database
#[test]
fn the_hook_is_identity_without_a_selected_mutation() {
    if mutation::active_id().is_some() {
        return; // the runner selected one: this test is about the unselected state
    }
    let before = mutation::hits();
    for d in [Dialect::Sqlite, Dialect::Postgres] {
        let s = mutation::rendered(d, "SELECT rs_now() FROM unnest($1) AS t(a, b)".to_owned());
        assert_eq!(s, "SELECT rs_now() FROM unnest($1) AS t(a, b)");
    }
    assert_eq!(mutation::hits(), before);
    assert!(!mutation::flag("no-retry"));
}

// backend-neutral: runner probe, drives both backends itself
#[tokio::test]
async fn the_selected_mutation_changed_at_least_one_statement() {
    let Some(id) = mutation::active_id() else {
        return; // nothing selected: nothing to probe
    };
    let m = TABLE.iter().find(|m| m.id == id).unwrap();
    for b in backends().await {
        // Every operation may fail under a mutation; only reaching the statements matters.
        let Ok(r) = tokio::time::timeout(std::time::Duration::from_secs(60), tokio::spawn(async move {
            let (_d, _a, mb) = acct(&b, "probe").await;
            let raw = bytes::Bytes::from_static(b"Subject: probe\r\nMessage-ID: <p@x>\r\n\r\nbody\r\n");
            let _ = mb.append(rusty_core::FolderId(1), raw.clone(), Flags::empty()).await;
            let _ = mb.create_folder(None, "F").await;
            let items = vec![rusty_store::DeliverItem {
                raw,
                flags: Flags::empty(),
                delivery_id: rusty_core::DeliveryId([7; 32]),
            }];
            let _ = mb.deliver_many(rusty_core::FolderId(1), items).await;
            let _ = mb.set_flags(rusty_core::MessageId(1), Flags::SEEN).await;
            let _ = mb.move_message(rusty_core::MessageId(1), rusty_core::FolderId(2)).await;
            let _ = mb.expunge(rusty_core::MessageId(1)).await;
            let jq = b.jobs();
            let _ = jq.enqueue("probe", None, &serde_json::json!({}), None).await;
            if let Ok(Some(j)) = jq.claim("w", &["probe"], 30).await {
                let _ = jq.complete(j.id, j.epoch).await;
            }
            let _ = b.gc().purge_deleted(i64::MAX).await;
        }))
        .await
        else {
            continue;
        };
        let _ = r;
    }
    if m.flag.is_empty() {
        assert!(mutation::hits() > 0, "mutation {id} matched no statement: it is vacuous");
    }
}

/// Prints the mutation list for `scripts/pg-mutate.sh`: the dialect/repository table and the
/// schema mutations derived from the live Postgres catalog (drop every named CHECK / UNIQUE /
/// FOREIGN KEY constraint, every trigger and every stand-alone index), as `KIND\tid\ttext`.
// backend-neutral: runner helper (ignored), prints the mutation list
#[tokio::test]
#[ignore]
async fn list_mutations() {
    use common::pg::{pg_admin_url, PgTestDb};
    for m in TABLE {
        println!("DIALECT\t{}\t{}", m.id, m.what);
    }
    let Some(url) = pg_admin_url() else { return };
    let db = PgTestDb::create(&url).await;
    let store = db.connect().await;
    let rows: Vec<(String, String, String)> = sqlx_rows(
        &store,
        "SELECT 'constraint', c.conrelid::regclass::text || '.' || c.conname, \
                'ALTER TABLE ' || c.conrelid::regclass::text || ' DROP CONSTRAINT ' || quote_ident(c.conname) \
         FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace \
         WHERE n.nspname = 'public' AND c.contype IN ('c', 'u', 'f') AND c.conrelid::regclass::text <> '_sqlx_migrations' \
         UNION ALL \
         SELECT 'trigger', c.relname || '.' || t.tgname, 'DROP TRIGGER ' || quote_ident(t.tgname) || ' ON ' || c.relname \
         FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND NOT t.tgisinternal \
         UNION ALL \
         SELECT 'index', i.tablename || '.' || i.indexname, 'DROP INDEX ' || quote_ident(i.indexname) \
         FROM pg_indexes i WHERE i.schemaname = 'public' AND i.tablename <> '_sqlx_migrations' \
           AND NOT EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conindid = (quote_ident(i.indexname))::regclass) \
         ORDER BY 1, 2",
    )
    .await;
    for (kind, id, sql) in rows {
        println!("SCHEMA\t{kind}:{id}\t{sql}");
    }
    store.close().await;
    db.cleanup().await;
}

async fn sqlx_rows(store: &rusty_store::PgStore, sql: &str) -> Vec<(String, String, String)> {
    use sqlx::Row;
    sqlx::query(sql)
        .fetch_all(store.pool())
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect()
}

// backend-neutral: reads the committed table, no database
#[test]
fn the_committed_mutation_table_lists_every_dialect_mutation() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/superpowers/plans/2026-11-03-postgres-backend-mutation-table.md");
    let doc = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}; run scripts/pg-mutate.sh", path.display()));
    for m in TABLE {
        assert!(doc.contains(&format!("`{}`", m.id)), "mutation {} is not in the committed table; regenerate it", m.id);
    }
    assert!(!doc.contains("| UNEXPLAINED"), "unexplained survivors in the table");
}
