#!/usr/bin/env bash
# Script test: scripts/check-raw-db.sh passes on a clean tree and fails for each bypass variant.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1
GATE="$PWD/scripts/check-raw-db.sh"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
mk() { rm -rf "$tmp/t"; mkdir -p "$tmp/t/crates/app/src" "$tmp/t/crates/app/tests" "$tmp/t/fuzz/fuzz_targets" "$tmp/t/tools/x/src" "$tmp/t/crates/rusty-store/src"
  printf 'fn main() {}\n' > "$tmp/t/crates/app/src/lib.rs"
  printf '[package]\nname="app"\n[dependencies]\nrusty-store = { path = "../rusty-store" }\n' > "$tmp/t/crates/app/Cargo.toml"
  printf 'let p = s.pool();\n' > "$tmp/t/crates/rusty-store/src/lib.rs"; }
expect() { # name, expected rc (0 pass / 1 fail)
  (cd "$tmp/t" && bash "$GATE" "$tmp/t" >/dev/null 2>&1); local rc=$?
  if [ "$rc" -ne "$2" ]; then echo "raw-db-gate: '$1': expected rc $2, got $rc" >&2; exit 1; fi; }
mk; expect "clean tree" 0
mk; echo 'let p = s.pool();' > "$tmp/t/crates/app/src/a.rs"; expect "plain .pool()" 1
mk; echo 'let p = s . pool ( );' > "$tmp/t/crates/app/src/a.rs"; expect "whitespace .pool ()" 1
mk; echo 'let p = SqliteStore::pool(&s);' > "$tmp/t/crates/app/src/a.rs"; expect "UFCS pool" 1
mk; echo 'let c = p.acquire().await;' > "$tmp/t/crates/app/src/a.rs"; expect ".acquire()" 1
mk; echo 'let s = d.store().clone();' > "$tmp/t/crates/app/src/a.rs"; expect ".store() escape" 1
mk; echo 'let t = s.begin_write();' > "$tmp/t/crates/app/tests/t.rs"; expect "begin_write in tests/" 1
mk; echo 'use sqlx::SqlitePool;' > "$tmp/t/fuzz/fuzz_targets/f.rs"; expect "sqlx in fuzz/" 1
mk; echo 'let p: PgPool;' > "$tmp/t/tools/x/src/main.rs"; expect "PgPool in tools/" 1
mk; printf 'sqlx = "0.8"\n' >> "$tmp/t/crates/app/Cargo.toml"; expect "sqlx under [dependencies]" 1
mk; printf '[dev-dependencies]\nsqlx = "0.8"\n' >> "$tmp/t/crates/app/Cargo.toml"; expect "sqlx dev-dependency outside rusty-directory" 1
mk; printf "[dependencies.sqlx]
version = \"0.8\"
" >> "$tmp/t/crates/app/Cargo.toml"; expect "sqlx table form" 1
mk; echo "stop.store(true, Ordering::Relaxed);" > "$tmp/t/tools/x/src/main.rs"; expect "atomic store() is not a hit" 0

mk; mkdir -p "$tmp/t/crates/rusty-directory/tests"; echo 'let p = d.store().pool();' > "$tmp/t/crates/rusty-directory/tests/directory.rs"; expect "no exemption for the rusty-directory tests" 1
mk; mkdir -p "$tmp/t/crates/rusty-directory"; printf '[package]\nname="rusty-directory"\n[dev-dependencies]\nsqlx = "0.8"\n' > "$tmp/t/crates/rusty-directory/Cargo.toml"; expect "no sqlx dev-dependency exemption for rusty-directory" 1
mk; mkdir -p "$tmp/t/crates/rusty-directory/tests"; echo 'let f = Fixtures::of(&store); f.scalar("SELECT 1", &[]).await;' > "$tmp/t/crates/rusty-directory/tests/d.rs"; expect "the fixtures API passes the text scan" 0
echo "raw-db-gate: OK"
