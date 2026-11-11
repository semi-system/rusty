#!/usr/bin/env bash
# Self-test of scripts/ci-cache-prune.sh on a fake target directory inside a scratch copy of the
# crate layout: workspace artefacts and test binaries go, third-party artefacts stay.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
fail() { echo "ci-cache-prune self-test: $*" >&2; exit 1; }
mkdir -p "$tmp/scripts" "$tmp/crates/rusty-core" "$tmp/tools/compose-check"
cp "$here/scripts/ci-cache-prune.sh" "$tmp/scripts/"
printf '[package]\nname = "rusty-core"\n' > "$tmp/crates/rusty-core/Cargo.toml"
printf '[package]\nname = "compose-check"\n' > "$tmp/tools/compose-check/Cargo.toml"
cd "$tmp"
for prof in target/debug target/x86_64-unknown-linux-musl/release; do
  mkdir -p "$prof/deps" "$prof/.fingerprint/rusty-core-aaa" "$prof/.fingerprint/serde-bbb" "$prof/build/rusty-core-aaa" "$prof/build/ring-ccc" "$prof/incremental/x"
  : > "$prof/deps/librusty_core-aaa.rlib"; : > "$prof/deps/rusty_core-aaa.d"; : > "$prof/deps/libserde-bbb.rlib"
  : > "$prof/deps/libserde_derive-ddd.so"; : > "$prof/deps/pg_smoke-eee"; chmod +x "$prof/deps/pg_smoke-eee"
  : > "$prof/rusty"; chmod +x "$prof/rusty"; : > "$prof/.cargo-lock"; : > "$prof/compose-check"; chmod +x "$prof/compose-check"
  : > "$prof/deps/compose_check-fff"; chmod +x "$prof/deps/compose_check-fff"
done
bash scripts/ci-cache-prune.sh target >"$tmp/out"
grep -q '2 workspace crates' "$tmp/out" || fail "crate names not found: $(cat "$tmp/out")"
for prof in target/debug target/x86_64-unknown-linux-musl/release; do
  for gone in .fingerprint/rusty-core-aaa build/rusty-core-aaa incremental deps/librusty_core-aaa.rlib deps/rusty_core-aaa.d deps/pg_smoke-eee rusty compose-check deps/compose_check-fff; do
    [ ! -e "$prof/$gone" ] || fail "$prof/$gone was not removed"
  done
  for kept in .fingerprint/serde-bbb build/ring-ccc deps/libserde-bbb.rlib deps/libserde_derive-ddd.so .cargo-lock; do
    [ -e "$prof/$kept" ] || fail "$prof/$kept was removed"
  done
done
bash scripts/ci-cache-prune.sh "$tmp/absent" | grep -q 'nothing to prune' || fail "missing target dir must be a no-op"
echo "ci-cache-prune self-test OK"
