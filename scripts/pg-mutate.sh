#!/usr/bin/env bash
# Mutation runner for the Postgres backend (plan PG6, D5). Generates the committed mutation table.
#
#   bash scripts/pg-mutate.sh reset                    forget earlier results
#   bash scripts/pg-mutate.sh baseline                 build once, run the unmutated judging suites (must be green), list mutations
#   bash scripts/pg-mutate.sh precheck                 every dialect mutation must change at least one statement (else VOID)
#   bash scripts/pg-mutate.sh dialect [OFFSET COUNT]   dialect / repository / logic mutations (chunkable)
#   bash scripts/pg-mutate.sh schema  [OFFSET COUNT]   drop one named constraint / trigger / index (chunkable)
#   bash scripts/pg-mutate.sh parity                   one-sided additions, judged by the sqlite_master oracle
#   bash scripts/pg-mutate.sh forget ID_REGEX...       drop recorded rows whose id matches (to re-run only those)
#   bash scripts/pg-mutate.sh table [OUT.md]           write the table from the accumulated results
#
# Needs RUSTY_TEST_PG_URL (scripts/pg-up.sh) and cargo. Nothing in the repository is edited: each
# mutation is selected at run time through environment variables read by the `mutation-testing`
# hook of rusty-store (src/mutation.rs), so the suites are compiled once. The hook is passed on the
# command line only and is absent from release builds (scripts/check-mutation-hook.sh). Results
# accumulate in $PG_MUTATE_RESULTS (default ~/pg-mutate.results.tsv) so a long run can be done in
# bounded chunks.
#
# Judging: each mutation is first run against the few test binaries that plausibly cover it
# (`judges`), and, if that does not kill it, against the FULL judging suite; only then is it
# reported SURVIVED. KILLED = a test failed (first failing test recorded), VOID = the mutation could
# not be applied or matched no statement (never counted as a kill), SURVIVED = nothing failed. A
# survivor is listed with the written waiver argument from scripts/pg-mutate-waivers.tsv, if any;
# an unwaived survivor is printed as UNEXPLAINED and fails the committed-table test.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
MODE="${1:-}"; shift || true
: "${RUSTY_TEST_PG_URL:?RUSTY_TEST_PG_URL is required}"
export RUSTY_REQUIRE_PG=1
RES="${PG_MUTATE_RESULTS:-$HOME/pg-mutate.results.tsv}"
LIST="${PG_MUTATE_LIST:-$HOME/pg-mutate.list.tsv}"
READY="${RES}.baseline"
F=rusty-store/mutation-testing
FS=rusty-store/mutation-testing
WORK="${TMPDIR:-$HOME}/pg-mutate.$$"; mkdir -p "$WORK"; trap 'rm -rf "$WORK"' EXIT
PROBE=the_selected_mutation_changed_at_least_one_statement
SKIP=(-- --skip the_committed_mutation_table_lists_every_dialect_mutation)

# Full judging suites.
FULL_DIALECT=(-p rusty-store -p rusty-directory --features "$F" --no-fail-fast
  --test mailbox_conformance --test jobs_conformance --test gc_conformance --test pg_mailbox_concurrency
  --test pg_jobs_gc_concurrency --test directory_conformance --test mailbox --test jobs --test pg_dialect_mutations
  --test query_shape --test input_validation_conformance --test race_conformance "${SKIP[@]}")
FULL_SCHEMA=(-p rusty-store --features "$FS" --test pg_schema_cases --test pg_schema_constraints
  --test pg_schema_concurrency --test pg_plans --test mailbox_conformance --test jobs_conformance --test gc_conformance
  --test pg_mailbox_concurrency)
QUICK_SCHEMA=(-p rusty-store --features "$FS" --test pg_schema_cases --test pg_schema_constraints)
ORACLE=(-p rusty-store --features "$FS" --test pg_schema_parity pg_catalog_matches_snapshot)

# judges <id>: the cheap, plausible judging set of a dialect mutation (cargo args)
judges() {
  local s=(-p rusty-store --features "$FS" --test mailbox_conformance --test jobs_conformance --test gc_conformance)
  local d=(-p rusty-store -p rusty-directory --features "$F" --test mailbox_conformance --test jobs_conformance --test gc_conformance --test directory_conformance)
  case "$1" in
    D01*|D03*|D05*|D07*|D11*|D12*) printf '%s\n' "${d[@]}" -- --skip the_committed_mutation_table_lists_every_dialect_mutation ;;
    D09*|R01*) printf '%s\n' "${s[@]}" --test pg_mailbox_concurrency ;;
    R02*|R03*|R04*|R05*|R06*) printf '%s\n' "${s[@]}" --test pg_jobs_gc_concurrency ;;
    R08*) printf '%s\n' "${s[@]}" --test query_shape ;;
    L01*) printf '%s\n' "${s[@]}" --test pg_mailbox_concurrency --test error_busy ;;
    *) printf '%s\n' "${s[@]}" ;;
  esac
}

# first_fail <log> [all]: "binary::test" of the first (or all) failing tests, never the probe
first_fail() {
  ALLFAIL="${2:-0}" PROBE="$PROBE" awk '
    /Running tests\// { match($0, /tests\/[A-Za-z0-9_]+/); bin = substr($0, RSTART + 6, RLENGTH - 6) }
    /^test .* FAILED$/ { sub(/^test /, ""); sub(/ \.\.\. FAILED$/, "");
      if ($0 == ENVIRON["PROBE"]) next
      r = (r == "" ? "" : r ", ") bin "::" $0
      if (ENVIRON["ALLFAIL"] != "1") { print r; exit } }
    END { if (ENVIRON["ALLFAIL"] == "1" && r != "") print r }' "$1"
}

# stamp: the commit the rows below are produced at ("+dirty" when crates/ or this script differ from it)
stamp() {
  local c; c="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
  [ -z "$(git status --porcelain -- crates scripts/pg-mutate.sh 2>/dev/null)" ] || c="$c+dirty"
  echo "$c"
}

# record <class> <id> <outcome> <killer> <what> [oracle]: one row, stamped with its own commit (field 7)
record() { printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" "$5" "${6:-}" "$(stamp)" >>"$RES"; echo "$1 $2: $3 $4" >&2; }

# run <log> <env...> -- <cargo args>: returns 0 always; log in $1
run() {
  local log="$1"; shift
  local envs=(); while [ "$1" != "--" ]; do envs+=("$1"); shift; done; shift
  env "${envs[@]}" cargo test "$@" >"$log" 2>&1
}

# judge <class> <id> <what> <env> <quick args as lines via function> ...
judge_dialect() {
  local id="$1" what="$2" log="$WORK/log.txt" fail
  mapfile -t qa < <(judges "$id")
  run "$log" RUSTY_SQL_MUTATION="$id" -- "${qa[@]}"
  fail="$(first_fail "$log")"
  if [ -z "$fail" ]; then
    run "$log" RUSTY_SQL_MUTATION="$id" -- "${FULL_DIALECT[@]}"
    fail="$(first_fail "$log")"
  fi
  if grep -q "^error: could not compile" "$log"; then record dialect "$id" VOID "build failed" "$what"
  elif [ -n "$fail" ]; then record dialect "$id" KILLED "$fail" "$what"
  else record dialect "$id" SURVIVED "-" "$what"; fi
}

# The baseline marker holds the commit the unmutated suites were last green at. Rows are written
# only when (a) that commit is the current one and (b) the unmutated suite of the chunk's class is
# green again right now (`recheck_baseline`), so a row can never be a kill of an already-red tree.
need_baseline() {
  case "$(stamp)" in *+dirty) echo "the tree has uncommitted changes under crates/ or scripts/pg-mutate.sh: commit first, rows are stamped with a commit" >&2; exit 1 ;; esac
  [ -f "$READY" ] && [ -f "$LIST" ] || { echo "run '$0 baseline' first" >&2; exit 1; }
  [ "$(cat "$READY")" = "$(stamp)" ] || { echo "baseline was green at '$(cat "$READY")', the tree is at '$(stamp)': run '$0 baseline' again" >&2; exit 1; }
}

# recheck_baseline <suite array name>: run that suite unmutated; abort the chunk when it is red
recheck_baseline() {
  declare -n suite="$1"
  if ! env RUSTY_SQL_MUTATION= RUSTY_SQL_MUTATION_POST_MIGRATE= cargo test "${suite[@]}" >"$WORK/recheck.txt" 2>&1; then
    echo "the unmutated $1 suite is red now; no rows are written:" >&2
    grep -E "^test .* FAILED|panicked" "$WORK/recheck.txt" | head >&2; exit 1
  fi
  echo "baseline re-checked ($1 green at $(stamp))" >&2
}

case "$MODE" in
  reset) rm -f "$RES" "$READY" "$LIST"; echo "reset" >&2 ;;

  baseline)
    case "$(stamp)" in *+dirty) echo "commit first: the baseline is recorded for a commit" >&2; exit 1 ;; esac
    cargo test --no-run "${FULL_DIALECT[@]}" >/dev/null 2>&1 || { echo "build failed (dialect suites)" >&2; exit 1; }
    cargo test --no-run "${FULL_SCHEMA[@]}" >/dev/null 2>&1 || { echo "build failed (schema suites)" >&2; exit 1; }
    for args in FULL_DIALECT FULL_SCHEMA; do
      declare -n a="$args"
      if ! env RUSTY_SQL_MUTATION= cargo test "${a[@]}" >"$WORK/base.txt" 2>&1; then
        echo "baseline is not green for $args; fix it first:" >&2
        grep -E "^test .* FAILED|panicked" "$WORK/base.txt" | head >&2; exit 1
      fi
    done
    cargo test -p rusty-store --features "$FS" --test pg_dialect_mutations -- --ignored --nocapture list_mutations 2>/dev/null \
      | grep -E '^(DIALECT|SCHEMA)	' >"$LIST"
    echo "baseline green; $(grep -c '^DIALECT' "$LIST") dialect and $(grep -c '^SCHEMA' "$LIST") schema mutations" >&2
    stamp >"$READY" ;;

  forget)
    [ "$#" -ge 1 ] || { echo "usage: $0 forget ID_REGEX..." >&2; exit 2; }
    for re in "$@"; do
      n=$(awk -F'\t' -v re="$re" '$2 ~ re' "$RES" | wc -l)
      awk -F'\t' -v re="$re" '$2 !~ re' "$RES" >"$WORK/res.new" && cat "$WORK/res.new" >"$RES"
      echo "forgot $n row(s) matching $re" >&2
    done ;;

  precheck)
    need_baseline
    while IFS=$'\t' read -r kind id what; do
      [ "$kind" = DIALECT ] || continue
      log="$WORK/pre.txt"
      env RUSTY_SQL_MUTATION="$id" cargo test -p rusty-store --features "$FS" --test pg_dialect_mutations "$PROBE" >"$log" 2>&1
      if grep -q "matched no statement" "$log"; then record dialect "$id" VOID "matched no statement (vacuous)" "$what"; fi
    done <"$LIST"
    echo "precheck done: VOID rows above (none = every dialect mutation changes a statement)" >&2 ;;

  dialect)
    need_baseline
    recheck_baseline FULL_DIALECT
    off="${1:-0}"; cnt="${2:-1000}"; n=0
    while IFS=$'\t' read -r kind id what; do
      [ "$kind" = DIALECT ] || continue
      n=$((n + 1)); [ "$n" -gt "$off" ] && [ "$n" -le $((off + cnt)) ] || continue
      grep -q "^dialect	$id	" "$RES" 2>/dev/null && { echo "skip $id (done)" >&2; continue; }
      judge_dialect "$id" "$what"
    done <"$LIST" ;;

  schema)
    need_baseline
    recheck_baseline FULL_SCHEMA
    off="${1:-0}"; cnt="${2:-1000}"; n=0
    while IFS=$'\t' read -r kind id sql; do
      [ "$kind" = SCHEMA ] || continue
      n=$((n + 1)); [ "$n" -gt "$off" ] && [ "$n" -le $((off + cnt)) ] || continue
      grep -q "^schema	$id	" "$RES" 2>/dev/null && { echo "skip $id (done)" >&2; continue; }
      log="$WORK/log.txt"; mut="postgres|$sql"
      run "$log" RUSTY_SQL_MUTATION_POST_MIGRATE="$mut" -- "${QUICK_SCHEMA[@]}"
      fail="$(first_fail "$log")"
      if [ -z "$fail" ] && ! grep -q "mutation statement failed" "$log"; then
        run "$log" RUSTY_SQL_MUTATION_POST_MIGRATE="$mut" -- "${FULL_SCHEMA[@]}"
        fail="$(first_fail "$log")"
      fi
      run "$WORK/par.txt" RUSTY_SQL_MUTATION_POST_MIGRATE="$mut" -- "${ORACLE[@]}"
      if grep -q "^test .* FAILED" "$WORK/par.txt"; then par=yes; else par=no; fi
      if grep -q "mutation statement failed" "$log"; then
        if grep -q "depend on it" "$log"; then record schema "$id" VOID "not mutable alone: other objects depend on it (a foreign key references it)" "$sql" "$par"
        else record schema "$id" VOID "mutation could not be applied" "$sql" "$par"; fi
      elif [ -n "$fail" ]; then record schema "$id" KILLED "$fail" "$sql" "$par"
      else record schema "$id" SURVIVED "-" "$sql" "$par"; fi
    done <"$LIST" ;;

  parity)
    need_baseline
    recheck_baseline FULL_SCHEMA
    one() { # id what env
      local id="$1" what="$2" e="$3" log="$WORK/log.txt"
      grep -q "^parity	$id	" "$RES" 2>/dev/null && return
      run "$log" RUSTY_SQL_MUTATION_POST_MIGRATE="$e" -- -p rusty-store --features "$FS" --test pg_schema_parity
      local fail; fail="$(first_fail "$log" 1)"
      if grep -q "mutation statement failed" "$log"; then record parity "$id" VOID "mutation could not be applied" "$what"
      elif echo "$fail" | grep -q "pg_schema_matches_sqlite_structure"; then record parity "$id" KILLED "$fail" "$what"
      elif [ -n "$fail" ]; then record parity "$id" "KILLED (snapshot only)" "$fail" "$what"
      else record parity "$id" SURVIVED "-" "$what"; fi
    }
    NL=$'\n;;\n'
    one pg-only-column "ALTER TABLE domains ADD COLUMN mut_extra integer on Postgres only" "postgres|ALTER TABLE domains ADD COLUMN mut_extra integer"
    one pg-only-index "CREATE INDEX on Postgres only" "postgres|CREATE INDEX mut_idx ON domains (created_at)"
    one pg-only-trigger "a trigger on Postgres only" "postgres|CREATE FUNCTION mut_fn() RETURNS trigger LANGUAGE plpgsql AS \$\$ BEGIN RETURN NEW; END \$\$${NL}postgres|CREATE TRIGGER mut_trg BEFORE INSERT ON domains FOR EACH ROW EXECUTE FUNCTION mut_fn()"
    one sqlite-only-column "ALTER TABLE domains ADD COLUMN mut_extra integer on SQLite only" "sqlite|ALTER TABLE domains ADD COLUMN mut_extra integer"
    one sqlite-only-index "CREATE INDEX on SQLite only" "sqlite|CREATE INDEX mut_idx ON domains (created_at)"
    one sqlite-only-trigger "a trigger on SQLite only" "sqlite|CREATE TRIGGER mut_trg BEFORE INSERT ON domains BEGIN SELECT 1; END" ;;

  table)
    OUT="${1:-docs/superpowers/plans/2026-11-03-postgres-backend-mutation-table.md}"
    WAIVERS="scripts/pg-mutate-waivers.tsv"
    {
      echo "# Postgres backend: mutation table"
      echo
      echo "Generated by \`scripts/pg-mutate.sh table\` from the accumulated results on $(date -u +%Y-%m-%d). Do not edit by hand; regenerate. Rows were produced across several commits: the last column of every row is the commit it was judged at (the runner refuses to write a row unless the tree is committed, equals the baseline commit and the unmutated suite is green again at that moment)."
      echo "Mutations are selected at run time (see the header of \`scripts/pg-mutate.sh\`); nothing in the repository is edited."
      echo "Each mutation was judged first by the test binaries that plausibly cover it and, if that did not kill it, by the full judging suite. The baseline (hook compiled in, no mutation selected) was green."
      echo "Schema coverage: every named CHECK, UNIQUE, FOREIGN KEY and PRIMARY KEY constraint, every trigger and every non-constraint index is dropped once. Not covered: NOT NULL constraints, column defaults and identity definitions, and the bodies of trigger functions (they are exercised by the behaviour suites but never mutated)."
      echo
      total=$(wc -l <"$RES"); killed=$(grep -c $'\tKILLED' "$RES"); surv=$(grep -c $'\tSURVIVED\t' "$RES"); void=$(grep -c $'\tVOID\t' "$RES")
      echo "**$total mutations: $killed killed, $surv survived, $void void.** A void mutation is not a kill. Survivors carry a written waiver (one line per id in \`scripts/pg-mutate-waivers.tsv\`, no wildcards) for the reviewer to accept or reject; a survivor without one is printed as UNEXPLAINED and fails the committed-table test."
      echo
      for class in dialect schema parity; do
        grep -q "^$class"$'\t' "$RES" || continue
        echo "## $class"; echo
        if [ "$class" = schema ]; then echo "| id | outcome | first failing test | sqlite_master oracle also fails | mutation | commit |"; echo "|---|---|---|---|---|---|"
        else echo "| id | outcome | first failing test | mutation | commit |"; echo "|---|---|---|---|---|"; fi
        grep "^$class"$'\t' "$RES" | while IFS=$'\t' read -r _ id outcome killer what par commit; do
          what="${what//|/\|}"
          if [ "$class" = schema ]; then echo "| \`$id\` | $outcome | \`$killer\` | ${par:-n/a} | \`$what\` | \`${commit:-before per-row stamps}\` |"
          else echo "| \`$id\` | $outcome | \`$killer\` | $what | \`${commit:-before per-row stamps}\` |"; fi
        done; echo
      done
      echo "## Survivors"; echo
      if [ "$surv" -eq 0 ]; then echo "None."; else
        grep $'\tSURVIVED\t' "$RES" | while IFS=$'\t' read -r class id _ _ what _; do
          w="$(awk -F'\t' -v id="$id" '$1 == id { print $2; exit }' "$WAIVERS" 2>/dev/null)"
          if [ -n "$w" ]; then echo "- $class \`$id\` (WAIVER proposed): $what. $w"
          else echo "- $class \`$id\` (UNEXPLAINED): $what"; fi
        done
      fi
    } >"$OUT"
    echo "table written to $OUT" >&2 ;;

  *) echo "usage: $0 reset|baseline|precheck|dialect [OFFSET COUNT]|schema [OFFSET COUNT]|parity|forget ID_REGEX...|table [OUT]" >&2; exit 2 ;;
esac
