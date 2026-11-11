#!/usr/bin/env bash
# Self-test of scripts/prepush-check.sh in scratch repositories: a clean history passes and each
# kind of finding (identity, trailer, size, key, token, password, workstation path, key-named file,
# missing origin, refs/original) fails by itself; allowlist entries are explicit and honoured.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
fail() { echo "prepush-check self-test: $*" >&2; exit 1; }
GOOD='Tester <tester@example.invalid>'

mk() { # fresh repo with the checker, the identity list, an empty allowlist, origin and one commit
  rm -rf "$tmp/r"; mkdir -p "$tmp/r/scripts"; cd "$tmp/r"
  git init -q -b main .
  git config user.name Tester; git config user.email tester@example.invalid; git config core.autocrlf false
  git remote add origin https://example.invalid/x.git
  cp "$here/scripts/prepush-check.sh" scripts/
  printf '# ids\n%s\n' "$GOOD" > scripts/prepush-identity.txt
  printf '# the checker holds its own patterns\nprivkey scripts/prepush-check.sh\npassword scripts/prepush-check.sh\nworkpath scripts/prepush-check.sh\n' > scripts/prepush-allow.txt
  echo hello > a.txt
  git add -A; git commit -q -m "feat: base"
}
run() { bash scripts/prepush-check.sh "$@" >"$tmp/out" 2>&1; }
expect_pass() { run "$@" || { cat "$tmp/out" >&2; fail "expected PASS ($*)"; }; }
expect_fail() { # reason-substring
  local want="$1"; shift
  if run "$@"; then cat "$tmp/out" >&2; fail "expected FAIL for $want"; fi
  grep -q "^\[FAIL\].*$want" "$tmp/out" || { cat "$tmp/out" >&2; fail "FAIL did not name '$want'"; }
}
add() { # path content message
  mkdir -p "$(dirname "$1")"; printf '%s\n' "$2" > "$1"; git add -A; git commit -q -m "${3:-chore: add}"
}
# strings are assembled so this file itself holds no literal secret or workstation path
AWS="AKIA$(printf 'ABCDEFGHIJKLMNOP')"
GH="ghp_$(printf 'a%.0s' $(seq 1 36))"
PK="-----BEGIN $(printf 'RSA') PRIVATE KEY-----"
WP="C:$(printf '\')Users$(printf '\')someone"

mk; expect_pass; grep -q 'PASS' "$tmp/out" || fail "no PASS line"
mk; git config user.email intruder@example.invalid; add b.txt b "feat: other identity"; expect_fail "identity"
mk; git -c user.name=Tester -c user.email=tester@example.invalid commit -q --allow-empty -m "$(printf 'feat: x\n\nCo-Authored-By: A <a@example.invalid>')"; expect_fail "Co-Authored-By"
mk; git commit -q --allow-empty -m "$(printf 'feat: x\n\nGenerated with a tool')"; expect_fail "Co-Authored-By"
mk; head -c $((5 * 1024 * 1024 + 1)) /dev/zero > big.bin; git add -A; git commit -q -m "feat: big"; expect_fail "5 MiB"
mk; add k.txt "$PK" "feat: key"; expect_fail "private-key"
mk; add k.txt "$PK" "feat: key"; printf 'privkey k.txt\n' >> scripts/prepush-allow.txt; expect_pass
mk; add k.txt "$PK" "feat: key"; printf 'privkey other.txt\n' >> scripts/prepush-allow.txt; expect_fail "private-key"
mk; add f/k.txt "$PK" "feat: key"; printf 'privkey f/\n' >> scripts/prepush-allow.txt; expect_pass   # explicit directory
mk; add t.txt "key $AWS" "feat: aws"; expect_fail "token"
mk; add t.txt "$GH" "feat: gh"; expect_fail "token"
mk; add c.rs 'let password = "hunter2";' "feat: pw"; expect_fail "password"
mk; add c.sh 'PASSWORD="$(generate)"' "feat: pw var"; expect_pass
mk; add d.md "see $WP" "docs: path"; expect_fail "workstation"
mk; add d.md "see /home/someone/x" "docs: path"; expect_fail "workstation"
mk; add d.md "see /home/someone/x" "docs: path"; printf 'workpath d.md\n' >> scripts/prepush-allow.txt; expect_pass
mk; add id_ed25519 "x" "feat: k"; expect_fail "named like"
mk; add .env "A=1" "feat: env"; expect_fail "named like"
mk; add ex.env.example "A=1" "docs: example"; expect_pass
mk; add .rusty-remote.known_hosts "h k" "feat: kh"; expect_fail "named like"
mk; echo x > cert.pem; expect_fail "named like"                   # untracked, not ignored
mk; echo x > cert.pem; echo '*.pem' > .gitignore; git add .gitignore; git commit -q -m "chore: ignore"; expect_pass   # ignored: fine
mk; echo x > staged.key; git add staged.key; expect_fail "named like"   # staged
mk; git remote remove origin; expect_fail "origin"
mk; git update-ref refs/original/refs/heads/main HEAD; expect_fail "refs/original"
# range argument: only the commits in it are checked
mk; git tag base; git config user.email intruder@example.invalid; add b.txt b "feat: other identity"
expect_pass base..base; expect_fail "identity" base..HEAD
# a secret is never printed
mk; add t.txt "$GH" "feat: gh"; run || true; ! grep -q "$GH" "$tmp/out" || fail "a token value was printed"
echo "prepush-check self-test OK"
