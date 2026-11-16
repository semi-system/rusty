#!/usr/bin/env bash
# Works out the memory and CPU limit of every container of the stack from what the machine (or the swarm) has: it takes the total, keeps 80 % of it for the stack (the rest is the host's, Docker's and
# the page cache's), and divides that over the containers by weight, every container first getting its minimum.
#
#   ./limits.sh [--percent 80] [--profiles filter,valkey,monitoring] [--replicas imap=2,smtp-in=3]
#               [--equal] [--out compose.limits.yaml] [--total-mem-mib N --total-cpus N]
#
# Where the totals come from: a swarm manager adds up the Ready and Active nodes (`docker node inspect`); anything else reads this machine (/proc/meminfo, nproc). `--total-mem-mib` and
# `--total-cpus` give the totals yourself (a machine you plan for, a test). Services of the profiles you name are counted; `--replicas` multiplies a service (every replica gets its own limit).
# Default weights follow what the services do (the database and the virus scanner need the most; the Rust services idle at a few MiB and need headroom for bursts); `--equal` ignores them.
#
# The output is a table and, with `--out`, a Compose override with `deploy.resources.limits` (honoured by `docker compose` and by `docker stack deploy`):
#   docker compose -f compose.yaml -f compose.limits.yaml up -d      (or set COMPOSE_FILE=compose.yaml:compose.limits.yaml in .env)
# The memory budget of the inbound listeners (`[limits] inflight_message_mib`, default 1024) must stay well under their container's limit: the table shows the value to set.
set -euo pipefail

PERCENT=80; PROFILES=""; REPLICAS=""; EQUAL=0; OUT=""; TOTAL_MEM=""; TOTAL_CPUS=""
while [ $# -gt 0 ]; do
  case "$1" in
    --percent) PERCENT="${2:-}"; shift 2 ;;
    --profiles) PROFILES="${2:-}"; shift 2 ;;
    --replicas) REPLICAS="${2:-}"; shift 2 ;;
    --equal) EQUAL=1; shift ;;
    --out) OUT="${2:-}"; shift 2 ;;
    --total-mem-mib) TOTAL_MEM="${2:-}"; shift 2 ;;
    --total-cpus) TOTAL_CPUS="${2:-}"; shift 2 ;;
    *) echo "limits: unknown argument $1 (see the header of this script)" >&2; exit 2 ;;
  esac
done
[[ "$PERCENT" =~ ^[0-9]+$ ]] && [ "$PERCENT" -ge 10 ] && [ "$PERCENT" -le 100 ] || { echo "limits: --percent is a number from 10 to 100" >&2; exit 2; }
[ -z "$TOTAL_MEM" ] || [[ "$TOTAL_MEM" =~ ^[0-9]+$ ]] || { echo "limits: --total-mem-mib is a number of MiB" >&2; exit 2; }
[ -z "$TOTAL_CPUS" ] || [[ "$TOTAL_CPUS" =~ ^[0-9]+(\.[0-9]+)?$ ]] || { echo "limits: --total-cpus is a number" >&2; exit 2; }

# ---- what the machine or the swarm has
source_of_totals="this machine"
if [ -z "$TOTAL_MEM" ] || [ -z "$TOTAL_CPUS" ]; then
  if command -v docker >/dev/null 2>&1 && [ "$(docker info --format '{{.Swarm.LocalNodeState}} {{.Swarm.ControlAvailable}}' 2>/dev/null || true)" = "active true" ]; then
    mem_b=0; nanos=0; nodes=0
    while read -r id status avail; do
      [ "$status" = "Ready" ] && [ "$avail" = "Active" ] || continue
      read -r n m < <(docker node inspect "$id" --format '{{.Description.Resources.NanoCPUs}} {{.Description.Resources.MemoryBytes}}')
      nanos=$((nanos + n)); mem_b=$((mem_b + m)); nodes=$((nodes + 1))
    done < <(docker node ls --format '{{.ID}} {{.Status}} {{.Availability}}')
    [ "$nodes" -gt 0 ] || { echo "limits: the swarm has no Ready and Active node" >&2; exit 1; }
    [ -n "$TOTAL_MEM" ] || TOTAL_MEM=$((mem_b / 1048576))
    [ -n "$TOTAL_CPUS" ] || TOTAL_CPUS="$(awk -v n="$nanos" 'BEGIN { printf "%.2f", n / 1000000000 }')"
    source_of_totals="the swarm ($nodes nodes)"
  else
    [ -n "$TOTAL_MEM" ] || TOTAL_MEM="$(awk '/^MemTotal:/ { printf "%d", $2 / 1024 }' /proc/meminfo)"
    [ -n "$TOTAL_CPUS" ] || TOTAL_CPUS="$(nproc)"
  fi
fi
[ "${TOTAL_MEM:-0}" -gt 0 ] || { echo "limits: could not read the memory of the machine" >&2; exit 1; }

# ---- the containers: name, weight (relative share), minimum MiB, listener budget note (1 = an inbound listener holding messages in memory)
#      the core stack always; a profile adds its own
SERVICES="db:40:512:0 edge:10:64:0 smtp-in:20:256:1 submission:20:256:1 imap:20:128:0 outbound:15:128:0 gc:10:64:0 admin:10:64:0"
IFS=',' read -ra PROF <<< "$PROFILES"
for p in "${PROF[@]:-}"; do
  case "$p" in
    "") ;;
    filter) SERVICES="$SERVICES clamd:60:2048:0 rspamd:15:256:0 resolver:5:64:0 freshclam:5:128:0" ;;
    valkey) SERVICES="$SERVICES valkey:5:64:0" ;;
    monitoring) SERVICES="$SERVICES prometheus:15:256:0" ;;
    *) echo "limits: unknown profile $p (filter, valkey, monitoring)" >&2; exit 2 ;;
  esac
done

declare -A REP
IFS=',' read -ra RS <<< "$REPLICAS"
for r in "${RS[@]:-}"; do
  [ -n "$r" ] || continue
  [[ "$r" =~ ^[a-z-]+=[1-9][0-9]?$ ]] || { echo "limits: --replicas wants service=count (imap=2,smtp-in=3), got '$r'" >&2; exit 2; }
  REP["${r%%=*}"]="${r##*=}"
done

# ---- the arithmetic, in awk (integers of MiB for memory, hundredths for CPU)
budget_mem=$((TOTAL_MEM * PERCENT / 100))
budget_cpu="$(awk -v t="$TOTAL_CPUS" -v p="$PERCENT" 'BEGIN { printf "%.2f", t * p / 100 }')"
rows="$(for s in $SERVICES; do IFS=':' read -r name w min note <<< "$s"; [ "$EQUAL" = 1 ] && w=1; echo "$name $w $min $note ${REP[$name]:-1}"; done)"
result="$(awk -v budget="$budget_mem" -v cpu="$budget_cpu" '
  { name[NR] = $1; w[NR] = $2; min[NR] = $3; note[NR] = $4; rep[NR] = $5; n = NR; minsum += $3 * $5; wsum += $2 * $5 }
  END {
    if (minsum > budget) { printf "ERROR %d %d\n", minsum, budget; exit }
    spare = budget - minsum
    for (i = 1; i <= n; i++) {
      mem = min[i] + int(spare * w[i] / wsum)
      c = cpu * w[i] / wsum; if (c < 0.25) c = 0.25
      infl = int(mem / 3); if (infl < 64) infl = 64
      printf "%s %d %.2f %d %d %d\n", name[i], mem, c, rep[i], note[i], infl
    }
  }' <<< "$rows")"
case "$result" in
  ERROR*) read -r _ need have <<< "$result"; echo "limits: the minimums alone need $need MiB and the budget is $have MiB ($PERCENT % of $TOTAL_MEM): use fewer profiles or replicas, or a bigger machine" >&2; exit 1 ;;
esac

printf 'Total: %s MiB and %s CPUs on %s; budget %s %% = %s MiB and %s CPUs.\n\n' "$TOTAL_MEM" "$TOTAL_CPUS" "$source_of_totals" "$PERCENT" "$budget_mem" "$budget_cpu"
printf '%-12s %10s %8s %9s   %s\n' container "memory MiB" cpus replicas notes
used=0
while read -r name mem cpus reps note infl; do
  printf '%-12s %10d %8s %9d   %s\n' "$name" "$mem" "$cpus" "$reps" "$([ "$note" = 1 ] && echo "[limits] inflight_message_mib <= $infl")"
  used=$((used + mem * reps))
done <<< "$result"
printf '\nSum of the memory limits: %d MiB of the %d MiB budget (CPU limits are ceilings that containers share, not reservations).\n' "$used" "$budget_mem"

if [ -n "$OUT" ]; then
  {
    echo "# Generated by limits.sh: ${PERCENT} % of ${TOTAL_MEM} MiB and ${TOTAL_CPUS} CPUs of ${source_of_totals}, divided over the containers. Memory is per replica."
    echo "services:"
    while read -r name mem cpus _; do
      echo "  $name:"
      echo "    deploy:"
      echo "      resources:"
      echo "        limits: { cpus: \"$cpus\", memory: ${mem}M }"
    done <<< "$result"
  } > "$OUT"
  echo "limits: wrote $OUT"
fi
