#!/usr/bin/env bash
# Shared by scripts/check-mutation-hook.sh and scripts/check-test-fixtures.sh (sourced, not run).
#
# Why a binary: a marker that is only referenced by tests never lands in object code, and an rlib
# carries every constant in its metadata, so comparing two rlibs proves nothing about what a
# shipped executable contains (review PG6 finding 3). The gates therefore link a small scratch
# program against rusty-store, with and without the feature, and grep the LINKED executable.
# The program opens a store (the production path) and, with the feature, calls the gated API.
#
#   probe_binary <rusty-store features, comma separated> <probe features> [target triple]
#       builds the scratch program in release mode and prints the path of the executable
#   binary_has_marker <executable> <marker>      exit 0 when the marker string is in the file
#
# The scratch crate lives in a temporary directory and shares the Cargo.lock of the repository
# (so no new dependency versions are resolved) and the target directory of the caller.
PROBE_NAME="rusty-gate-probe"

probe_binary() {
  local store_features="$1" probe_features="$2" target="${3:-}" repo dir tdir bin feats=""
  repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  dir="$(mktemp -d)"
  mkdir -p "$dir/src"
  local f
  for f in ${store_features//,/ }; do feats="$feats\"$f\", "; done
  cat >"$dir/Cargo.toml" <<EOF
[package]
name = "$PROBE_NAME"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
rusty-store = { path = "$repo/crates/rusty-store", features = [${feats%, }] }
tokio = { version = "1", default-features = false, features = ["rt"] }

[features]
m = ["rusty-store/mutation-testing"]
f = ["rusty-store/test-fixtures"]

[workspace]
EOF
  cat >"$dir/src/main.rs" <<'EOF'
fn main() {
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(async {
        // The production path: opening a store runs the migrations through the hook.
        let store = rusty_store::SqliteStore::open("sqlite::memory:").await.unwrap();
        #[cfg(feature = "m")]
        println!("{:?}", rusty_store::mutation::active_id());
        #[cfg(feature = "f")]
        println!("{}", rusty_store::fixtures::Fixtures::from_sqlite(store).is_sqlite());
        #[cfg(not(feature = "f"))]
        drop(store);
    });
}
EOF
  cp "$repo/Cargo.lock" "$dir/Cargo.lock"
  local args=(--release --manifest-path "$dir/Cargo.toml" --message-format=json)
  [ -z "$target" ] || args+=(--target "$target")
  [ -z "$probe_features" ] || args+=(--features "$probe_features")
  # The probe shares the caller's target directory; its executable name is unique to the gates.
  bin="$(cargo build "${args[@]}" 2>"$dir/build.log" \
    | { grep -o '"executable":"[^"]*'"$PROBE_NAME"'"' || true; } | tail -1 | sed 's/^"executable":"//; s/"$//')"
  if [ -z "$bin" ] || [ ! -f "$bin" ]; then
    echo "probe build failed (features: $store_features / $probe_features):" >&2
    tail -20 "$dir/build.log" >&2
    rm -rf "$dir"
    return 1
  fi
  # Keep the executable beyond the scratch directory: copy it next to the build log's directory.
  local out
  out="$(mktemp)"
  cp "$bin" "$out"
  rm -rf "$dir" "$bin"
  echo "$out"
}

binary_has_marker() { grep -aqF -- "$2" "$1"; }
