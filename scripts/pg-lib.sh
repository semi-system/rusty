# shellcheck shell=bash disable=SC2034
# Shared naming for the throw-away test Postgres. Source it; do not execute.
# Every run has an id (RUSTY_PG_RUN_ID, default "dev" for manual use). The container, compose
# project and URL file are all derived from it, so concurrent runs never touch each other.
# ci.sh always generates a fresh random id.
RUN_ID="${RUSTY_PG_RUN_ID:-dev}"
[[ "$RUN_ID" =~ ^[a-z0-9]{1,24}$ ]] || { echo "pg: RUSTY_PG_RUN_ID must match [a-z0-9]{1,24}" >&2; exit 1; }
PG_NAME="rusty-pg-test-$RUN_ID"
URL_FILE="$HOME/.rusty-pg-test-url.$RUN_ID"
COMPOSE=(docker compose -p "$PG_NAME" -f deploy/dev/postgres.compose.yaml)
export RUSTY_PG_NAME="$PG_NAME"
export RUSTY_PG_RUN_ID="$RUN_ID"
