//! The rules a migration must follow so that an update needs no downtime (docs/design-zero-downtime.md). From migration 0034 on, every migration states in `schema_compat` the oldest binary that
//! keeps working on it, and one that says an older binary keeps working (`min_binary` below its own number) may only ADD: no drop, no rename, no type change, no new NOT NULL without a
//! default, no data deletion. A migration that must break (a drop, once nothing reads the column) says `min_binary` equal to its own number and starts with the line `-- rusty:breaking <why>`;
//! the deploy tool then knows an update across it is not rolling.

use std::fs;
use std::path::Path;

const FIRST_RULED: u64 = 34;

/// The tables that grow with the mail: an index built on one while it is written blocks every delivery for as long as the build takes.
const BIG_TABLES: [&str; 8] = ["messages", "jobs", "change_log", "deliveries", "blobs", "blob_locations", "keywords", "folders"];

/// An index on a big table must be built `CONCURRENTLY`, which needs a migration outside a transaction (`-- no-transaction` as the first line, a sqlx directive).
fn blocking_index(stmt: &str) -> Option<String> {
    if !(stmt.starts_with("create index") || stmt.starts_with("create unique index")) || stmt.contains(" concurrently ") {
        return None;
    }
    let on = stmt.split(" on ").nth(1)?;
    let table = on.trim_start_matches("only ").split(|c: char| !(c.is_alphanumeric() || c == '_')).next()?;
    BIG_TABLES.contains(&table).then(|| table.to_owned())
}

/// The statements of a migration without comments, lower-cased, split at `;`.
fn statements(sql: &str) -> Vec<String> {
    let no_comments: String = sql.lines().filter(|l| !l.trim_start().starts_with("--")).collect::<Vec<_>>().join("\n");
    no_comments.split(';').map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()).filter(|s| !s.is_empty()).collect()
}

/// Why this statement is not allowed in an additive migration, if it is not.
fn breaks_old_binaries(stmt: &str) -> Option<&'static str> {
    let s = stmt;
    for (needle, why) in [
        ("drop table", "drops a table"),
        ("drop column", "drops a column"),
        ("drop index", "drops an index (the old release may depend on its plan)"),
        ("drop constraint", "drops a constraint"),
        ("drop trigger", "drops a trigger"),
        ("drop function", "drops a function"),
        ("drop type", "drops a type"),
        ("rename", "renames something"),
        ("truncate", "truncates a table"),
        ("delete from", "deletes rows"),
        ("set not null", "makes a column NOT NULL"),
        (" type ", "may change a column type"),
    ] {
        if s.contains(needle) && (needle != " type " || s.contains("alter column")) {
            return Some(why);
        }
    }
    if s.contains("add column") && s.contains("not null") && !s.contains("default") {
        return Some("adds a NOT NULL column without a default (the old release's INSERT would fail)");
    }
    None
}

#[test]
fn migrations_from_0034_declare_their_compatibility_and_additive_ones_only_add() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut checked = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let Some(number) = name.split('_').next().and_then(|n| n.parse::<u64>().ok()) else { continue };
        if number < FIRST_RULED {
            continue;
        }
        checked += 1;
        let sql = fs::read_to_string(&path).unwrap();
        let stmts = statements(&sql);
        let row = stmts.iter().find(|s| s.starts_with("insert into schema_compat")).unwrap_or_else(|| panic!("{name}: no `INSERT INTO schema_compat (version, min_binary)` row"));
        let values = row.split("values").nth(1).unwrap_or_else(|| panic!("{name}: the schema_compat row has no VALUES"));
        let nums: Vec<u64> = values.split(|c: char| !c.is_ascii_digit()).filter(|t| !t.is_empty()).map(|t| t.parse().unwrap()).collect();
        assert_eq!(nums.len(), 2, "{name}: the row is (version, min_binary)");
        assert_eq!(nums[0], number, "{name}: the row names another migration");
        assert!(nums[1] <= number, "{name}: min_binary is above the migration's own number");
        let breaking = nums[1] == number;
        let marked = sql.lines().next().is_some_and(|l| l.starts_with("-- rusty:breaking"));
        assert_eq!(breaking, marked, "{name}: a breaking migration (min_binary = its number) starts with `-- rusty:breaking <why>`, and only such a migration does");
        for s in &stmts {
            if let Some(table) = blocking_index(s) {
                panic!("{name}: builds an index on `{table}` without CONCURRENTLY: it would block deliveries while it builds. Use `CREATE INDEX CONCURRENTLY` and start the file with `-- no-transaction`.");
            }
        }
        if sql.contains(" CONCURRENTLY ") || sql.to_lowercase().contains(" concurrently ") {
            assert!(sql.lines().next().is_some_and(|l| l.trim() == "-- no-transaction"), "{name}: CONCURRENTLY needs `-- no-transaction` as the first line");
        }
        if !breaking {
            for s in &stmts {
                if let Some(why) = breaks_old_binaries(s) {
                    panic!("{name}: says the old release keeps working but {why}: `{s}`. Split it: add now, remove in a later release, or mark it `-- rusty:breaking`.");
                }
            }
        }
    }
    // Nothing to check yet is fine; the rule starts with the next migration.
    let _ = checked;
}

#[test]
fn an_index_on_a_big_table_must_be_concurrent() {
    assert_eq!(blocking_index(&statements("CREATE INDEX i ON messages (a)")[0]).as_deref(), Some("messages"));
    assert!(blocking_index(&statements("CREATE INDEX CONCURRENTLY i ON messages (a)")[0]).is_none());
    assert!(blocking_index(&statements("CREATE INDEX i ON accounts (a)")[0]).is_none(), "a small table is fine");
}

#[test]
fn the_rule_catches_what_it_is_for() {
    for bad in ["ALTER TABLE t DROP COLUMN c", "ALTER TABLE t RENAME COLUMN a TO b", "ALTER TABLE t ALTER COLUMN a TYPE bigint", "ALTER TABLE t ALTER COLUMN a SET NOT NULL", "ALTER TABLE t ADD COLUMN a bigint NOT NULL", "DELETE FROM t"] {
        assert!(breaks_old_binaries(&statements(bad)[0]).is_some(), "{bad}");
    }
    for fine in ["ALTER TABLE t ADD COLUMN a bigint", "ALTER TABLE t ADD COLUMN a bigint NOT NULL DEFAULT 0", "CREATE TABLE t (a bigint NOT NULL)", "CREATE INDEX i ON t (a)", "UPDATE t SET a = 1"] {
        assert!(breaks_old_binaries(&statements(fine)[0]).is_none(), "{fine}");
    }
}
