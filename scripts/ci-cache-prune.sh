#!/usr/bin/env bash
# Shrink a cargo target directory before it is saved to the CI cache.
#   ci-cache-prune.sh [TARGET_DIR]      (default: target)
# Removes what a restored cache would not reuse or what is rebuilt anyway:
#   * incremental data (CI builds with CARGO_INCREMENTAL=0, but a stray directory is huge),
#   * every artefact of the workspace crates themselves (fingerprints, rlibs, build-script output,
#     test and binary executables): they change with every commit, so keeping them only makes a
#     cache entry bigger and churns it; third-party dependencies, which are the expensive part, stay,
#   * extension-less executables in deps/ (test binaries) and top-level binaries of each profile.
# Workspace crate names come from the Cargo.toml files of crates/* and tools/* (no jq, no cargo).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
T="${1:-target}"
[ -d "$T" ] || { echo "ci-cache-prune: $T does not exist, nothing to prune"; exit 0; }
before="$(du -sk "$T" | cut -f1)"

names=()
for m in crates/*/Cargo.toml tools/*/Cargo.toml; do
  [ -f "$m" ] || continue
  n="$(sed -n 's/^name *= *"\([^"]*\)".*/\1/p' "$m" | head -n 1)"
  [ -z "$n" ] || names+=("$n")
done

# Every profile directory: target/debug, target/release, target/<triple>/{debug,release}.
mapfile -t profiles < <(find "$T" -maxdepth 2 -type d \( -name debug -o -name release \) | LC_ALL=C sort)
for p in "${profiles[@]}"; do
  rm -rf "$p/incremental"
  for n in "${names[@]}"; do
    u="${n//-/_}"
    rm -rf "$p"/.fingerprint/"$n"-* "$p"/build/"$n"-*
    rm -f "$p"/deps/"$n"-* "$p"/deps/"$u"-* "$p"/deps/lib"$u"-* "$p"/"$n" "$p"/lib"$u".* 2>/dev/null || true
  done
  # test binaries (no extension, executable) and top-level binaries
  if [ -d "$p/deps" ]; then
    find "$p/deps" -maxdepth 1 -type f -perm -u+x ! -name '*.*' -delete
  fi
  find "$p" -maxdepth 1 -type f -perm -u+x ! -name '*.*' -delete
done

after="$(du -sk "$T" | cut -f1)"
echo "ci-cache-prune: $T ${before} KiB -> ${after} KiB (${#names[@]} workspace crates)"
