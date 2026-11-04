#!/usr/bin/env bash
# Gate: every behaviour test of the database crates runs on BOTH backends, or says why not.
# Usage: bash scripts/check-parametrised.sh [ROOT]
#
# Scans crates/rusty-store/tests/*.rs and crates/rusty-directory/tests/*.rs (not common/). For
# every `#[test]` / `#[tokio::test]` function (also inside `proptest!`), the test passes when
#   * its body mentions `for_each_backend`, `for_each_env` (any `for_each_env*` / `for_each_backend*`
#     variant) or `backends()` (the harness iterators that run a closure once per backend), or
#   * its body calls a function of the same file whose body does (a shared helper), or
#   * a `// backend-neutral: <reason>` comment sits within the three lines above its attribute or
#     between the attribute and the `fn` line, or the file carries `//! backend-neutral: <reason>`
#     (Postgres-only suites, SQLite-only schema checks, pure unit tests).
# The reason is mandatory: an empty `backend-neutral:` marker does not count.
#
# What it cannot see: that a for_each_* closure really runs the assertions that matter; it only
# proves the test is wired to the per-backend runner or explicitly exempted with a reason.
# Function bodies end at the first line equal to the fn's indentation followed by `}` (rustfmt
# layout); a one-line `fn x() { .. }` is handled too.
set -euo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"

files="$(ls crates/rusty-store/tests/*.rs crates/rusty-directory/tests/*.rs 2>/dev/null | sort || true)"
[ -n "$files" ] || { echo "check-parametrised: no test files found under $ROOT" >&2; exit 2; }

total=0; param=0; marked=0; bad=""
for f in $files; do
  out="$(awk -v FILE="$f" '
    function indent_of(s,   m) { match(s, /^[ \t]*/); return substr(s, 1, RLENGTH) }
    { line[NR] = $0 }
    END {
      n = NR
      filemark = 0
      for (i = 1; i <= n; i++) if (line[i] ~ /^\/\/! backend-neutral: *[^ ]/) filemark = 1
      # pass 1: functions, their end lines, and which of them drive the per-backend runner
      nf = 0
      for (i = 1; i <= n; i++) {
        if (line[i] ~ /^[ \t]*(pub(\([a-z]+\))? )?(async )?fn [A-Za-z_0-9]+/) {
          s = line[i]; sub(/^[ \t]*(pub(\([a-z]+\))? )?(async )?fn /, "", s); name = s; sub(/[^A-Za-z_0-9].*$/, "", name)
          ind = indent_of(line[i]); end = i
          if (!(line[i] ~ /\{.*\}[ \t]*$/)) {
            for (j = i + 1; j <= n; j++) if (line[j] == ind "}") { end = j; break }
            if (end == i) end = n
          }
          nf++; fname[nf] = name; fstart[nf] = i; fend[nf] = end
          body = ""
          for (j = i; j <= end; j++) body = body "\n" line[j]
          fbody[nf] = body
          if (body ~ /for_each_(backend|env)[A-Za-z_0-9]*|backends\(\)/) { runner[name] = 1 }
        }
      }
      # pass 2: tests
      for (i = 1; i <= n; i++) {
        if (line[i] !~ /^[ \t]*#\[(tokio::)?test/) continue
        # the fn this attribute belongs to: the next fn line
        k = 0
        for (m = 1; m <= nf; m++) if (fstart[m] >= i) { k = m; break }
        if (k == 0) continue
        total++
        ok = 0; why = ""
        if (fbody[k] ~ /for_each_(backend|env)[A-Za-z_0-9]*|backends\(\)/) { ok = 1; why = "runner" }
        if (!ok) for (h in runner) if (h != fname[k] && index(fbody[k], h "(") > 0) { ok = 1; why = "runner"; break }
        mk = 0
        lo = i - 3; if (lo < 1) lo = 1
        for (j = lo; j <= fstart[k]; j++) if (line[j] ~ /backend-neutral: *[^ ]/) mk = 1
        if (!ok && (mk || filemark)) { ok = 1; why = "marked" }
        if (ok && why == "runner") print "P"
        else if (ok) print "M"
        else print "X " (fstart[k]) " " fname[k]
      }
    }' "$f")"
  while IFS= read -r l; do
    [ -n "$l" ] || continue
    total=$((total + 1))
    case "$l" in
      P) param=$((param + 1)) ;;
      M) marked=$((marked + 1)) ;;
      X\ *) set -- $l; bad="$bad$f:$2: $3"$'\n' ;;
    esac
  done <<<"$out"
done

if [ -n "$bad" ]; then
  echo "tests that neither run on both backends nor carry a '// backend-neutral: <reason>' marker:" >&2
  printf '%s' "$bad" >&2
  echo "check-parametrised: $(printf '%s' "$bad" | grep -c .) unmarked of $total tests ($param parametrised, $marked marked)" >&2
  exit 1
fi
echo "check-parametrised: $total tests: $param run on both backends, $marked marked backend-neutral, 0 unmarked"
