#!/usr/bin/env bash
# Script test: scripts/check-mutation-hook.sh static passes on a clean tree and fails when anything
# but rusty-store's own [features] definition mentions the mutation-testing feature.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
GATE="$PWD/scripts/check-mutation-hook.sh"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
mk() { rm -rf "$tmp/t"; mkdir -p "$tmp/t/crates/app/src" "$tmp/t/crates/rusty-store/src" "$tmp/t/deploy" "$tmp/t/tools/x"
  printf '[workspace]\nmembers = ["crates/*"]\n' > "$tmp/t/Cargo.toml"
  printf '[package]\nname="app"\n[dependencies]\nrusty-store = { path = "../rusty-store" }\n' > "$tmp/t/crates/app/Cargo.toml"
  printf 'fn main() {}\n' > "$tmp/t/crates/app/src/lib.rs"
  printf '[package]\nname="rusty-store"\n[features]\ndefault = []\n# about mutation-testing\nmutation-testing = []\n[dependencies]\ntokio = "1"\n' > "$tmp/t/crates/rusty-store/Cargo.toml"
  printf 'fn x() {}\n' > "$tmp/t/crates/rusty-store/src/lib.rs"; }
expect() { (cd "$tmp/t" && bash "$GATE" static "$tmp/t" >/dev/null 2>&1); local rc=$?
  if [ "$rc" -ne "$2" ]; then echo "mutation-hook-gate: '$1': expected rc $2, got $rc" >&2; exit 1; fi; }
mk; expect "clean tree" 0
mk; sed -i 's|^default = \[\]|default = ["mutation-testing"]|' "$tmp/t/crates/rusty-store/Cargo.toml"; expect "default feature enables it" 1
mk; printf '[dev-dependencies]\nrusty-store = { path = ".", features = ["mutation-testing"] }\n' >> "$tmp/t/crates/rusty-store/Cargo.toml"; expect "self dev-dependency" 1
mk; printf '[dev-dependencies]\nrusty-store = { path = "../rusty-store", features = ["mutation-testing"] }\n' >> "$tmp/t/crates/app/Cargo.toml"; expect "other crate dev-dependency" 1
mk; printf '[features]\nm = ["rusty-store/mutation-testing"]\n' >> "$tmp/t/crates/app/Cargo.toml"; expect "other crate re-exports it" 1
mk; echo 'use rusty_store::mutation::TABLE;' > "$tmp/t/crates/app/src/a.rs"; expect "hook API in another crate" 1
mk; echo 'RUN cargo build --features rusty-store/mutation-testing' > "$tmp/t/deploy/Dockerfile"; expect "Dockerfile" 1
mk; echo 'pub fn mutation_style() {}' > "$tmp/t/crates/app/src/a.rs"; expect "unrelated text" 0
echo "mutation-hook-gate: OK"
