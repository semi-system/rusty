#!/usr/bin/env bash
# Self-test of scripts/check-wrapped-libs.sh on a scratch tree.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
mk() { rm -rf "$tmp/t"; mkdir -p "$tmp/t/scripts" "$tmp/t/crates/a/src/adapter" "$tmp/t/crates/b/src" "$tmp/t/fuzz"
  printf 'libx crates/a/src/adapter\n' > "$tmp/t/scripts/wrapped-libs.txt"
  printf '[dependencies]\nlibx = "1"\n' > "$tmp/t/crates/a/Cargo.toml"
  printf 'use libx::Thing;\n' > "$tmp/t/crates/a/src/adapter/mod.rs"; echo 'fn main(){}' > "$tmp/t/crates/b/src/lib.rs"; }
expect() { local want="$2" rc=0; bash "$here/scripts/check-wrapped-libs.sh" "$tmp/t" >/dev/null 2>&1 || rc=$?; [ "$rc" = "$want" ] || { echo "FAIL: $1 (rc=$rc want=$want)" >&2; exit 1; }; }
mk; expect "clean tree" 0
mk; echo 'use libx::Thing;' >> "$tmp/t/crates/b/src/lib.rs"; expect "use outside the adapter" 1
mk; echo '// see libx docs' >> "$tmp/t/crates/a/src/lib.rs"; expect "mention in the owner crate outside the adapter" 1
mk; printf '[dependencies]\nlibx = "1"\n' > "$tmp/t/crates/b/Cargo.toml"; expect "manifest of another crate" 1
mk; echo 'use libx_other::X;' >> "$tmp/t/crates/b/src/lib.rs"; expect "a different crate with a similar name" 0
mk; echo 'use libx::T;' > "$tmp/t/fuzz/t.rs"; expect "fuzz target" 1
echo "wrapped-libs gate self-test ok"
