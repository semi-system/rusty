#!/usr/bin/env bash
# Script test: scripts/check-service-crates.sh passes for facade-only code and fails for each way of
# reaching around the traits (concrete store types, escape hatches, SqliteStore constructors, free GC functions).
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1
GATE="$PWD/scripts/check-service-crates.sh"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
mk() { rm -rf "$tmp/t"; mkdir -p "$tmp/t/crates/rusty-svc/src" "$tmp/t/crates/rusty/src" "$tmp/t/crates/rusty-svc/tests"
  printf 'use rusty_store::Repos;\nfn ok(r: &Repos) { let _d = Directory::with_repo(r.directory.clone()); let _m = Mailbox::open_with(r.mailbox.clone(), b, a); let _j = JobQueue::with_repo(r.jobs.clone()); let g = Gc::new(r.gc.clone()); g.purge_deliveries_default(); ctx.gc.collect_blobs(b, d); let _s = Store::open(c); }\n' > "$tmp/t/crates/rusty-svc/src/lib.rs"; }
expect() { # name, expected rc
  (cd "$tmp/t" && bash "$GATE" "$tmp/t" >/dev/null 2>&1); local rc=$?
  if [ "$rc" -ne "$2" ]; then echo "service-crates-gate: '$1': expected rc $2, got $rc" >&2; exit 1; fi; }
bad() { # name, line, file
  mk; printf '%s\n' "$2" >> "$tmp/t/${3:-crates/rusty-svc/src/lib.rs}"; expect "$1" 1; }
mk; expect "facade-only tree" 0
bad "SqliteStore named" 'fn f(s: &SqliteStore) {}'
bad "SqliteStore in a test" 'fn f(s: &SqliteStore) {}' crates/rusty-svc/tests/t.rs
bad "SqliteStore in the composition root" 'use rusty_store::SqliteStore;' crates/rusty/src/main.rs
bad "PgStore named" 'fn f(s: PgStore) {}'
bad "SqliteRepo named" 'let r = SqliteRepo::new(s);'
bad "repo::sqlite path" 'use rusty_store::repo::sqlite::X;'
bad "store.sqlite() escape hatch" 'let s = store.sqlite();'
bad "spaced .sqlite ( )" 'let s = store . sqlite ( );'
bad "store.postgres() escape hatch" 'let s = store.postgres();'
bad "Directory::new" 'let d = Directory::new(store);'
bad "Mailbox::open" 'let m = Mailbox::open(s, b, a).await;'
bad "JobQueue::new" 'let j = JobQueue::new(s);'
bad "JobQueue::with_limits" 'let j = JobQueue::with_limits(s, 1, 1);'
bad "free purge_deleted" 'purge_deleted(&store, 1).await;'
bad "free purge_deliveries_default" 'rusty_store::purge_deliveries_default(&s).await;'
bad "free collect_blobs" 'let r = collect_blobs(&s, &b, g).await;'
bad "free purge_capabilities_at" 'purge_capabilities_at(&s, 1, 2).await;'
echo "service-crates-gate: OK"
