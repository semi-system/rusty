#!/usr/bin/env bash
# Gate (plan section 1b): a wrapped third-party protocol library is named only inside its adapter module, and
# nothing of it leaves the adapter through a public item.
# Usage: bash scripts/check-wrapped-libs.sh [ROOT]
# Config: scripts/wrapped-libs.txt, one "<crate> <adapter dir>" per line. For each library the gate fails when
#  1. its name (Rust form with underscores or Cargo form with hyphens) appears in any .rs file under crates/,
#     tools/ or fuzz/ outside the adapter directory (comments included: a false positive is preferred to a miss);
#  2. a Cargo.toml under crates/, tools/ or fuzz/ names it outside the manifest of the crate that owns the adapter:
#     inline key (hyphen or underscore form), table form `[dependencies.<lib>]`; or any manifest (the workspace
#     root too) renames it with `package = "<lib>"` (a rename changes the identifier used in the sources, which
#     check 1 could then not see);
#  3. inside the adapter, a `pub` item (any pub(...) visibility) re-exports, aliases or mentions the library or an
#     identifier imported from it: `pub type X = lib::Req`, `pub use`, a `pub fn` taking or returning a library
#     type. Only private items and `pub(crate) fn` signatures made of Rusty-owned types may cross.
# What it cannot see: a library type reached through a trait method of a Rusty-owned type, or macro-generated items.
set -euo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"
cfg=scripts/wrapped-libs.txt
[ -f "$cfg" ] || { echo "check-wrapped-libs: $cfg is missing" >&2; exit 2; }
fail=0
while read -r lib adapter _; do
  case "${lib:-}" in ''|'#'*) continue ;; esac
  [ -n "${adapter:-}" ] || { echo "check-wrapped-libs: line for '$lib' has no adapter dir" >&2; exit 2; }
  us="${lib//-/_}"
  owner="${adapter%%/src/*}"
  name_re="(${us}|${lib})"

  # 1. sources outside the adapter
  hits="$(grep -rnE "(^|[^A-Za-z0-9_])${name_re}([^A-Za-z0-9_]|$)" crates tools fuzz --include='*.rs' 2>/dev/null | grep -v "^${adapter}/" | grep -v '/target/' || true)"
  if [ -n "$hits" ]; then echo "wrapped library '$lib' used outside $adapter:" >&2; printf '%s\n' "$hits" >&2; fail=1; fi

  # 2. manifests
  mh="$(grep -rnE "^[[:space:]]*${name_re}[[:space:]]*(=|\.)" --include=Cargo.toml crates tools fuzz 2>/dev/null | grep -v "^${owner}/Cargo.toml:" | grep -v '/target/' || true)"
  th="$(grep -rnE "^[[:space:]]*\[[^]]*dependencies\.${name_re}\]" --include=Cargo.toml crates tools fuzz 2>/dev/null | grep -v "^${owner}/Cargo.toml:" | grep -v '/target/' || true)"
  ph="$(grep -rnE "package[[:space:]]*=[[:space:]]*\"${name_re}\"" --include=Cargo.toml . 2>/dev/null | grep -v '/target/' || true)"
  for x in "$mh" "$th" "$ph"; do
    if [ -n "$x" ]; then echo "wrapped library '$lib' declared or renamed outside $owner/Cargo.toml:" >&2; printf '%s\n' "$x" >&2; fail=1; fi
  done

  # 3. public items of the adapter
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    bad="$(awk -v lib="$lib" -v us="$us" '
      # identifiers imported from the library (single or braced, multi-line `use` statements)
      function collect(stmt,   t, n, i, parts) {
        t = stmt
        sub(/^[[:space:]]*(pub(\([a-z]+\))?[[:space:]]+)?use[[:space:]]+[A-Za-z0-9_]+::/, "", t)
        gsub(/[{};]/, " ", t); gsub(/,/, " ", t); gsub(/[A-Za-z0-9_]+::/, " ", t)
        n = split(t, parts, /[[:space:]]+/)
        for (i = 1; i <= n; i++) if (parts[i] ~ /^[A-Za-z_][A-Za-z0-9_]*$/ && parts[i] != "as" && parts[i] != "self") ids[parts[i]] = 1
      }
      { lines[NR] = $0 }
      END {
        stmt = ""; collecting = 0
        for (i = 1; i <= NR; i++) {
          l = lines[i]
          if (!collecting && l ~ ("^[[:space:]]*(pub(\\([a-z]+\\))?[[:space:]]+)?use[[:space:]]+(" lib "|" us ")(::|[[:space:]])")) { collecting = 1; stmt = "" }
          if (collecting) { stmt = stmt " " l; if (l ~ /;/) { collect(stmt); collecting = 0 } }
        }
        for (i = 1; i <= NR; i++) {
          l = lines[i]
          # a glob import of the library collects no identifiers, so it is refused outright
          if (l ~ ("^[[:space:]]*(pub([(][a-z]+[)])?[[:space:]]+)?use[[:space:]]+(" lib "|" us ")::.*[*]")) { print FILENAME ":" i ": " l; continue }
          if (l !~ /^[[:space:]]*pub/) continue
          # the whole item head (a signature may span lines) up to its `{` or `;`
          st = l; j = i
          while (st !~ /[{;]/ && j < NR) { j++; st = st " " lines[j] }
          # a pub use / pub type is a re-export or alias: never allowed in an adapter
          if (l ~ /^[[:space:]]*pub(\([a-z]+\))?[[:space:]]+(use|type)[[:space:]]/) { print FILENAME ":" i ": " l; continue }
          if (index(st, lib) > 0 || index(st, us) > 0) { print FILENAME ":" i ": " l; continue }
          for (id in ids) if (st ~ ("(^|[^A-Za-z0-9_])" id "([^A-Za-z0-9_]|$)")) { print FILENAME ":" i ": " l; break }
        }
      }' "$f")"
    if [ -n "$bad" ]; then echo "adapter item exposes '$lib' (pub use/type, or a pub signature naming it):" >&2; printf '%s\n' "$bad" >&2; fail=1; fi
  done < <(find "$adapter" -name '*.rs' 2>/dev/null | sort)
done < "$cfg"
exit "$fail"
