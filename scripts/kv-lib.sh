# shellcheck shell=bash disable=SC2034
# Shared naming for the throw-away test Valkey. Source it; do not execute.
# Every run has an id (RUSTY_KV_RUN_ID, default "dev" for manual use). The container, compose project and URL
# file are all derived from it, so concurrent runs never touch each other. ci.sh always generates a fresh id.
KV_RUN_ID="${RUSTY_KV_RUN_ID:-dev}"
[[ "$KV_RUN_ID" =~ ^[a-z0-9]{1,24}$ ]] || { echo "kv: RUSTY_KV_RUN_ID must match [a-z0-9]{1,24}" >&2; exit 1; }
KV_NAME="rusty-kv-test-$KV_RUN_ID"
KV_URL_FILE="$HOME/.rusty-kv-test-url.$KV_RUN_ID"
KV_COMPOSE=(docker compose -p "$KV_NAME" -f deploy/dev/valkey.compose.yaml)
export RUSTY_KV_NAME="$KV_NAME"
export RUSTY_KV_RUN_ID="$KV_RUN_ID"
