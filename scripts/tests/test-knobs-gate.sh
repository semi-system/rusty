#!/usr/bin/env bash
# Script test: scripts/check-test-knobs.sh static passes on a clean tree and fails for each way
# of enabling the test-knobs feature outside rusty-store's own tests.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1
GATE="$PWD/scripts/check-test-knobs.sh"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
mk() { rm -rf "$tmp/t"; mkdir -p "$tmp/t/crates/app/src" "$tmp/t/crates/rusty-store/src" "$tmp/t/deploy" "$tmp/t/fuzz" "$tmp/t/tools/x"
  printf '[workspace]\nmembers = ["crates/*"]\n[workspace.dependencies]\nserde = "1"\n' > "$tmp/t/Cargo.toml"
  printf '[package]\nname="app"\n[dependencies]\nrusty-store = { path = "../rusty-store" }\n' > "$tmp/t/crates/app/Cargo.toml"
  printf 'fn main() {}\n' > "$tmp/t/crates/app/src/lib.rs"
  printf '[package]\nname="rusty-store"\n[features]\ndefault = []\npostgres = ["sqlx/postgres"]\n# comment about test-knobs\ntest-knobs = ["tokio/sync"]\n[dependencies]\ntokio = "1"\n[dev-dependencies]\nrusty-store = { path = ".", features = ["test-knobs"] }\nproptest = "1"\n' > "$tmp/t/crates/rusty-store/Cargo.toml"
  printf 'fn x() {}\n' > "$tmp/t/crates/rusty-store/src/lib.rs"; }
expect() { # name, expected rc (0 pass / 1 fail)
  (cd "$tmp/t" && bash "$GATE" static "$tmp/t" >/dev/null 2>&1); local rc=$?
  if [ "$rc" -ne "$2" ]; then echo "test-knobs-gate: '$1': expected rc $2, got $rc" >&2; exit 1; fi; }
mk; expect "clean tree" 0
mk; sed -i 's|^default = \[\]|default = ["test-knobs"]|' "$tmp/t/crates/rusty-store/Cargo.toml"; expect "default feature enables it" 1
mk; sed -i 's|^postgres = \["sqlx/postgres"\]|postgres = ["sqlx/postgres", "test-knobs"]|' "$tmp/t/crates/rusty-store/Cargo.toml"; expect "postgres feature enables it" 1
mk; sed -i 's|^tokio = "1"|tokio = "1"\nrusty-store = { path = ".", features = ["test-knobs"] }|' "$tmp/t/crates/rusty-store/Cargo.toml"; expect "self dependency under [dependencies]" 1
mk; printf '[build-dependencies]\nrusty-store = { path = ".", features = ["test-knobs"] }\n' >> "$tmp/t/crates/rusty-store/Cargo.toml"; expect "self build-dependency" 1
mk; printf 'x = { path = "../rusty-store", features = ["test-knobs"] }\n' >> "$tmp/t/crates/app/Cargo.toml"; expect "other crate depends with the feature" 1
mk; printf '[dev-dependencies]\nrusty-store = { path = "../rusty-store", features = ["test-knobs"] }\n' >> "$tmp/t/crates/app/Cargo.toml"; expect "other crate dev-dependency" 1
mk; printf '[workspace.dependencies.rusty-store]\npath = "x"\nfeatures = ["test-knobs"]\n' >> "$tmp/t/Cargo.toml"; expect "workspace root manifest" 1
mk; printf '[package]\nname="t"\n[dependencies]\nrusty-store = { path = "x", features = ["test-knobs"] }\n' > "$tmp/t/tools/x/Cargo.toml"; expect "tools manifest" 1
mk; printf '[package]\nname="f"\n[dependencies]\nrusty-store = { path = "x", features = ["test-knobs"] }\n' > "$tmp/t/fuzz/Cargo.toml"; expect "fuzz manifest" 1
mk; echo 'let r = repo.with_knobs_for_test(k);' > "$tmp/t/crates/app/src/a.rs"; expect "knob setter in another crate" 1
mk; echo 'repo.inject_deadlocks_for_test(2);' > "$tmp/t/tools/x/m.rs"; expect "deadlock injector in tools" 1
mk; echo '#[cfg(feature = "test-knobs")] fn f() {}' > "$tmp/t/crates/app/src/a.rs"; expect "cfg on the feature elsewhere" 1
mk; echo 'RUN cargo build --features rusty-store/test-knobs' > "$tmp/t/deploy/Dockerfile"; expect "Dockerfile under deploy" 1
mk; mkdir -p "$tmp/t/.github/workflows"; echo '      - run: cargo build --features test-knobs' > "$tmp/t/.github/workflows/ci.yml"; expect "GitHub workflow" 1
mk; mkdir -p "$tmp/t/.cargo"; printf '[build]\nrustflags = []\n# test-knobs\n' > "$tmp/t/.cargo/config.toml"; expect ".cargo config" 1
mk; echo 'pub fn knob_style() {}' > "$tmp/t/crates/app/src/a.rs"; expect "unrelated text is not a hit" 0
echo "test-knobs-gate: OK"
