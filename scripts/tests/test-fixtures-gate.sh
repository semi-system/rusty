#!/usr/bin/env bash
# Script test: scripts/check-test-fixtures.sh static passes on a clean tree and fails for each way
# of enabling the test-fixtures feature outside dev-dependencies.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1
GATE="$PWD/scripts/check-test-fixtures.sh"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
mk() { rm -rf "$tmp/t"; mkdir -p "$tmp/t/crates/app/src" "$tmp/t/crates/app/tests" "$tmp/t/crates/rusty-store/src" "$tmp/t/deploy" "$tmp/t/fuzz" "$tmp/t/tools/x"
  printf '[workspace]\nmembers = ["crates/*"]\n[workspace.dependencies]\nserde = "1"\n' > "$tmp/t/Cargo.toml"
  printf '[package]\nname="app"\n[dependencies]\nrusty-store = { path = "../rusty-store" }\n[dev-dependencies]\nrusty-store = { path = "../rusty-store", features = ["test-fixtures"] }\n' > "$tmp/t/crates/app/Cargo.toml"
  printf 'fn main() {}\n' > "$tmp/t/crates/app/src/lib.rs"
  printf 'use rusty_store::fixtures::Fixtures;\n' > "$tmp/t/crates/app/tests/t.rs"
  printf '[package]\nname="rusty-store"\n[features]\ndefault = []\npostgres = ["sqlx/postgres"]\n# comment about test-fixtures\ntest-fixtures = ["dep:url"]\n[dependencies]\ntokio = "1"\n[dev-dependencies]\nrusty-store = { path = ".", features = ["test-fixtures"] }\n' > "$tmp/t/crates/rusty-store/Cargo.toml"
  printf 'fn x() {}\n' > "$tmp/t/crates/rusty-store/src/lib.rs"; }
expect() { # name, expected rc (0 pass / 1 fail)
  (cd "$tmp/t" && bash "$GATE" static "$tmp/t" >/dev/null 2>&1); local rc=$?
  if [ "$rc" -ne "$2" ]; then echo "test-fixtures-gate: '$1': expected rc $2, got $rc" >&2; exit 1; fi; }
mk; expect "clean tree (dev-dependency in another crate, tests use the API)" 0
mk; sed -i 's|^default = \[\]|default = ["test-fixtures"]|' "$tmp/t/crates/rusty-store/Cargo.toml"; expect "default feature enables it" 1
mk; sed -i 's|^postgres = \["sqlx/postgres"\]|postgres = ["sqlx/postgres", "test-fixtures"]|' "$tmp/t/crates/rusty-store/Cargo.toml"; expect "postgres feature enables it" 1
mk; sed -i 's|^tokio = "1"|tokio = "1"\nrusty-store = { path = ".", features = ["test-fixtures"] }|' "$tmp/t/crates/rusty-store/Cargo.toml"; expect "self dependency under [dependencies]" 1
mk; sed -i 's|^\[dependencies\]\nrusty-store = { path = "../rusty-store" }|X|; s|^rusty-store = { path = "../rusty-store" }$|rusty-store = { path = "../rusty-store", features = ["test-fixtures"] }|' "$tmp/t/crates/app/Cargo.toml"; expect "other crate [dependencies] enables it" 1
mk; printf '[build-dependencies]\nrusty-store = { path = "../rusty-store", features = ["test-fixtures"] }\n' >> "$tmp/t/crates/app/Cargo.toml"; expect "other crate build-dependency" 1
mk; printf '[features]\nfx = ["rusty-store/test-fixtures"]\n' >> "$tmp/t/crates/app/Cargo.toml"; expect "other crate re-exports the feature" 1
mk; printf '[workspace.dependencies.rusty-store]\npath = "x"\nfeatures = ["test-fixtures"]\n' >> "$tmp/t/Cargo.toml"; expect "workspace root manifest" 1
mk; printf '[package]\nname="t"\n[dependencies]\nrusty-store = { path = "x", features = ["test-fixtures"] }\n' > "$tmp/t/tools/x/Cargo.toml"; expect "tools manifest" 1
mk; echo 'use rusty_store::fixtures::Fixtures;' > "$tmp/t/crates/app/src/a.rs"; expect "fixtures API in production source" 1
mk; echo 'use rusty_store::fixtures::Fixtures;' > "$tmp/t/tools/x/m.rs"; expect "fixtures API in tools" 1
mk; echo 'RUN cargo build --features rusty-store/test-fixtures' > "$tmp/t/deploy/Dockerfile"; expect "Dockerfile under deploy" 1
mk; mkdir -p "$tmp/t/.github/workflows"; echo '      - run: cargo build --features test-fixtures' > "$tmp/t/.github/workflows/ci.yml"; expect "GitHub workflow" 1
mk; echo 'pub fn fixture_style() {}' > "$tmp/t/crates/app/src/a.rs"; expect "unrelated text is not a hit" 0
# binary mode: the marker anywhere in an executable fails the gate (a linked binary carries it as plain data).
printf 'ELF....\0\0%s\0\0tail' "rusty-test-fixtures-marker" > "$tmp/with"; printf 'ELF....\0\0nothing\0\0' > "$tmp/without"
bin_expect() { bash "$GATE" binary "$1" >/dev/null 2>&1; local rc=$?
  if [ "$rc" -ne "$2" ]; then echo "test-fixtures-gate: binary '$1': expected rc $2, got $rc" >&2; exit 1; fi; }
bin_expect "$tmp/with" 1; bin_expect "$tmp/without" 0
echo "test-fixtures-gate: OK"
