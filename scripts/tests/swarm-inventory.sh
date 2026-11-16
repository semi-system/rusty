#!/usr/bin/env bash
# The inventory checks of deploy/swarm/rusty-swarm need no Docker: a bad inventory is refused before any node is touched, a good one is accepted up to the first node call.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
tool=deploy/swarm/rusty-swarm
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

expect_refused() { # description, expected text, inventory body
  local f="$tmp/inv.env" out
  printf '%s\n' "$3" > "$f"
  if out="$("$tool" --env "$f" cluster check 2>&1)"; then echo "swarm-inventory: '$1' was accepted" >&2; exit 1; fi
  case "$out" in *"$2"*) ;; *) echo "swarm-inventory: '$1': expected '$2', got: $out" >&2; exit 1 ;; esac
}

expect_refused "no nodes" "no NODES" 'NODES=()'
expect_refused "no manager" "no manager" 'NODES=("a|docker:a|worker||")'
expect_refused "two managers" "more than one manager" 'NODES=("a|docker:a|manager||" "b|docker:b|manager||")'
expect_refused "a duplicate name" "appears twice" 'NODES=("a|docker:a|manager||" "a|docker:b|worker||")'
expect_refused "a bad transport" "transport" 'NODES=("a|telnet:a|manager||")'
expect_refused "a bad role" "role is manager or worker" 'NODES=("a|docker:a|boss||")'
expect_refused "a bad name" "node name" 'NODES=("A_1|docker:a|manager||")'
expect_refused "too few fields" "five fields" 'NODES=("a|docker:a|manager")'
if "$tool" --env "$tmp/missing.env" cluster check >/dev/null 2>&1; then echo "swarm-inventory: a missing file was accepted" >&2; exit 1; fi
# The examples shipped with the tool are valid inventories: they get as far as reaching for a node (which does not exist here).
for f in deploy/swarm/local.env deploy/swarm/swarm.env.example; do
  out="$("$tool" --env "$f" cluster check 2>&1 || true)"
  case "$out" in *"no inventory"*|*"has no NODES"*|*"five fields"*|*"node name"*|*"appears twice"*|*"is ssh:USER"*|*"role is manager"*|*"no manager"*|*"more than one manager"*) echo "swarm-inventory: $f is refused: $out" >&2; exit 1 ;; esac
done
echo "swarm-inventory OK"
