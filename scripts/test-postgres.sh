#!/usr/bin/env bash
# Start (or reuse) a throwaway Postgres for the worker tests and print its URL.
# Its data lives in memory (tmpfs): removing the container deletes everything.
set -euo pipefail
NAME=sqlite-production-test-pg
PORT=55432
IMAGE=postgres:18.6

if ! docker ps --format '{{.Names}}' | grep -qx "$NAME"; then
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker run -d --name "$NAME" -e POSTGRES_PASSWORD=test \
    -p "127.0.0.1:$PORT:5432" --tmpfs /var/lib/postgresql "$IMAGE" >/dev/null
fi

# -h 127.0.0.1: during first start the image runs a temporary server on a unix
# socket only; waiting for TCP skips that phase.
for _ in $(seq 1 120); do
  if docker exec "$NAME" pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1; then
    echo "postgres://postgres:test@127.0.0.1:$PORT/postgres"
    exit 0
  fi
  sleep 0.5
done
echo "test Postgres did not become ready" >&2
exit 1
