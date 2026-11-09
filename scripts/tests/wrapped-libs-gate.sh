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
mk; printf 'use libx::Req;\npub(crate) type R = Req;\n' > "$tmp/t/crates/a/src/adapter/mod.rs"; expect "a pub(crate) type alias of a library type" 1
mk; printf 'use libx::Req;\npub(crate) fn parse(r: Req) -> u8 { 0 }\n' > "$tmp/t/crates/a/src/adapter/mod.rs"; expect "a pub(crate) fn taking a library type" 1
mk; printf 'use libx::{Req, Resp as R2};\npub fn make() -> R2 { todo!() }\n' > "$tmp/t/crates/a/src/adapter/mod.rs"; expect "a pub fn returning an aliased library type" 1
mk; printf 'pub use libx::Req;\n' > "$tmp/t/crates/a/src/adapter/mod.rs"; expect "a pub use re-export" 1
mk; printf 'use libx::Req;\nfn private(r: Req) {}\npub(crate) fn parse(line: &[u8]) -> Result<u8, u8> { Ok(0) }\n' > "$tmp/t/crates/a/src/adapter/mod.rs"; expect "private use of the library and a clean pub(crate) fn" 0
mk; printf '[dependencies.libx]\nversion = "1"\n' > "$tmp/t/crates/b/Cargo.toml"; expect "table form in another crate" 1
mk; printf '[dependencies]\nfoo = { package = "libx", version = "1" }\n' > "$tmp/t/crates/b/Cargo.toml"; expect "package rename in another crate" 1
mk; printf '[dependencies]\nfoo = { package = "libx", version = "1" }\n' > "$tmp/t/Cargo.toml"; expect "package rename in the workspace root" 1
mk; printf '[dependencies]\nlibx = "1"\n' > "$tmp/t/crates/b/Cargo.toml"; sed -i 's/libx/libx_y/' "$tmp/t/crates/b/Cargo.toml"; expect "a different crate with a similar name" 0
mk; printf 'lib_x = "1"\n' > "$tmp/t/crates/b/Cargo.toml"; printf 'lib-x crates/a/src/adapter\n' > "$tmp/t/scripts/wrapped-libs.txt"; expect "underscore key of a hyphen-named library" 1
echo "wrapped-libs gate self-test ok"
