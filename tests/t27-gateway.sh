#!/bin/bash
# Actual gateway CLI, unprivileged server, and immutable S3 operations against local MinIO.
. "$(dirname "$0")/lib.sh"

if [ -z "${LAYR_MINIO_ENDPOINT:-}" ]; then
    echo 'SKIP gateway MinIO test: set LAYR_MINIO_ENDPOINT and test credentials'
    finish
    exit $?
fi
: "${LAYR_MINIO_ACCESS_KEY:?Set the local MinIO test access key}"
: "${LAYR_MINIO_SECRET_KEY:?Set the local MinIO test secret key}"
export LAYR_GATEWAY_BINARY="$LAYR"
REPO=$(cd "$(dirname "$0")/.." && pwd)
LOG="$T/gateway-minio.log"
if (cd "$REPO" && cargo test --locked gateway::minio::actual_cli_minio_round_trip -- --exact --ignored --nocapture) > "$LOG" 2>&1; then
    PASS=$((PASS + 1))
    echo 'PASS actual gateway CLI and MinIO boundary checks'
else
    FAIL=$((FAIL + 1))
    echo 'FAIL actual gateway CLI and MinIO boundary checks'
    tail -50 "$LOG"
fi
finish
