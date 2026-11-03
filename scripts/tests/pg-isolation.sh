#!/usr/bin/env bash
# Script test (needs Docker): two pg-up runs with different run ids yield two containers and
# two URL files, and pg-down of one leaves the other alive.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
A="isoa$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
B="isob$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
fail() { echo "pg-isolation: $*" >&2; RUSTY_PG_RUN_ID=$A bash scripts/pg-down.sh >/dev/null 2>&1; RUSTY_PG_RUN_ID=$B bash scripts/pg-down.sh >/dev/null 2>&1; exit 1; }
running() { [ "$(docker inspect -f '{{.State.Running}}' "rusty-pg-test-$1" 2>/dev/null || true)" = "true" ]; }

RUSTY_PG_RUN_ID=$A bash scripts/pg-up.sh >/dev/null 2>&1 & pa=$!
RUSTY_PG_RUN_ID=$B bash scripts/pg-up.sh >/dev/null 2>&1 & pb=$!
wait "$pa" || fail "pg-up $A failed"
wait "$pb" || fail "pg-up $B failed"
running "$A" || fail "container $A is not running"
running "$B" || fail "container $B is not running"
[ -s "$HOME/.rusty-pg-test-url.$A" ] && [ -s "$HOME/.rusty-pg-test-url.$B" ] || fail "missing URL file"
[ "$(cat "$HOME/.rusty-pg-test-url.$A")" != "$(cat "$HOME/.rusty-pg-test-url.$B")" ] || fail "URL files are identical"
RUSTY_PG_RUN_ID=$A bash scripts/pg-down.sh >/dev/null 2>&1
running "$A" && fail "container $A survived its own pg-down"
running "$B" || fail "pg-down of $A removed container $B"
[ -s "$HOME/.rusty-pg-test-url.$B" ] || fail "pg-down of $A removed the URL file of $B"
RUSTY_PG_RUN_ID=$B bash scripts/pg-down.sh >/dev/null 2>&1
running "$B" && fail "container $B survived its own pg-down"
echo "pg-isolation: OK"
