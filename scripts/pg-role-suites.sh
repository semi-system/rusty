#!/usr/bin/env bash
# Runs each service's integration suites logged in as that service's own database role (RUSTY_TEST_PG_ROLE), so a missing grant shows as a failing test and a grant nobody
# needs can be removed. Needs RUSTY_TEST_PG_URL (a superuser URL of a throw-away server). Usage: scripts/pg-role-suites.sh [role ...]   (default: every service role)
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
CARGO="${CARGO:-cargo}"
declare -A SUITES=(
  [rusty_smtp_in]="-p rusty-smtp --test inbound --test message --test scan --test session --test busy_e2e"
  [rusty_submission]="-p rusty-smtp --test submission --test session_auth"
  [rusty_imap]="-p rusty-imap"
  [rusty_outbound]="-p rusty-smtp --test outbound_worker --test outbound_crash --test outbound_dns --test dns_job --test dkim_sealed --test relay_check"
  [rusty_gc]="-p rusty-smtp --test outbound_gc -p rusty-svc --test service"
)
roles=("$@"); [ "${#roles[@]}" -gt 0 ] || roles=(rusty_smtp_in rusty_submission rusty_imap rusty_outbound rusty_gc)
bad=0
for r in "${roles[@]}"; do
  [ -n "${SUITES[$r]:-}" ] || { echo "pg-role-suites: unknown role $r" >&2; exit 2; }
  echo "== $r: ${SUITES[$r]}"
  # shellcheck disable=SC2086
  if RUSTY_TEST_PG_ROLES=1 RUSTY_TEST_PG_ROLE="$r" $CARGO test ${SUITES[$r]} --no-fail-fast 2>&1 | tee "/tmp/pg-role-$r.log" | grep -E "^test .*FAILED|test result: FAILED|permission denied" | sort | uniq -c | head -20 | grep . ; then bad=1; fi
  grep -E "^test result" "/tmp/pg-role-$r.log" | awk '{p+=$4; f+=$6} END {print "   passed=" p " failed=" f}'
done
exit "$bad"
