#!/usr/bin/env bash
# Self-test of deploy/standalone/limits.sh: the 80 % budget is never exceeded, every container gets at least its minimum, replicas share the budget, --equal ignores the weights, and bad input is
# refused. The generated override is checked with `docker compose config` when Docker is there.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
L=deploy/standalone/limits.sh
fail() { echo "FAIL: $*" >&2; exit 1; }
mem_of() { awk -v n="$2" '$1 == n { print $2 }' <<< "$1"; }   # memory MiB of a container in a table
rows() { sed -n '/^container/,/^$/p' <<< "$1" | tail -n +2; }

out="$(bash $L --total-mem-mib 8192 --total-cpus 4 --profiles filter,valkey --replicas imap=2,smtp-in=3)"
t="$(rows "$out")"
budget=$((8192 * 80 / 100))
sum=0
while read -r _ mem _ reps _; do sum=$((sum + mem * reps)); done <<< "$t"
[ "$sum" -le "$budget" ] || fail "the limits add up to $sum MiB, over the budget of $budget"
[ "$sum" -ge $((budget - 40)) ] || fail "the limits use only $sum of the $budget MiB budget: the division loses memory"
[ "$(mem_of "$t" clamd)" -ge 2048 ] || fail "clamd got less than its minimum"
[ "$(mem_of "$t" db)" -ge 512 ] || fail "the database got less than its minimum"
[ "$(mem_of "$t" clamd)" -gt "$(mem_of "$t" gc)" ] || fail "the weights are ignored: the virus scanner got no more than gc"
echo "$out" | grep -q "^Total: 8192 MiB and 4 CPUs" || fail "the total line is wrong"

# Replicas: more replicas of a service leave less for each (and for the others).
one="$(rows "$(bash $L --total-mem-mib 8192 --total-cpus 4 --replicas imap=1)")"
two="$(rows "$(bash $L --total-mem-mib 8192 --total-cpus 4 --replicas imap=2)")"
[ "$(mem_of "$two" imap)" -lt "$(mem_of "$one" imap)" ] || fail "two replicas of imap did not get less each than one"

# A bigger share of the machine gives bigger limits.
p100="$(rows "$(bash $L --total-mem-mib 8192 --total-cpus 4 --percent 100)")"
p50="$(rows "$(bash $L --total-mem-mib 8192 --total-cpus 4 --percent 50)")"
[ "$(mem_of "$p100" db)" -gt "$(mem_of "$p50" db)" ] || fail "--percent has no effect"

# --equal: the extra over the minimum is the same for everyone (db minimum 512, edge 64).
eq="$(rows "$(bash $L --total-mem-mib 8192 --total-cpus 4 --equal)")"
d=$(( $(mem_of "$eq" db) - 512 )); e=$(( $(mem_of "$eq" edge) - 64 ))
[ "${d#-}" -le $(( e + 1 )) ] && [ "${e#-}" -le $(( d + 1 )) ] || fail "--equal did not give everyone the same extra ($d and $e)"

# Refusals: too small a machine, unknown profile, a bad percentage, a bad replica list.
if bash $L --total-mem-mib 1024 --total-cpus 2 --profiles filter >/dev/null 2>&1; then fail "a 1 GiB machine was accepted for the filter profile"; fi
if bash $L --total-mem-mib 8192 --total-cpus 4 --profiles nonsense >/dev/null 2>&1; then fail "an unknown profile was accepted"; fi
if bash $L --total-mem-mib 8192 --total-cpus 4 --percent 5 >/dev/null 2>&1; then fail "5 % was accepted"; fi
if bash $L --total-mem-mib 8192 --total-cpus 4 --replicas "imap=0" >/dev/null 2>&1; then fail "zero replicas were accepted"; fi
if bash $L --total-mem-mib 8192 --total-cpus 4 --replicas "Imap=2" >/dev/null 2>&1; then fail "a bad service name was accepted"; fi

# The override is a Compose file that Compose accepts together with the stack's own.
if command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
  tmp="$(mktemp)"
  # The profiles the stack defines today (the override names only services the stack has).
  bash $L --total-mem-mib 8192 --total-cpus 4 --profiles filter,valkey --out "$tmp" >/dev/null
  ( cd deploy/standalone && RUSTY_STATE=/tmp/rusty-limits-gate COMPOSE_PROFILES=filter,valkey docker compose -f compose.yaml -f "$tmp" config --quiet ) || fail "Compose refuses the generated override"
  rm -f "$tmp"
fi
echo "limits gate OK"
