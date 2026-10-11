#!/usr/bin/env bash
# End-to-end test of the whole system (spec §12). It starts a separate,
# throwaway cluster (projects e2e-central and e2e-node1..3, web on ports
# 4101-4103), runs five scenarios, and deletes the cluster again. Your own
# `make up` cluster is not touched. Needs Docker, curl, jq and `make images`.
set -euo pipefail
cd "$(dirname "$0")/.."

export PREFIX=e2e-
export SHARED_NETWORK=e2e-sqlite-prod
export POSTGRES_HOST_PORT=5434
export PROMETHEUS_HOST_PORT=9091
export HTTP_PORT_BASE=4100
NODE_FILES=(-f deploy/compose/node.yml -f deploy/compose/local/node.override.yml)
WORK=
cleanup() {
  NODE_ID=node9 docker compose -p e2e-node9 "${NODE_FILES[@]}" --env-file deploy/compose/local/node2.env down -v >/dev/null 2>&1 || true
  scripts/cluster.sh reset --yes >/dev/null 2>&1 || true
  [ -z "$WORK" ] || rm -rf "$WORK"
}
trap cleanup EXIT
cleanup # volumes left by a crashed earlier run would make the nodes look non-fresh
WORK=$(mktemp -d)

step() { printf '\n==> %s\n' "$*"; }
fail() { echo "E2E FAILED: $*" >&2; exit 1; }

# wait_until <seconds> <what> <command...>: retry the command every second.
wait_until() {
  local deadline=$((SECONDS + $1)) what=$2
  shift 2
  until "$@" >/dev/null 2>&1; do
    ((SECONDS < deadline)) || fail "timed out waiting for: $what"
    sleep 1
  done
}

port() { echo $((HTTP_PORT_BASE + ${1#node})); }
node_compose() {
  local node=$1
  shift
  HTTP_PORT=$(port "$node") docker compose -p "e2e-$node" "${NODE_FILES[@]}" --env-file "deploy/compose/local/$node.env" "$@"
}
node9_compose() { NODE_ID=node9 HTTP_PORT=4109 docker compose -p e2e-node9 "${NODE_FILES[@]}" --env-file deploy/compose/local/node2.env "$@"; }
central() { docker compose -p e2e-central -f deploy/compose/central.yml "$@"; }
sql() { central exec -T postgres psql -U sync -d library -tAc "$1"; }
healthy() { curl -fsS "http://127.0.0.1:$(port "$1")/api/health"; }
worker_healthy() { node_compose "$1" exec -T sync-worker curl -fsS http://127.0.0.1:9100/health; }
metric() { node_compose "$1" exec -T sync-worker curl -fsS http://127.0.0.1:9100/metrics | awk -v name="$2" '$1 == name { print $2 }'; }
in_postgres() { [ "$(sql "SELECT count(*) FROM books WHERE node_id = '$1' AND title = '$2'")" = 1 ]; }
count_is() { [ "$(sql "SELECT count(*) FROM $2 WHERE node_id = '$1'")" = "$3" ]; }
# Successful Litestream uploads of a node so far, as Prometheus last saw them.
put_count() {
  curl -fsS -G "http://127.0.0.1:$PROMETHEUS_HOST_PORT/api/v1/query" \
    --data-urlencode "query=sum(litestream_replica_operation_total{operation=\"PUT\",instance=\"$1-litestream:9090\"})" |
    jq -r '.data.result | map(.value[1] | tonumber) | add // 0'
}
put_above() { [ "$(put_count "$1")" -gt "$2" ]; }
targets_up() {
  curl -fsS "http://127.0.0.1:$PROMETHEUS_HOST_PORT/api/v1/targets" |
    jq -e '[.data.activeTargets[] | select(.health == "up")] | length == 6'
}

login() {
  node_compose "$1" exec -T web sh -c 'printf "password123\n" | library-web create-user admin --password-stdin' >/dev/null
  curl -fsS -c "$WORK/$1.cookies" -H 'content-type: application/json' \
    -d '{"username":"admin","password":"password123"}' "http://127.0.0.1:$(port "$1")/api/auth/login" >/dev/null
}
add_book() {
  jq -n --arg title "$2" '{title: $title, author: "E2E", isbn: null, published_year: 2026, total_copies: 1}' |
    curl -fsS -b "$WORK/$1.cookies" -H 'content-type: application/json' -d @- \
      "http://127.0.0.1:$(port "$1")/api/books" >/dev/null
}

step "start the cluster"
scripts/cluster.sh up
for n in node1 node2 node3; do wait_until 120 "$n healthy" healthy "$n"; done
for n in node1 node2 node3; do wait_until 120 "$n's worker healthy" worker_healthy "$n"; done

step "1. writes on every node reach Postgres under the right node_id"
node_compose node1 exec -T web library-web seed >/dev/null
wait_until 30 "node1's 12 seeded books in Postgres" count_is node1 books 12
for n in node1 node2 node3; do
  login "$n"
  add_book "$n" "E2E $n"
done
for n in node1 node2 node3; do wait_until 30 "the book from $n in Postgres" in_postgres "$n" "E2E $n"; done
count_is node2 books 1 || fail "node2 should have exactly its own book"
wait_until 60 "Prometheus scraping all 6 targets" targets_up

step "2. VM lost: delete node2's disk and start it again"
node_compose node2 down -v # a clean stop: Litestream uploads the last changes first
node_compose node2 up -d   # ALLOW_FRESH_DB=false: it must restore from S3
wait_until 120 "node2 healthy after the restore" healthy node2
node_compose node2 logs restore | grep "restored /data/library.db" >/dev/null || fail "node2 was not restored from S3"
curl -fsS -b "$WORK/node2.cookies" "http://127.0.0.1:$(port node2)/api/books?q=E2E" | jq -e '.total == 1' >/dev/null ||
  fail "node2's book (and its login session) should be back"
wait_until 120 "node2's worker healthy" worker_healthy node2
add_book node2 "E2E node2 after restore"
wait_until 30 "node2's new book in Postgres" in_postgres node2 "E2E node2 after restore"
# Read only now: at least one full cycle has run since the restore.
[ "$(metric node2 sync_resyncs_total)" = 0 ] || fail "a clean restore must not need a resync"

step "3. an empty disk with no backup refuses to start"
ALLOW_FRESH_DB=false node9_compose up -d >/dev/null 2>&1 || true
node9_compose logs restore 2>&1 | grep "No database and no backup for node 'node9'" >/dev/null || fail "the restore gate should refuse node9"
if node9_compose ps --status running --services 2>/dev/null | grep -x web >/dev/null; then
  fail "web must not start when the restore gate refuses"
fi

step "4. Postgres down: the app keeps working and the worker catches up"
central stop postgres
puts_before=$(put_count node1)
add_book node1 "E2E during outage" # succeeds: the app never needs Postgres
sleep 5
central start postgres
wait_until 120 "the outage book in Postgres" in_postgres node1 "E2E during outage"

step "5. restore drill"
# The drill reads from S3, so the outage book must be uploaded first. A
# rising PUT counter (successful uploads only) shows that; Prometheus sees
# it within one 5s scrape.
wait_until 60 "Litestream to upload node1's outage book" put_above node1 "$puts_before"
sleep 6 # the last upload may still be in flight after the counter first rises
scripts/restore-drill.sh node1 --strict

printf '\nE2E PASSED\n'
