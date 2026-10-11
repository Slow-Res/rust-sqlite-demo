#!/usr/bin/env bash
# Restore drill (spec §7.3): restore a node's newest backup into a scratch
# volume, check it, and show its row counts next to the live node's. The
# running node is never touched. Run it regularly in production (RUNBOOK: weekly).
#   scripts/restore-drill.sh <node> [env-file]
set -euo pipefail
cd "$(dirname "$0")/.."
node=${1:?usage: restore-drill.sh <node> [env-file]}
env_file=${2:-deploy/compose/local/$node.env}
PREFIX="${PREFIX:-}"
NETWORK="${SHARED_NETWORK:-${PREFIX}sqlite-prod}"
LITESTREAM_IMAGE="${LITESTREAM_IMAGE:-sqlite-production/litestream:dev}"
WEB_IMAGE="${WEB_IMAGE:-sqlite-production/web:dev}"
volume="${PREFIX}drill-${node}-$$"

# The scratch volume is mounted at /data, whose owner in the images (uid
# 10001) is copied into a new volume, so the restore can write there.
trap 'docker volume rm -f "$volume" >/dev/null' EXIT
docker volume create "$volume" >/dev/null
net=()
if docker network inspect "$NETWORK" >/dev/null 2>&1; then net=(--network "$NETWORK"); fi

echo "== restoring the newest backup of $node into a scratch volume"
docker run --rm --env-file "$env_file" -e NODE_ID="$node" "${net[@]}" -v "$volume:/data" \
  --entrypoint litestream "$LITESTREAM_IMAGE" \
  restore -config /etc/litestream.yml -integrity-check full /data/library.db

echo "== checking the restored copy"
if ! docker run --rm -v "$volume:/data" "$WEB_IMAGE" check-db; then
  echo "DRILL FAILED: the restored copy did not pass check-db" >&2
  exit 1
fi

echo "== the live node, for comparison (it keeps changing, so small differences are normal)"
docker compose -p "${PREFIX}${node}" -f deploy/compose/node.yml --env-file "$env_file" \
  exec -T web library-web check-db || echo "(live node not reachable from here)"
echo "DRILL PASSED"
