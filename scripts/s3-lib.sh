# shellcheck shell=bash disable=SC2034
# Shared naming for the throw-away test S3 server (RustFS). Source it; do not execute.
# Every run has an id (RUSTY_S3_RUN_ID, default "dev" for manual use). The container, compose project and settings
# file are all derived from it, so concurrent runs never touch each other. ci.sh always generates a fresh id.
S3_RUN_ID="${RUSTY_S3_RUN_ID:-dev}"
[[ "$S3_RUN_ID" =~ ^[a-z0-9]{1,24}$ ]] || { echo "s3: RUSTY_S3_RUN_ID must match [a-z0-9]{1,24}" >&2; exit 1; }
S3_NAME="rusty-s3-test-$S3_RUN_ID"
S3_ENV_FILE="$HOME/.rusty-s3-test-env.$S3_RUN_ID"
S3_COMPOSE=(docker compose -p "$S3_NAME" -f deploy/dev/s3.compose.yaml)
export RUSTY_S3_NAME="$S3_NAME"
export RUSTY_S3_RUN_ID="$S3_RUN_ID"
