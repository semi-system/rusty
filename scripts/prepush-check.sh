#!/usr/bin/env bash
# Pre-push check: run it before `git push`. Read-only; never contacts a remote.
#   bash scripts/prepush-check.sh              commits not yet on any origin/* ref (all history if none)
#   bash scripts/prepush-check.sh REV_RANGE    e.g. origin/main..HEAD
# Checks (each prints PASS or FAIL; the exit status is 1 when anything fails):
#   1. a remote named origin exists
#   2. author and committer name+email of every commit are listed in scripts/prepush-identity.txt
#   3. no commit message has a Co-Authored-By trailer or a "Generated with" line
#   4. no blob in the range is larger than 5 MiB
#   5. no private-key material (BEGIN ... PRIVATE KEY), cloud or GitHub token, `password = "..."`
#      literal, or workstation path (C:\Users, D:\code, /home/) in an ADDED line, except at the
#      explicit paths listed per kind in scripts/prepush-allow.txt
#   6. no file in the range, in the index or untracked (not ignored) is named like a key, .env or
#      known_hosts file (unless listed under `file` in scripts/prepush-allow.txt)
#   7. no refs/original (leftover of a history rewrite)
# Secret values are never printed: findings show the kind, the commit and path:line only.
set -uo pipefail
top="$(git rev-parse --show-toplevel 2>/dev/null)" || { echo "prepush-check: not inside a git repository" >&2; exit 2; }
cd "$top" || exit 2
ID_FILE="${PREPUSH_IDENTITY_FILE:-scripts/prepush-identity.txt}"
ALLOW_FILE="${PREPUSH_ALLOW_FILE:-scripts/prepush-allow.txt}"
MAX_BYTES=$((5 * 1024 * 1024))

tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
fails=0
pass() { echo "[PASS] $1"; }
fail() { echo "[FAIL] $1"; fails=$((fails + 1)); }
detail() { sed "s/^/         /" | head -n "${1:-20}"; }

# --- range
if [ "$#" -ge 1 ]; then
  range=("$1"); label="$1"
elif git for-each-ref --count=1 refs/remotes/origin | grep -q .; then
  range=(HEAD --not --remotes=origin); label="HEAD not on origin/*"
else
  range=(HEAD); label="HEAD (no origin/* refs: the whole history)"
fi
if ! git rev-list "${range[@]}" > "$tmp/commits" 2>"$tmp/err"; then
  echo "prepush-check: bad range: $(cat "$tmp/err")" >&2; exit 2
fi
ncommits="$(wc -l < "$tmp/commits" | tr -d ' ')"
echo "prepush-check: range $label, $ncommits commit(s)"

# --- 1 origin
if url="$(git remote get-url origin 2>/dev/null)" && [ -n "$url" ]; then pass "remote origin is set ($url)"; else fail "no remote named origin"; fi

# --- 2 identity
if [ ! -s "$ID_FILE" ]; then
  fail "identity allowlist $ID_FILE is missing or empty"
else
  grep -vE '^[[:space:]]*(#|$)' "$ID_FILE" > "$tmp/ids"
  git log --format='%h%x09%an <%ae>%x09%cn <%ce>' "${range[@]}" 2>/dev/null |
    awk -F'\t' 'NR == FNR { ok[$0] = 1; next } { if (!($2 in ok)) print $1 " author " $2; if (!($3 in ok)) print $1 " committer " $3 }' "$tmp/ids" - > "$tmp/badid"
  if [ -s "$tmp/badid" ]; then fail "commit identity not in $ID_FILE"; sort -u "$tmp/badid" | detail; else pass "all authors and committers are in $ID_FILE"; fi
fi

# --- 3 messages
git log --format='@@C %h %s%n%b' "${range[@]}" 2>/dev/null | awk '
  /^@@C / { c = $2 " " substr($0, index($0, $3)); next }
  { l = tolower($0); if (l ~ /^[ \t]*co-authored-by:/ || l ~ /generated with/) hit[c] = 1 }
  END { for (k in hit) print k }' > "$tmp/badmsg"
if [ -s "$tmp/badmsg" ]; then fail "Co-Authored-By or 'Generated with' in commit messages"; detail < "$tmp/badmsg"; else pass "no Co-Authored-By / Generated-with lines"; fi

# --- 4 size
git rev-list --objects "${range[@]}" 2>/dev/null | git cat-file --batch-check='%(objecttype) %(objectsize) %(rest)' 2>/dev/null |
  awk -v max="$MAX_BYTES" '$1 == "blob" && $2 > max { printf "%s bytes %s\n", $2, $3 }' > "$tmp/big"
if [ -s "$tmp/big" ]; then fail "files over 5 MiB"; detail < "$tmp/big"; else pass "no file over 5 MiB"; fi

# --- allowlist lookup: ALLOW <kind> <path>; an entry ending in / is an explicit directory
allowed() { # kind path
  [ -f "$ALLOW_FILE" ] || return 1
  awk -v k="$1" -v p="$2" '
    /^[[:space:]]*(#|$)/ { next }
    $1 == k { e = $2; if (e == p) f = 1; else if (substr(e, length(e)) == "/" && index(p, e) == 1) f = 1 }
    END { exit f ? 0 : 1 }' "$ALLOW_FILE"
}

# --- 5 content of added lines: "commit<TAB>path<TAB>line<TAB>text"
git log --no-renames --unified=0 --format='@@COMMIT %h' -p "${range[@]}" 2>/dev/null | awk '
  /^@@COMMIT / { commit = $2; next }
  /^\+\+\+ / { p = substr($0, 7); if ($0 == "+++ /dev/null") p = ""; next }
  /^--- / { next }
  /^@@ / { if (match($0, /\+[0-9]+/)) ln = substr($0, RSTART + 1, RLENGTH - 1) + 0; next }
  /^\+/ { if (p != "") printf "%s\t%s\t%d\t%s\n", commit, p, ln, substr($0, 2); ln++ }
' > "$tmp/added"
scan() { # kind label PCRE
  local kind="$1" label="$2" re="$3" n=0
  : > "$tmp/hits-$kind"
  grep -P "^[^\t]*\t[^\t]*\t[^\t]*\t.*(${re})" "$tmp/added" 2>/dev/null | cut -f1-3 | sort -u |
    while IFS=$'\t' read -r commit path line; do
      allowed "$kind" "$path" || printf '%s %s:%s\n' "$commit" "$path" "$line"
    done > "$tmp/hits-$kind"
  n="$(wc -l < "$tmp/hits-$kind" | tr -d ' ')"
  if [ "$n" -gt 0 ]; then fail "$label ($n)"; detail 25 < "$tmp/hits-$kind"; else pass "no $label"; fi
}
scan privkey "private-key material outside allowlisted fixtures" 'BEGIN [A-Z ]*PRIVATE KEY'
scan secret "AWS or GitHub token patterns" '\b(AKIA|ASIA)[0-9A-Z]{16}\b|\bgh[pousr]_[A-Za-z0-9]{36,}\b|\bgithub_pat_[A-Za-z0-9_]{22,}\b'
scan password 'password = "..." literals' '(?i)password\s*=\s*"[^"$`]+"'
scan workpath "workstation paths (C:\\Users, D:\\code, /home/)" 'C:\\Users|C:/Users|D:\\code|D:/code|/home/'

# --- 6 key/env/known_hosts names
name_bad() { # path -> 0 when the basename looks like a key / env / known_hosts file
  local b="${1##*/}"
  case "$b" in *.example|*.sample|*.pub|*.template) return 1 ;; esac
  case "$b" in
    *.pem|*.key|*.ppk|*.p12|*.pfx|*.jks|id_rsa*|id_ed25519*|id_ecdsa*|id_dsa*|privkey|privkey.*) return 0 ;;
    .env|.env.*|*.env|*known_hosts*|credentials|credentials.*|.netrc|.npmrc|.pgpass) return 0 ;;
  esac
  return 1
}
{
  git log --no-renames --name-only --format= "${range[@]}" 2>/dev/null | sed 's/^/range\t/'
  git diff --cached --name-only 2>/dev/null | sed 's/^/staged\t/'
  git ls-files --others --exclude-standard 2>/dev/null | sed 's/^/untracked\t/'
} | sort -u > "$tmp/names"
: > "$tmp/badnames"
while IFS=$'\t' read -r where path; do
  [ -n "$path" ] || continue
  if name_bad "$path" && ! allowed file "$path"; then echo "$where $path" >> "$tmp/badnames"; fi
done < "$tmp/names"
if [ -s "$tmp/badnames" ]; then fail "files named like keys / env / known_hosts"; detail < "$tmp/badnames"; else pass "no key/env/known_hosts-named files (range, index, untracked)"; fi

# --- 7 refs/original
if git for-each-ref --count=1 refs/original | grep -q .; then
  fail "refs/original exists (leftover of a history rewrite: git for-each-ref refs/original | cut -d' ' -f3 | xargs -n1 git update-ref -d)"
else
  pass "no refs/original"
fi

if [ "$fails" -eq 0 ]; then echo "prepush-check: PASS"; exit 0; fi
echo "prepush-check: FAIL ($fails check(s))"
exit 1
