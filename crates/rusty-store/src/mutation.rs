//! SQL mutation hook (feature `mutation-testing`, task PG6).
//!
//! Mutation testing of the dialect layer: one environment variable selects ONE mutation from
//! [`TABLE`] and the repositories then run with a deliberately broken statement text (or schema),
//! so `scripts/pg-mutate.sh` can run the real conformance suites against it and record which test
//! fails. The hook is compiled only with the `mutation-testing` feature, which no manifest enables
//! (the runner passes it on the command line); `scripts/check-mutation-hook.sh` proves a release
//! build contains none of this code (marker below) and that nothing enables the feature.
//!
//! Three mutation kinds, all selected by `RUSTY_SQL_MUTATION=<id>`:
//! * statement text: a literal `find` -> `replace` applied to every statement of one dialect that
//!   contains `find` (shared statements after `{NOW}` rendering, and the dialect-only statements);
//! * schema: statements run right after the migrations (drop a constraint, trigger or index; add
//!   an object on one side only), given in `RUSTY_SQL_MUTATION_POST_MIGRATE`;
//! * flag: Rust-logic switches read with [`flag`] (no retry).
//!
//! With the variable unset every function is the identity.

use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// Present in the binary only when the hook is compiled in (release gate marker).
pub const HOOK_MARKER: &str = "rusty-sql-mutation-hook-marker";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    Postgres,
}

impl Dialect {
    pub fn name(self) -> &'static str {
        match self {
            Dialect::Sqlite => "sqlite",
            Dialect::Postgres => "postgres",
        }
    }
}

/// One text or flag mutation.
#[derive(Debug)]
pub struct Mutation {
    pub id: &'static str,
    pub dialect: Dialect,
    /// Literal text to find in a statement (`""` for a flag mutation).
    pub find: &'static str,
    pub replace: &'static str,
    /// Flag name read by [`flag`] (`""` for a text mutation).
    pub flag: &'static str,
    pub what: &'static str,
}

const fn text(id: &'static str, dialect: Dialect, find: &'static str, replace: &'static str, what: &'static str) -> Mutation {
    Mutation { id, dialect, find, replace, flag: "", what }
}

use Dialect::{Postgres as PG, Sqlite as LITE};

/// The dialect and repository mutations of the committed table
/// (`docs/superpowers/plans/2026-11-03-postgres-backend-mutation-table.md`).
pub const TABLE: &[Mutation] = &[
    // ---- dialect translation (plan D5)
    text("D01-bare-int-in-union", PG, "CAST(1 AS BIGINT)", "1", "CAST(1 AS BIGINT) becomes a bare 1: Postgres types it integer, the decoder expects bigint"),
    text("D02-no-derived-table-alias", PG, " AS t(seq, item)", "", "unnest derived table loses its alias (Postgres requires one)"),
    text("D03-least-for-greatest", PG, "GREATEST(", "LEAST(", "uid_validity high-water mark uses LEAST instead of GREATEST"),
    text("D04-in-for-any", PG, "= ANY($", "IN ($", "= ANY($n) written as IN ($n): invalid for an array parameter"),
    text("D05-unixepoch-in-postgres", PG, "rs_now()", "unixepoch()", "the SQLite clock function in a Postgres statement"),
    text("D06-rs-now-in-sqlite", LITE, "unixepoch()", "rs_now()", "the Postgres clock function in a SQLite statement"),
    text("D07-question-placeholders", PG, "$1", "?1", "$N placeholders written as ?N on Postgres"),
    text("D08-unnest-insert-unordered", PG, "FROM unnest($2::bigint[], $3::bigint[]) AS t(seq, item) ORDER BY t.seq", "FROM unnest($2::bigint[], $3::bigint[]) AS t(seq, item)", "change_log unnest insert loses its ORDER BY"),
    text("D09-blob-insert-unordered", PG, "AS t(h, s) ORDER BY t.h", "AS t(h, s)", "batch blob insert loses its ORDER BY (the ascending lock order)"),
    text("D10-blob-insert-no-conflict-clause", PG, " ON CONFLICT (hash) DO NOTHING", "", "ON CONFLICT DO NOTHING removed from the blob upsert"),
    text("D11-list-member-insert-do-nothing", PG, "INSERT INTO list_members (list_id, account_id, external) VALUES ($1, $2, $3)", "INSERT INTO list_members (list_id, account_id, external) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING", "ON CONFLICT DO NOTHING added to the list member insert (a duplicate member no longer errors)"),
    text("D12-account-insert-do-nothing", PG, "VALUES ($1, $2, $3, $4, 'active', rs_now()) RETURNING id", "VALUES ($1, $2, $3, $4, 'active', rs_now()) ON CONFLICT DO NOTHING RETURNING id", "ON CONFLICT DO NOTHING added to the account insert (a duplicate address no longer errors)"),
    // ---- repository SQL (plan step 3)
    text("R01-no-mailbox-row-lock", PG, "SELECT id FROM mailboxes WHERE id = $1 FOR UPDATE", "SELECT id FROM mailboxes WHERE id = $1", "the mailbox row lock (FOR UPDATE) dropped"),
    text("R02-no-skip-locked", PG, "FOR UPDATE OF j SKIP LOCKED", "FOR UPDATE OF j", "job claim without SKIP LOCKED"),
    text("R03-claim-ignores-lease", PG, "lease_expires_at <= ", "lease_expires_at <= 1000000000 + ", "claim and its lock step treat a live lease as expired (reclaims running jobs)"),
    text("R04-fence-without-epoch", PG, "WHERE id = $1 AND epoch = $2 AND state = 'running' AND lease_expires_at > rs_now() AND (domain_id", "WHERE id = $1 AND state = 'running' AND lease_expires_at > rs_now() AND (domain_id", "the fence (COMPLETE_JOB) ignores the epoch"),
    text("R05-fence-without-lease", PG, "WHERE id = $1 AND epoch = $2 AND state = 'running' AND lease_expires_at > rs_now() AND (domain_id", "WHERE id = $1 AND epoch = $2 AND state = 'running' AND (domain_id", "the fence (COMPLETE_JOB) ignores lease expiry"),
    text("R06-transaction-clock", PG, "rs_now()", "extract(epoch FROM now())::bigint", "the database clock is the transaction start (now()) instead of the statement clock"),
    text("R07-gc-no-grace-recheck", PG, "AND zero_since IS NOT NULL AND zero_since < $2", "", "GC delete does not re-check the grace cutoff under the row lock"),
    text("R08-gc-shard-listing-empty", PG, "FROM blobs WHERE hash >= $1 AND hash <= $2", "FROM blobs WHERE hash >= $1 AND hash <= $2 AND false", "the GC shard listing returns nothing"),
    // ---- Rust logic
    Mutation { id: "L01-no-retry", dialect: PG, find: "", replace: "", flag: "no-retry", what: "a deadlock/serialization failure is not retried (one attempt)" },
];

struct Active {
    mutation: Option<&'static Mutation>,
    post_migrate: Vec<(Dialect, String)>,
}

fn active() -> &'static Active {
    static ACTIVE: OnceLock<Active> = OnceLock::new();
    ACTIVE.get_or_init(|| {
        // Keeps the marker string in the object code of every binary that reaches the hook (a
        // const used only by tests never lands in a binary; scripts/check-mutation-hook.sh).
        std::hint::black_box(HOOK_MARKER);
        let id = std::env::var("RUSTY_SQL_MUTATION").unwrap_or_default();
        let mutation = TABLE.iter().find(|m| !id.is_empty() && m.id == id);
        // Schema mutations: "<dialect>|<sql>" entries separated by a line with only ";;".
        let mut post_migrate = vec![];
        if let Ok(spec) = std::env::var("RUSTY_SQL_MUTATION_POST_MIGRATE") {
            for part in spec.split("\n;;\n") {
                if let Some((d, sql)) = part.split_once('|') {
                    let d = if d.trim() == "sqlite" { Dialect::Sqlite } else { Dialect::Postgres };
                    post_migrate.push((d, sql.trim().to_owned()));
                }
            }
        }
        Active { mutation, post_migrate }
    })
}

static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Statement texts that went through the hook while a mutation was selected (debugging aid for a
/// vacuous mutation: shows what the statements really look like).
pub fn seen() -> Vec<String> {
    SEEN.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

fn note(sql: &str) {
    if active().mutation.is_some() {
        let mut s = SEEN.lock().unwrap_or_else(|p| p.into_inner());
        if s.len() < 5000 {
            s.push(sql.to_owned());
        }
    }
}

/// Statements changed so far in this process (a mutation that never matched is vacuous).
static HITS: AtomicUsize = AtomicUsize::new(0);

pub fn hits() -> usize {
    HITS.load(Ordering::SeqCst)
}

/// The id of the active mutation, if any.
pub fn active_id() -> Option<&'static str> {
    active().mutation.map(|m| m.id)
}

/// Apply the active text mutation to a statement of `dialect`.
pub fn statement(dialect: Dialect, sql: &'static str) -> Cow<'static, str> {
    note(sql);
    match active().mutation {
        Some(m) if m.dialect == dialect && !m.find.is_empty() && sql.contains(m.find) => {
            HITS.fetch_add(1, Ordering::SeqCst);
            Cow::Owned(sql.replace(m.find, m.replace))
        }
        _ => Cow::Borrowed(sql),
    }
}

/// Same, for a statement that is already rendered at run time.
pub fn rendered(dialect: Dialect, sql: String) -> String {
    note(&sql);
    match active().mutation {
        Some(m) if m.dialect == dialect && !m.find.is_empty() && sql.contains(m.find) => {
            HITS.fetch_add(1, Ordering::SeqCst);
            sql.replace(m.find, m.replace)
        }
        _ => sql,
    }
}

/// Is the Rust-logic mutation `name` active?
pub fn flag(name: &str) -> bool {
    active().mutation.is_some_and(|m| !m.flag.is_empty() && m.flag == name)
}

/// Schema mutation statements for `dialect`, run right after the migrations.
pub(crate) fn post_migrate(dialect: Dialect) -> Vec<String> {
    active()
        .post_migrate
        .iter()
        .filter(|(d, _)| *d == dialect)
        .map(|(_, s)| s.clone())
        .collect()
}
