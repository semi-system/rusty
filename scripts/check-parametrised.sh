#!/usr/bin/env bash
# Gate: every behaviour test of the database crates runs on BOTH backends, or says why not.
# Usage: bash scripts/check-parametrised.sh [ROOT]
#
# Scans crates/rusty-store/tests/*.rs, crates/rusty-directory/tests/*.rs and, since U1, the service crates
# crates/rusty-svc/tests/*.rs, crates/rusty-ctl/tests/*.rs, crates/rusty-smtp/tests/*.rs and crates/rusty/tests/*.rs (not common/; a service test that does not touch a
# database says so with a marker, one that builds a Context over a store names why the backend is irrelevant). Every
# `#[test]` / `#[tokio::test]` function (also inside `proptest!`) must pass ONE of
#   * RUNNER: its body, as CODE, calls a harness iterator `for_each_backend*` / `for_each_env*`
#     whose closure parameter is named and used (not `|_|` / `|_b|`), or iterates `backends()`;
#     or it calls a function of the same file that does (a shared helper);
#   * MARKER: its OWN `// backend-neutral: <reason>` comment, within the three lines above its
#     attribute or between the attribute and the `fn` line. The reason must be a real sentence
#     (at least 12 characters and three words). There are NO file-level exemptions: a
#     `//! backend-neutral:` header is documentation only, and a future test in the same file
#     needs its own marker.
#
# The check reads real code: comments, string literals (also raw strings) and char literals are
# blanked before looking for the iterators, so `// for_each_backend` in a comment or `"backends()"`
# in a string does not satisfy it; and a marker counts only when it is in a real comment, not in a
# string. Function bodies are found by brace counting on the blanked code, so any layout works.
#
# What it cannot see: that a for_each_* closure runs the assertions that matter (it only proves the
# closure uses its backend argument), nor that a marker's reason is true; the reasons are reviewed.
set -euo pipefail
ROOT="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"

files="$(ls crates/rusty-store/tests/*.rs crates/rusty-directory/tests/*.rs crates/rusty-svc/tests/*.rs crates/rusty-imap/tests/*.rs crates/rusty-ctl/tests/*.rs crates/rusty-smtp/tests/*.rs crates/rusty/tests/*.rs 2>/dev/null | sort || true)"
[ -n "$files" ] || { echo "check-parametrised: no test files found under $ROOT" >&2; exit 2; }

total=0; param=0; marked=0; bad=""
for f in $files; do
  out="$(awk -v FILE="$f" '
    # ---- lexer: code[i] has comments and literals blanked, com[i] the comment text of line i
    function lex(   i, n, s, c, c2, j, k, line, code, com, hashes, closing, ok) {
      mode = 0; depth = 0; hashes = 0
      for (i = 1; i <= NR; i++) {
        s = src[i]; n = length(s); code = ""; com = ""; j = 1
        while (j <= n) {
          c = substr(s, j, 1); c2 = substr(s, j, 2)
          if (mode == 0) {
            if (c2 == "//") { com = com substr(s, j); j = n + 1; continue }
            if (c2 == "/*") { mode = 1; depth = 1; com = com " "; j += 2; continue }
            if (c == "\"") { mode = 2; code = code "\"\""; j++; continue }
            if (c == "r" && (j == 1 || substr(s, j - 1, 1) !~ /[A-Za-z0-9_]/)) {
              k = j + 1; hashes = 0
              while (substr(s, k, 1) == "#") { hashes++; k++ }
              if (substr(s, k, 1) == "\"") { mode = 3; code = code "\"\""; j = k + 1; continue }
            }
            if (c == "\x27") {
              if (substr(s, j + 1, 1) == "\\") { k = index(substr(s, j + 3), "\x27"); if (k > 0) { j = j + 3 + k; code = code "_"; continue } }
              else if (substr(s, j + 2, 1) == "\x27") { j += 3; code = code "_"; continue }
            }
            code = code c; j++
          } else if (mode == 1) {
            if (c2 == "/*") { depth++; j += 2 }
            else if (c2 == "*/") { depth--; j += 2; if (depth == 0) mode = 0 }
            else { com = com c; j++ }
          } else if (mode == 2) {
            if (c == "\\") j += 2
            else if (c == "\"") { mode = 0; j++ }
            else j++
          } else {
            closing = "\""; for (k = 0; k < hashes; k++) closing = closing "#"
            if (substr(s, j, length(closing)) == closing) { mode = 0; j += length(closing) }
            else j++
          }
        }
        cd[i] = code; cm[i] = com
      }
    }
    function indent_of(s,   m) { match(s, /^[ \t]*/); return substr(s, 1, RLENGTH) }
    function count_char(s, ch,   t) { t = s; return gsub(ch, "", t) }
    { src[NR] = $0 }
    END {
      n = NR; lex()
      # pass 1: functions and their extents (brace counting on blanked code)
      nf = 0
      for (i = 1; i <= n; i++) {
        if (cd[i] ~ /^[ \t]*(pub(\([a-z]+\))? )?(async )?fn [A-Za-z_0-9]+/) {
          s = cd[i]; sub(/^[ \t]*(pub(\([a-z]+\))? )?(async )?fn /, "", s); name = s; sub(/[^A-Za-z_0-9].*$/, "", name)
          bal = 0; seen = 0; end = n
          for (j = i; j <= n; j++) {
            t = cd[j]
            o = count_char(t, "[{]"); c = count_char(t, "[}]")
            if (o > 0) seen = 1
            bal += o - c
            if (seen && bal <= 0) { end = j; break }
          }
          nf++; fname[nf] = name; fstart[nf] = i; fend[nf] = end
          body = ""
          for (j = i; j <= end; j++) body = body "\n" cd[j]
          fbody[nf] = body
          if (is_runner(body)) runner[name] = 1
        }
      }
      # pass 2: tests
      for (i = 1; i <= n; i++) {
        if (cd[i] !~ /^[ \t]*#\[(tokio::)?test/) continue
        k = 0
        for (m = 1; m <= nf; m++) if (fstart[m] >= i) { k = m; break }
        if (k == 0) continue
        ok = 0; why = ""; note = ""
        if (is_runner(fbody[k])) { ok = 1; why = "runner" }
        else if (uses_runner_without_arg(fbody[k])) note = "the for_each closure does not use its backend argument"
        if (!ok && note == "") for (h in runner) if (h != fname[k] && index(fbody[k], h "(") > 0) { ok = 1; why = "runner"; break }
        mk = 0; reason = ""
        lo = i - 3; if (lo < 1) lo = 1
        for (j = lo; j <= fstart[k]; j++) {
          p = index(cm[j], "backend-neutral:")
          if (p > 0 && cm[j] !~ /^\/\/!/) {
            mk = 1; reason = substr(cm[j], p + 16); gsub(/^[ \t]+|[ \t]+$/, "", reason)
          }
        }
        if (!ok && mk) {
          nw = split(reason, w, /[ \t]+/)
          if (length(reason) >= 12 && nw >= 3) { ok = 1; why = "marked" }
          else note = "the backend-neutral reason is not a real sentence"
        }
        if (ok && why == "runner") print "P"
        else if (ok) print "M"
        else print "X " i " " fname[k] " " note
      }
    }
    function is_runner(body,   t, nm, rest, cnt, names, nn, i, after) {
      if (body ~ /backends\(\)/) return 1
      t = body
      while (match(t, /for_each_(backend|env)[A-Za-z_0-9]*\(/)) {
        rest = substr(t, RSTART + RLENGTH)
        # optional leading arguments (a sample size), then the closure `|a, b|`
        if (rest ~ /^[ \t\n]*([A-Za-z0-9_]+[ \t]*,[ \t\n]*)*(async )?(move )?\|[ \t]*[A-Za-z_][A-Za-z0-9_, \t]*\|/) {
          nm = rest; sub(/^[ \t\n]*([A-Za-z0-9_]+[ \t]*,[ \t\n]*)*(async )?(move )?\|[ \t]*/, "", nm)
          after = nm; sub(/^[^|]*\|/, "", after); sub(/[ \t]*\|.*$/, "", nm)
          nn = split(nm, names, /[ \t]*,[ \t]*/)
          for (i = 1; i <= nn; i++) {
            if (names[i] ~ /^_/ || names[i] == "") continue
            if (count_word(after, names[i]) >= 1) return 1
          }
        }
        t = rest
      }
      return 0
    }
    function uses_runner_without_arg(body) { return body ~ /for_each_(backend|env)[A-Za-z_0-9]*\(/ }
    function count_word(s, wd,   c, t, re) {
      c = 0; t = s; re = "(^|[^A-Za-z0-9_])" wd "([^A-Za-z0-9_]|$)"
      while (match(t, re)) { c++; t = substr(t, RSTART + RLENGTH - 1) }
      return c
    }' "$f")"
  while IFS= read -r l; do
    [ -n "$l" ] || continue
    total=$((total + 1))
    case "$l" in
      P) param=$((param + 1)) ;;
      M) marked=$((marked + 1)) ;;
      X\ *) set -- $l; shift; ln="$1"; nm="$2"; shift 2; bad="$bad$f:$ln: $nm ${*:+($*)}"$'\n' ;;
    esac
  done <<<"$out"
done

if [ -n "$bad" ]; then
  echo "tests that neither run on both backends nor carry their own '// backend-neutral: <reason>' marker:" >&2
  printf '%s' "$bad" >&2
  echo "check-parametrised: $(printf '%s' "$bad" | grep -c .) unmarked of $total tests ($param parametrised, $marked marked)" >&2
  exit 1
fi
echo "check-parametrised: $total tests: $param run on both backends, $marked marked backend-neutral, 0 unmarked"
