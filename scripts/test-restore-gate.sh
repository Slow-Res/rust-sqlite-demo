#!/usr/bin/env bash
# Tests the restore gate (spec §7.2) in every case that needs no working S3.
# The S3 cases (restore from a backup, no backup at all) run in scripts/e2e.sh.
set -euo pipefail
IMAGE="${LITESTREAM_IMAGE:-sqlite-production/litestream:dev}"
VOLUMES=()
PASSED=0
cleanup() { for v in "${VOLUMES[@]}"; do docker volume rm -f "$v" >/dev/null; done; }
trap cleanup EXIT

# new_volume <var>: create a scratch volume and store its name in <var>.
# Not `var=$(new_volume)`: a subshell would lose the VOLUMES entry, so every
# volume would get the same name and cleanup would miss them.
new_volume() {
  local v="restore-gate-test-$$-${#VOLUMES[@]}"
  docker volume create "$v" >/dev/null
  VOLUMES+=("$v")
  printf -v "$1" '%s' "$v"
}

# Run a shell command as uid 10001 inside a volume.
in_volume() { docker run --rm -v "$1:/data" --entrypoint sh "$IMAGE" -c "$2"; }

# gate <volume> <expected exit code> <text the output must contain> [docker run options...]
# S3 points at a closed port, so any attempt to reach it fails. Each such case
# takes about 4 minutes: litestream retries before it gives up. That is expected.
gate() {
  local volume=$1 want=$2 text=$3
  shift 3
  local out code=0
  out=$(docker run --rm -v "$volume:/data" \
    -e LITESTREAM_BUCKET=backups -e LITESTREAM_ENDPOINT=http://127.0.0.1:1 -e LITESTREAM_REGION=test \
    -e LITESTREAM_ACCESS_KEY_ID=test -e LITESTREAM_SECRET_ACCESS_KEY=test \
    "$@" --entrypoint restore.sh "$IMAGE" 2>&1) || code=$?
  if [[ "$code" != "$want" || "$out" != *"$text"* ]]; then
    printf 'FAIL (%s): exit %s, want %s; output:\n%s\n' "$*" "$code" "$want" "$out"
    exit 1
  fi
  PASSED=$((PASSED + 1))
}

new_volume empty
for bad in "" "Node1" "node_1" "-node" "a/b" "../x" $'node1\nx' "$(printf 'a%.0s' {1..41})"; do
  gate "$empty" 1 "must be 1-40 characters" -e "NODE_ID=$bad"
done

new_volume present
in_volume "$present" "sqlite3 /data/library.db 'CREATE TABLE t (x)'"
gate "$present" 0 "database present" -e NODE_ID=node1

new_volume leftover
in_volume "$leftover" "touch /data/library.db-wal"
gate "$leftover" 1 "part of the database was deleted" -e NODE_ID=node1

# The most important case: S3 unreachable must never become an empty start,
# not even for a node that is allowed to start fresh.
gate "$empty" 1 "litestream restore failed" -e NODE_ID=node1 -e ALLOW_FRESH_DB=true
gate "$empty" 1 "litestream restore failed" -e NODE_ID=node1
in_volume "$empty" "test ! -e /data/library.db" || { echo "FAIL: a database was created"; exit 1; }

echo "restore gate: $PASSED checks passed"
