#!/usr/bin/env bash
# Restore drill (spec §7.3): restore a node's newest backup into a scratch
# volume, check it, and show its row counts next to the live node's. The
# running node is never touched. Run it regularly in production (RUNBOOK: weekly).
# By default the exit code depends only on the copy, because a live node keeps
# changing. With --strict the drill also fails when the live node cannot be
# checked or any row count differs, so use it only when the node is idle
# (the end-to-end test does).
#   scripts/restore-drill.sh <node> [env-file] [--strict]
set -euo pipefail
cd "$(dirname "$0")/.."
strict=false
args=()
for arg in "$@"; do
  if [ "$arg" = --strict ]; then strict=true; else args+=("$arg"); fi
done
node=${args[0]:?usage: restore-drill.sh <node> [env-file] [--strict]}
env_file=${args[1]:-deploy/compose/local/$node.env}
PREFIX="${PREFIX:-}"
NETWORK="${SHARED_NETWORK:-${PREFIX}sqlite-prod}"
LITESTREAM_IMAGE="${LITESTREAM_IMAGE:-sqlite-production/litestream:dev}"
WEB_IMAGE="${WEB_IMAGE:-sqlite-production/web:dev}"
volume="${PREFIX}drill-${node}-$$"

# The scratch volume is mounted at /data, whose owner in the images (uid
# 10001) is copied into a new volume, so the restore can write there.
# Only NODE_ID and the LITESTREAM_* settings go to the Litestream container;
# the env file also holds POSTGRES_URL, which that container must never see.
litestream_env=$(mktemp)
trap 'docker volume rm -f "$volume" >/dev/null || true; rm -f "$litestream_env"' EXIT
grep -E '^(NODE_ID|LITESTREAM_[A-Z_]+)=' "$env_file" >"$litestream_env" || true
docker volume create "$volume" >/dev/null
net=()
if docker network inspect "$NETWORK" >/dev/null 2>&1; then net=(--network "$NETWORK"); fi

echo "== restoring the newest backup of $node into a scratch volume"
docker run --rm --env-file "$litestream_env" -e NODE_ID="$node" "${net[@]}" -v "$volume:/data" \
  --entrypoint litestream "$LITESTREAM_IMAGE" \
  restore -config /etc/litestream.yml -integrity-check full /data/library.db

echo "== checking the restored copy"
if ! copy_out=$(docker run --rm -v "$volume:/data" "$WEB_IMAGE" check-db 2>&1); then
  echo "$copy_out"
  echo "DRILL FAILED: the restored copy did not pass check-db" >&2
  exit 1
fi
echo "$copy_out"

echo "== the live node, for comparison (it keeps changing, so small differences are normal)"
live_ok=true
if ! live_out=$(docker compose -p "${PREFIX}${node}" -f deploy/compose/node.yml --env-file "$env_file" \
  exec -T web library-web check-db 2>&1); then
  live_ok=false
  echo "WARNING: the live check of $node failed (this does not fail the drill):" >&2
  echo "$live_out" >&2
else
  echo "$live_out"
fi

if $strict; then
  # "table: N" lines only; "integrity: ok" and the like are not counts.
  counts() { grep -E '^[a-z_]+: [0-9]+$' <<<"$1" || true; }
  copy_counts=$(counts "$copy_out")
  live_counts=$(counts "$live_out")
  $live_ok || { echo "DRILL FAILED (--strict): the live node could not be checked" >&2; exit 1; }
  if [ -z "$copy_counts" ] || [ -z "$live_counts" ]; then
    echo "DRILL FAILED (--strict): no row counts found to compare" >&2
    exit 1
  fi
  if [ "$copy_counts" != "$live_counts" ]; then
    echo "DRILL FAILED (--strict): row counts differ between the copy and the live node" >&2
    diff <(echo "$copy_counts") <(echo "$live_counts") >&2 || true
    exit 1
  fi
  echo "strict: all row counts match the live node"
fi
echo "DRILL PASSED"
