#!/usr/bin/env bash
# Script test: scripts/check-wrapped-libs.sh passes when the library is named only in the adapter and
# fails for a use anywhere else (source, test, comment-free code, chrono).
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
GATE="$PWD/scripts/check-wrapped-libs.sh"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
mk() { rm -rf "$tmp/t"; mkdir -p "$tmp/t/crates/rusty-imap/src" "$tmp/t/crates/rusty-imap/tests"
  printf 'use imap_codec::CommandCodec;\nuse chrono::Datelike;\n' > "$tmp/t/crates/rusty-imap/src/codec.rs"
  printf 'use crate::command::Body;\n' > "$tmp/t/crates/rusty-imap/src/session.rs"; }
expect() { (cd "$tmp/t" && bash "$GATE" "$tmp/t" >/dev/null 2>&1); local rc=$?
  if [ "$rc" -ne "$2" ]; then echo "wrapped-libs-gate: '$1': expected rc $2, got $rc" >&2; exit 1; fi; }
bad() { mk; printf '%s\n' "$2" >> "$tmp/t/${3:-crates/rusty-imap/src/session.rs}"; expect "$1" 1; }
mk; expect "adapter-only tree" 0
bad "imap_codec in the session" 'use imap_codec::imap_types::command::Command;'
bad "imap_types path" 'fn f(_: imap_types::core::Tag) {}'
bad "chrono in a parser" 'use chrono::NaiveDate;' crates/rusty-imap/src/search.rs
bad "library in a test" 'use imap_codec::CommandCodec;' crates/rusty-imap/tests/t.rs
echo "wrapped-libs-gate: OK"
