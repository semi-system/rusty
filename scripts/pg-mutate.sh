#!/usr/bin/env bash
# Mutation runner for the Postgres backend (plan PG6, D5). Generates the committed mutation table.
#
#   bash scripts/pg-mutate.sh [all|dialect|schema|parity] [OUT.md]
#
# Needs a Postgres test server (RUSTY_TEST_PG_URL, from scripts/pg-up.sh) and cargo. Nothing in the
# repository is edited: every mutation is selected at run time through environment variables read
# by the `mutation-testing` hook of rusty-store (src/mutation.rs), so the suites are compiled once
# and each mutation costs a test run. The hook is passed on the command line only and is absent from
# release builds (scripts/check-mutation-hook.sh).
#
# Classes:
#   dialect/repository  one literal statement rewrite or Rust-logic flag from mutation::TABLE,
#                       judged by the dialect suites below (both backends run in each binary);
#   schema              drop ONE named constraint / trigger / stand-alone index of the Postgres
#                       catalog (list generated from the live catalog), judged by the behavioural
#                       schema suites; the catalog snapshot/parity test is reported separately,
#                       because a snapshot is a change detector and a regenerated one hides drift;
#   parity              add a column / index / trigger on ONE side only (Postgres, or SQLite), judged
#                       by pg_schema_parity (the sqlite_master oracle).
# A mutation is KILLED when at least one test fails (first failing test recorded), SURVIVED when
# every judging test passes, VOID when the mutation itself could not be applied (statement failed).
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
MODE="${1:-all}"
OUT="${2:-docs/superpowers/plans/2026-11-03-postgres-backend-mutation-table.md}"
: "${RUSTY_TEST_PG_URL:?RUSTY_TEST_PG_URL is required}"
export RUSTY_REQUIRE_PG=1
F=rusty-store/postgres,rusty-store/mutation-testing,rusty-directory/postgres
FS=rusty-store/postgres,rusty-store/mutation-testing
WORK="${TMPDIR:-$HOME}/pg-mutate.$$"; mkdir -p "$WORK"; trap 'rm -rf "$WORK"' EXIT
RES="$WORK/results.tsv"; : > "$RES"

# Judging suites. Dialect: the conformance and concurrency suites of the repositories.
DIALECT_ARGS=(-p rusty-store -p rusty-directory --features "$F" --no-fail-fast
  --test mailbox_conformance --test jobs_conformance --test gc_conformance --test pg_mailbox_concurrency
  --test pg_jobs_gc_concurrency --test directory_conformance --test pg_dialect_mutations --test mailbox --test jobs
  -- --skip the_committed_mutation_table_lists_every_dialect_mutation)
SCHEMA_ARGS=(-p rusty-store --features "$FS" --test pg_schema_cases --test pg_schema_constraints
  --test pg_schema_concurrency --test mailbox_conformance --test jobs_conformance --test gc_conformance
  --test pg_mailbox_concurrency)
PARITY_ARGS=(-p rusty-store --features "$FS" --test pg_schema_parity)

# first_fail <logfile>: "binary::test" of the first failing test, or empty
first_fail() {
  awk '/Running tests\// { match($0, /tests\/[A-Za-z0-9_]+/); bin = substr($0, RSTART + 6, RLENGTH - 6) }
       /^test .* FAILED$/ { sub(/^test /, ""); sub(/ ... FAILED$/, ""); r = (r == "" ? "" : r ", ") bin "::" $0; if (ENVIRON["ALLFAIL"] != "1") { print r; exit } }
       END { if (ENVIRON["ALLFAIL"] == "1" && r != "") print r }' "$1"
}

# run_one <class> <id> <description> <env...> -- <cargo test args>
run_one() {
  local class="$1" id="$2" what="$3"; shift 3
  local envs=() ; while [ "$1" != "--" ]; do envs+=("$1"); shift; done; shift
  local log="$WORK/log.txt" fail
  env "${envs[@]}" cargo test "$@" >"$log" 2>&1
  fail="$(ALLFAIL=$([ "$class" = parity ] && echo 1 || echo 0) first_fail "$log")"
  if grep -q "mutation statement failed" "$log" || grep -q "^error: could not compile" "$log"; then
    printf '%s\t%s\tVOID\t%s\t%s\n' "$class" "$id" "mutation could not be applied" "$what" >>"$RES"
  elif [ -n "$fail" ]; then
    printf '%s\t%s\tKILLED\t%s\t%s\n' "$class" "$id" "$fail" "$what" >>"$RES"
  else
    printf '%s\t%s\tSURVIVED\t-\t%s\n' "$class" "$id" "$what" >>"$RES"
  fi
  echo "$class $id: $(tail -1 "$RES" | cut -f3,4)" >&2
}

echo "building the suites once ..." >&2
cargo test --no-run "${DIALECT_ARGS[@]}" >/dev/null 2>&1 || { echo "build failed" >&2; exit 1; }
cargo test --no-run "${SCHEMA_ARGS[@]}" >/dev/null 2>&1 || { echo "build failed" >&2; exit 1; }

# Baseline: with the hook compiled in and NO mutation selected every judging suite must be green,
# otherwise "first failing test" would be a pre-existing failure, not a kill.
for args in DIALECT_ARGS SCHEMA_ARGS PARITY_ARGS; do
  declare -n a="$args"
  if ! env RUSTY_SQL_MUTATION= cargo test "${a[@]}" >"$WORK/base.txt" 2>&1; then
    echo "baseline is not green for $args; fix it first:" >&2
    grep -E "^test .* FAILED|panicked" "$WORK/base.txt" | head >&2
    exit 1
  fi
done
echo "baseline green" >&2

LIST="$WORK/list.tsv"
cargo test -p rusty-store --features "$FS" --test pg_dialect_mutations -- --ignored --nocapture list_mutations 2>/dev/null \
  | grep -E '^(DIALECT|SCHEMA)	' >"$LIST"

if [ "$MODE" = all ] || [ "$MODE" = dialect ]; then
  while IFS=$'\t' read -r kind id what; do
    [ "$kind" = DIALECT ] || continue
    run_one dialect "$id" "$what" RUSTY_SQL_MUTATION="$id" -- "${DIALECT_ARGS[@]}"
  done <"$LIST"
fi

if [ "$MODE" = all ] || [ "$MODE" = schema ]; then
  while IFS=$'\t' read -r kind id sql; do
    [ "$kind" = SCHEMA ] || continue
    # behavioural verdict (stops at the first failing binary) ...
    run_one schema "$id" "$sql" RUSTY_SQL_MUTATION_POST_MIGRATE="postgres|$sql" -- "${SCHEMA_ARGS[@]}"
    # ... and whether the catalog snapshot / sqlite_master parity also notices it
    row="$(tail -1 "$RES")"
    cargo_log="$WORK/par.txt"
    RUSTY_SQL_MUTATION_POST_MIGRATE="postgres|$sql" cargo test "${PARITY_ARGS[@]}" >"$cargo_log" 2>&1
    if grep -q "^test .* FAILED" "$cargo_log"; then par=yes; else par=no; fi
    printf '%s\t%s\n' "$(printf '%s' "$row" | cut -f1-5)" "$par" >"$WORK/row.tmp"
    sed -i '$d' "$RES"; cat "$WORK/row.tmp" >>"$RES"
  done <"$LIST"
fi

if [ "$MODE" = all ] || [ "$MODE" = parity ]; then
  NL=$'\n;;\n'
  run_one parity "pg-only-column" "ALTER TABLE domains ADD COLUMN mut_extra integer on Postgres only" \
    RUSTY_SQL_MUTATION_POST_MIGRATE="postgres|ALTER TABLE domains ADD COLUMN mut_extra integer" -- "${PARITY_ARGS[@]}"
  run_one parity "pg-only-index" "CREATE INDEX on Postgres only" \
    RUSTY_SQL_MUTATION_POST_MIGRATE="postgres|CREATE INDEX mut_idx ON domains (created_at)" -- "${PARITY_ARGS[@]}"
  run_one parity "pg-only-trigger" "a trigger on Postgres only" \
    RUSTY_SQL_MUTATION_POST_MIGRATE="postgres|CREATE FUNCTION mut_fn() RETURNS trigger LANGUAGE plpgsql AS \$\$ BEGIN RETURN NEW; END \$\$${NL}postgres|CREATE TRIGGER mut_trg BEFORE INSERT ON domains FOR EACH ROW EXECUTE FUNCTION mut_fn()" -- "${PARITY_ARGS[@]}"
  run_one parity "sqlite-only-column" "ALTER TABLE domains ADD COLUMN mut_extra integer on SQLite only" \
    RUSTY_SQL_MUTATION_POST_MIGRATE="sqlite|ALTER TABLE domains ADD COLUMN mut_extra integer" -- "${PARITY_ARGS[@]}"
  run_one parity "sqlite-only-index" "CREATE INDEX on SQLite only" \
    RUSTY_SQL_MUTATION_POST_MIGRATE="sqlite|CREATE INDEX mut_idx ON domains (created_at)" -- "${PARITY_ARGS[@]}"
  run_one parity "sqlite-only-trigger" "a trigger on SQLite only" \
    RUSTY_SQL_MUTATION_POST_MIGRATE="sqlite|CREATE TRIGGER mut_trg BEFORE INSERT ON domains BEGIN SELECT 1; END" -- "${PARITY_ARGS[@]}"
fi

# ---- table
{
  echo "# Postgres backend: mutation table"
  echo
  echo "Generated by \`bash scripts/pg-mutate.sh $MODE\` on $(date -u +%Y-%m-%d) at commit \`$(git rev-parse --short HEAD)\`. Do not edit by hand; regenerate."
  echo "Mutations are selected at run time (see the header of \`scripts/pg-mutate.sh\`); nothing in the repository is edited."
  echo
  total=$(wc -l <"$RES"); killed=$(grep -c $'\tKILLED\t' "$RES"); surv=$(grep -c $'\tSURVIVED\t' "$RES"); void=$(grep -c $'\tVOID\t' "$RES")
  echo "**$total mutations: $killed killed, $surv survived, $void void** (void = the mutation statement could not be applied; not counted as a kill)."
  echo
  for class in dialect schema parity; do
    grep -q "^$class"$'\t' "$RES" || continue
    echo "## $class"
    echo
    if [ "$class" = schema ]; then
      echo "| id | outcome | first failing test | snapshot or parity also fails | mutation |"
      echo "|---|---|---|---|---|"
    else
      echo "| id | outcome | first failing test | mutation |"
      echo "|---|---|---|---|"
    fi
    grep "^$class"$'\t' "$RES" | while IFS=$'\t' read -r _ id outcome killer what par; do
      what="${what//|/\\|}"
      if [ "$class" = schema ]; then echo "| \`$id\` | $outcome | \`$killer\` | ${par:-n/a} | \`$what\` |"
      else echo "| \`$id\` | $outcome | \`$killer\` | $what |"; fi
    done
    echo
  done
  echo "## Survivors"
  echo
  if [ "$surv" -eq 0 ]; then echo "None."; else
    grep $'\tSURVIVED\t' "$RES" | while IFS=$'\t' read -r class id _ _ what _; do echo "- $class \`$id\`: $what"; done
  fi
} >"$OUT"
echo "table written to $OUT" >&2
