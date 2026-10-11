#!/usr/bin/env bash
# Load test of the web app, or of a whole node, limited to 2 CPUs and 4 GB
# (docs/LOADTEST.md). LT_MODE=web (default) starts nginx and the web app (project
# lt-web), with SQLite inside the app, or with Postgres in the same box when
# LT_DB=postgres; LT_MODE=node starts a node with Litestream and the sync worker
# (projects lt-central and lt-node1, web on port 4201). Either way it seeds
# the database through the API, drives it with k6 in Docker and finds the highest load
# that still meets the pass rule. Your own `make up` cluster and the e2e
# cluster are not touched. Needs Docker, curl, jq, awk and `make images`.
#   scripts/loadtest.sh up
#   scripts/loadtest.sh users N | rps R
#   scripts/loadtest.sh search users|rps [START] [FACTOR]
#   scripts/loadtest.sh down
set -euo pipefail
cd "$(dirname "$0")/.."

# PREFIX names every resource of one box (projects, containers, networks, volumes, state
# dir), so two boxes (PREFIX=lt- and PREFIX=lt-pg-) can run side by side. It always starts
# with lt- and ends with -.
PREFIX="${PREFIX:-lt-}"
[[ $PREFIX =~ ^lt-([a-z0-9-]*-)?$ ]] || { echo "LOADTEST: usage: PREFIX must start with lt- and end with -, e.g. lt- or lt-pg- (got '$PREFIX')" >&2; exit 2; }
export PREFIX
export SHARED_NETWORK="${PREFIX}sqlite-prod"
export POSTGRES_HOST_PORT=5436
export PROMETHEUS_HOST_PORT=9092
export HTTP_PORT_BASE=4200
export NODE_CPUS="${NODE_CPUS:-6,7}"          # the node: 2 efficiency cores (4.4 GHz)
export CENTRAL_CPUS="${CENTRAL_CPUS:-8-13}"   # Postgres, Garage, Prometheus
K6_CPUS="${K6_CPUS:-0-3}"                     # k6: performance cores (5.1 GHz)
K6_IMAGE="${K6_IMAGE:-grafana/k6:1.3.0}"
LT_MODE="${LT_MODE:-web}"
[[ $LT_MODE == web || $LT_MODE == node ]] || { echo "LOADTEST: LT_MODE must be web or node" >&2; exit 2; }
LT_DB="${LT_DB:-sqlite}"
[[ $LT_DB == sqlite || $LT_DB == postgres ]] || { echo "LOADTEST: LT_DB must be sqlite or postgres" >&2; exit 2; }
[[ $LT_MODE == web || $LT_DB == sqlite ]] || { echo "LOADTEST: usage: LT_DB=postgres works only with LT_MODE=web (node mode has its own Postgres)" >&2; exit 2; }
# As seen from the k6 container: through nginx in web mode, as a client would.
if [ "$LT_MODE" = web ]; then BASE_URL="${BASE_URL:-http://nginx:80}"; else BASE_URL="${BASE_URL:-http://web:4000}"; fi
HTTP_PORT=$((HTTP_PORT_BASE + 1))
PROM_URL="http://127.0.0.1:$PROMETHEUS_HOST_PORT"
if [ "$LT_MODE" = web ]; then
  K6_NETWORK="${PREFIX}web_default"
  # web first: WEB is NODE_CONTAINERS[0].
  NODE_CONTAINERS=("${PREFIX}web-web-1" "${PREFIX}web-nginx-1")
  [ "$LT_DB" = sqlite ] || NODE_CONTAINERS+=("${PREFIX}web-postgres-1")
  # The app image built by `make loadtest-image` (it has the postgres feature).
  export WEB_IMAGE="${WEB_IMAGE:-sqlite-production/web:lt}"
else
  K6_NETWORK="${PREFIX}node1_default"
  NODE_CONTAINERS=("${PREFIX}node1-web-1" "${PREFIX}node1-litestream-1" "${PREFIX}node1-sync-worker-1")
fi
WEB="${NODE_CONTAINERS[0]}"
WORKER="${PREFIX}node1-sync-worker-1"
NODE_FILES=(-f deploy/compose/node.yml -f deploy/compose/local/node.override.yml -f deploy/compose/local/loadtest.node.yml)

# LOCAL-ONLY test credentials of the throwaway load-test database.
export LT_ADMIN_USER=admin
export LT_ADMIN_PASSWORD="${LT_ADMIN_PASSWORD:-loadtest-local-only}"

# Seed sizes (up) and the settings of a level.
BOOKS="${BOOKS:-50000}" MEMBERS="${MEMBERS:-10000}" LOANS="${LOANS:-20000}" SEED_VUS="${SEED_VUS:-16}"
RAMP="${RAMP:-60}" HOLD="${HOLD:-300}"
P95_MS="${P95_MS:-500}" P99_MS="${P99_MS:-1000}" ERR_PCT="${ERR_PCT:-1}" DROP_PCT="${DROP_PCT:-1}"
PAUSE_S="${PAUSE_S:-30}"           # between levels, after the sync worker has drained
SAMPLE_S="${SAMPLE_S:-5}"
SESSION_POOL="${SESSION_POOL:-50}"   # rps model: sessions logged in at setup
DRAIN_TIMEOUT_S="${DRAIN_TIMEOUT_S:-900}"
MAX_LEVEL="${MAX_LEVEL:-100000}"   # the search gives up above this

RESULTS_ROOT=loadtest/results
STATE_DIR=$RESULTS_ROOT/.state
[ "$PREFIX" = lt- ] || STATE_DIR=$RESULTS_ROOT/.state-$PREFIX
SAMPLER_PID=
EVENTS_PID=
WORK=$(mktemp -d)
RUN_DIR=          # results of the running command, for the summary of an aborted run
RUN_RESULT=null   # the search result so far, as JSON
SUMMARY_DONE=false
ABORT_REASON=
LEVEL_PASS=       # set by run_level: true or false
on_exit() {
  local rc=$?
  trap - EXIT
  [ -z "$SAMPLER_PID" ] || kill "$SAMPLER_PID" 2>/dev/null || true
  [ -z "$EVENTS_PID" ] || kill "$EVENTS_PID" 2>/dev/null || true
  docker rm -f "${PREFIX}k6-$$" >/dev/null 2>&1 || true
  # Whatever ended the run early, keep what was measured.
  if [ -n "$RUN_DIR" ] && [ "$SUMMARY_DONE" != true ] && [ -s "$RUN_DIR/levels.jsonl" ]; then
    write_summary "$RUN_DIR" "$RUN_DIR/levels.jsonl" "$RUN_RESULT" "${ABORT_REASON:-interrupted or failed (exit $rc)}" >&2 || true
  fi
  rm -rf "$WORK"
  exit "$rc"
}
trap on_exit EXIT
trap 'exit 130' INT TERM

step() { printf '\n==> %s\n' "$*"; }
die() { ABORT_REASON="$*"; echo "LOADTEST: $*" >&2; exit 2; }
now() { date +%s.%N; }
elapsed_since() { awk -v a="$(now)" -v b="$1" 'BEGIN { printf "%.1f", a - b }'; }

# wait_until <seconds> <what> <command...>: retry the command every second.
wait_until() {
  local deadline=$((SECONDS + $1)) what=$2
  shift 2
  until "$@" >/dev/null 2>&1; do
    ((SECONDS < deadline)) || die "timed out waiting for: $what"
    sleep 1
  done
}

WEB_FILES=(-f deploy/compose/local/loadtest.web.yml)
[ "$LT_DB" = sqlite ] || WEB_FILES+=(-f deploy/compose/local/loadtest.web-pg.yml)
web_compose() { docker compose -p "${PREFIX}web" "${WEB_FILES[@]}" "$@"; }
node_compose() { HTTP_PORT=$HTTP_PORT docker compose -p "${PREFIX}node1" "${NODE_FILES[@]}" --env-file deploy/compose/local/node1.env "$@"; }
central() { docker compose -p "${PREFIX}central" -f deploy/compose/central.yml -f deploy/compose/local/loadtest.central.yml "$@"; }

# Cores in a list like "0-3,6" (one per line).
expand_cpus() {
  local part
  for part in ${1//,/ }; do
    if [[ $part == *-* ]]; then seq "${part%-*}" "${part#*-}"; else echo "$part"; fi
  done
}
check_cpus() {
  local all
  all=$({
    expand_cpus "$NODE_CPUS" | sed 's/^/node /'
    [ "$LT_MODE" = web ] || expand_cpus "$CENTRAL_CPUS" | sed 's/^/central /'
    expand_cpus "$K6_CPUS" | sed 's/^/k6 /'
  })
  echo "$all" | awk '$2 >= 14 { print "core " $2 " (" $1 ") is a 2.5 GHz low-power core"; bad = 1 } END { exit bad }' >&2 || die "never use cores 14 and 15"
  [ -z "$(echo "$all" | awk '{ print $2 }' | sort | uniq -d)" ] || die "NODE_CPUS, CENTRAL_CPUS and K6_CPUS must not share cores"
}

# Where the runner reaches web from the host: the published port in node mode;
# in web mode nothing is published, so the container's address on its network.
host_url() {
  if [ "$LT_MODE" = node ]; then echo "http://127.0.0.1:$HTTP_PORT"; return; fi
  local ip
  ip=$(docker inspect -f "{{(index .NetworkSettings.Networks \"$K6_NETWORK\").IPAddress}}" "$WEB" 2>/dev/null) || return 1
  [ -n "$ip" ] || return 1
  echo "http://$ip:4000"
}
healthy() { curl -fsS "$(host_url)/api/health"; }

# Lines in nginx's access log (web mode), or JSON null. nginx flushes its
# buffered log every 5 s, so wait that long first.
nginx_log_lines() {
  if [ "$LT_MODE" != web ]; then echo null; return; fi
  sleep 6
  docker exec "${PREFIX}web-nginx-1" sh -c 'wc -l </var/log/nginx/lt-access.log' | tr -d ' \r\n'
}

worker_metric() { docker exec "$WORKER" curl -fsS http://127.0.0.1:9100/metrics | awk -v name="$1" '$1 == name { print $2 }'; }
# A value from Prometheus (up to one 5 s scrape old), or "null".
prom() {
  curl -fsS -G "$PROM_URL/api/v1/query" --data-urlencode "query=$1" 2>/dev/null |
    jq -r '.data.result | if length == 0 then "null" else map(.value[1] | tonumber) | add end' 2>/dev/null || echo null
}

# Wait until the sync worker finished a cycle after $1 (epoch) and has nothing pending.
wait_drain() {
  local since=$1 deadline=$((SECONDS + DRAIN_TIMEOUT_S)) pending last
  while :; do
    pending=$(worker_metric sync_pending_changes 2>/dev/null || true)
    last=$(worker_metric sync_last_success_timestamp_seconds 2>/dev/null || true)
    if [ -n "$pending" ] && [ -n "$last" ] &&
      awk -v p="$pending" -v l="$last" -v s="$since" 'BEGIN { exit !(p == 0 && l > s) }'; then
      return 0
    fi
    ((SECONDS < deadline)) || die "the sync worker did not drain within ${DRAIN_TIMEOUT_S}s (pending=$pending)"
    sleep 2
  done
}

# Wait until Litestream's local position (txid) and its successful S3 uploads
# (PUT counter) have not moved for 3 samples in a row: it has uploaded what the
# last write produced. A counter that never moved at all is not "quiet".
wait_litestream() {
  local deadline=$((SECONDS + 180)) prev= cur stable=0
  while :; do
    cur="$(prom 'litestream_txid') $(prom 'sum(litestream_replica_operation_total{operation="PUT"})')"
    if [ "$cur" = "$prev" ] && [[ $cur != *null* ]]; then stable=$((stable + 1)); else stable=0; fi
    [ "$stable" -ge 3 ] && return 0
    prev=$cur
    ((SECONDS < deadline)) || die "Litestream kept changing for 180s (txid/PUTs: $cur)"
    sleep 5
  done
}

# The size of the database as the API and Postgres see it, as JSON.
db_rows() {
  local url
  url=$(host_url) || return 1
  curl -fsS -c "$WORK/jar" -H 'content-type: application/json' \
    -d "{\"username\":\"$LT_ADMIN_USER\",\"password\":\"$LT_ADMIN_PASSWORD\"}" "$url/api/auth/login" >/dev/null
  api() { curl -fsS -b "$WORK/jar" "$url$1" | jq .total; }
  pg() { docker exec "${PREFIX}central-postgres-1" psql -U sync -d library -tAc "SELECT count(*) FROM $1 WHERE node_id = 'node1'"; }
  if [ "$LT_MODE" = web ]; then
    jq -cn --argjson books "$(api '/api/books?per_page=1')" --argjson members "$(api '/api/members?per_page=1')" \
      --argjson loans "$(api '/api/loans?status=all&per_page=1')" --argjson active "$(api '/api/loans?status=active&per_page=1')" \
      '{books: $books, members: $members, loans: $loans, active_loans: $active, postgres: null}'
    return
  fi
  jq -cn --argjson books "$(api '/api/books?per_page=1')" --argjson members "$(api '/api/members?per_page=1')" \
    --argjson loans "$(api '/api/loans?status=all&per_page=1')" --argjson active "$(api '/api/loans?status=active&per_page=1')" \
    --argjson pg_books "$(pg books)" --argjson pg_members "$(pg members)" --argjson pg_loans "$(pg loans)" \
    '{books: $books, members: $members, loans: $loans, active_loans: $active, postgres: {books: $pg_books, members: $pg_members, loans: $pg_loans}}'
}

# k6_run <out-dir> <script> [docker run args...]: k6 on its own cores, inside
# the node's Docker network, so no docker-proxy is on the path.
k6_run() {
  local out=$1 script=$2
  shift 2
  [[ $out == /* ]] || out=$PWD/$out
  docker run --rm --name "${PREFIX}k6-$$" --cpuset-cpus "$K6_CPUS" --network "$K6_NETWORK" --user "$(id -u):$(id -g)" \
    -v "$PWD/loadtest:/lt:ro" -v "$out:/out" -e BASE_URL="$BASE_URL" -e OUT=/out \
    -e LT_ADMIN_USER -e LT_ADMIN_PASSWORD "$@" "$K6_IMAGE" run --quiet "/lt/$script"
}

# ---------------------------------------------------------------- up / down

node_limits_proof() {
  local name cpus mem swap
  echo "container                cpuset  memory   memswap  restart"
  for name in "${PREFIX}node1-restore-1" "${NODE_CONTAINERS[@]}"; do
    docker inspect -f '{{.Name}} {{.HostConfig.CpusetCpus}} {{.HostConfig.Memory}} {{.HostConfig.MemorySwap}} {{.HostConfig.RestartPolicy.Name}}' "$name" |
      awk '{ printf "%-24s %-7s %-8s %-8s %s\n", substr($1, 2), $2, $3 / 1048576 "m", $4 / 1048576 "m", $5 }'
  done
  echo "central:"
  for name in postgres garage prometheus; do
    docker inspect -f '{{.Name}} cpuset={{.HostConfig.CpusetCpus}}' "${PREFIX}central-$name-1"
  done
  local total expected_cpus
  total=$(for name in "${NODE_CONTAINERS[@]}"; do docker inspect -f '{{.HostConfig.Memory}}' "$name"; done | awk '{ s += $1 } END { print s / 1073741824 }')
  echo "memory limits of the running node containers add up to ${total} GB (restore is a one-shot that has finished)"
  expected_cpus=$(expand_cpus "$NODE_CPUS" | wc -l)
  echo "nproc in the web container: $(docker exec "${NODE_CONTAINERS[0]}" nproc) (expected $expected_cpus)"
  for name in "${NODE_CONTAINERS[@]}"; do
    [ "$(docker inspect -f '{{.HostConfig.CpusetCpus}}' "$name")" = "$NODE_CPUS" ] || die "$name has the wrong cpuset"
    [ "$(docker inspect -f '{{.HostConfig.Memory}}' "$name")" = "$(docker inspect -f '{{.HostConfig.MemorySwap}}' "$name")" ] || die "$name may use swap"
  done
  [ "$(docker exec "${NODE_CONTAINERS[0]}" nproc)" = "$expected_cpus" ] || die "nproc in the web container is not $expected_cpus"
}

# The web-mode proof: cpuset, memory and swap of every box container, the sum of
# the memory limits (4096 MiB), and nproc.
web_limits_proof() {
  local name expected_cpus total
  echo "container                cpuset  memory   memswap  restart"
  for name in "${NODE_CONTAINERS[@]}"; do
    docker inspect -f '{{.Name}} {{.HostConfig.CpusetCpus}} {{.HostConfig.Memory}} {{.HostConfig.MemorySwap}} {{.HostConfig.RestartPolicy.Name}}' "$name" |
      awk '{ printf "%-24s %-7s %-8s %-8s %s\n", substr($1, 2), $2, $3 / 1048576 "m", $4 / 1048576 "m", $5 }'
  done
  total=$(for name in "${NODE_CONTAINERS[@]}"; do docker inspect -f '{{.HostConfig.Memory}}' "$name"; done | awk '{ s += $1 } END { print s / 1048576 }')
  echo "memory limits of the box add up to ${total} MiB (expected 4096)"
  expected_cpus=$(expand_cpus "$NODE_CPUS" | wc -l)
  echo "nproc in the web container: $(docker exec "$WEB" nproc) (expected $expected_cpus)"
  for name in "${NODE_CONTAINERS[@]}"; do
    [ "$(docker inspect -f '{{.HostConfig.CpusetCpus}}' "$name")" = "$NODE_CPUS" ] || die "$name has the wrong cpuset"
    [ "$(docker inspect -f '{{.HostConfig.Memory}}' "$name")" = "$(docker inspect -f '{{.HostConfig.MemorySwap}}' "$name")" ] || die "$name may use swap"
  done
  [ "$total" = 4096 ] || die "the box's memory limits add up to ${total} MiB, not 4096"
  [ "$(docker exec "$WEB" nproc)" = "$expected_cpus" ] || die "nproc in the web container is not $expected_cpus"
}

cmd_up() {
  check_cpus
  local img images=(web sync-worker litestream)
  if [ "$LT_MODE" = node ]; then
    for img in "${images[@]}"; do
      docker image inspect "sqlite-production/$img:dev" >/dev/null 2>&1 || die "image sqlite-production/$img:dev is missing: run make images"
    done
  else
    docker image inspect "$WEB_IMAGE" >/dev/null 2>&1 || die "image $WEB_IMAGE is missing: run make loadtest-image"
  fi
  docker image inspect "$K6_IMAGE" >/dev/null 2>&1 || docker pull "$K6_IMAGE" >/dev/null || die "cannot pull $K6_IMAGE"
  if docker volume inspect "${PREFIX}node1_data" >/dev/null 2>&1 || docker volume inspect "${PREFIX}web_data" >/dev/null 2>&1 || docker volume inspect "${PREFIX}web_pgdata" >/dev/null 2>&1 || [ -f "$STATE_DIR/seed.env" ]; then
    die "a load-test cluster already exists: scripts/loadtest.sh down first"
  fi
  ((LOANS <= BOOKS)) || die "LOANS ($LOANS) must not exceed BOOKS ($BOOKS)"

  local create_admin="printf '%s\n' \"\$PW\" | library-web create-user $LT_ADMIN_USER --password-stdin"
  if [ "$LT_MODE" = web ]; then
    step "start nginx and the web app ($LT_DB) in one box (cores $NODE_CPUS, 4 GB)"
    # A web that cannot start (for example an image without the postgres
    # feature) restarts forever, so bound the wait and show why.
    web_compose up -d --wait --wait-timeout 180 || { web_compose logs --tail 20 web >&2 || true; die "the box did not become healthy (web logs above)"; }
    # Local-only test credentials, like the e2e test.
    web_compose exec -T -e PW="$LT_ADMIN_PASSWORD" web sh -c "$create_admin" >/dev/null
  else
    step "start Postgres, Garage and Prometheus (cores $CENTRAL_CPUS)"
    central up -d --wait postgres
    central up -d
    PREFIX="$PREFIX" deploy/garage/init.sh >/dev/null
    step "start node1 (cores $NODE_CPUS, 4 GB)"
    ALLOW_FRESH_DB=true node_compose up -d --wait
    wait_until 120 "node1's worker healthy" docker exec "$WORKER" curl -fsS http://127.0.0.1:9100/health
    # Local-only test credentials, like the e2e test.
    node_compose exec -T -e PW="$LT_ADMIN_PASSWORD" web sh -c "$create_admin" >/dev/null
  fi

  mkdir -p "$STATE_DIR"
  local phase count started
  for phase in books members loans; do
    case $phase in books) count=$BOOKS ;; members) count=$MEMBERS ;; loans) count=$LOANS ;; esac
    step "seed $count $phase through the API ($SEED_VUS VUs)"
    started=$(now)
    k6_run "$STATE_DIR" seed.js -e PHASE="$phase" -e BOOKS="$BOOKS" -e MEMBERS="$MEMBERS" -e LOANS="$LOANS" -e SEED_VUS="$SEED_VUS" \
      >"$STATE_DIR/seed-$phase.log" 2>&1 || { tail -20 "$STATE_DIR/seed-$phase.log" >&2; die "seeding $phase failed (log: $STATE_DIR/seed-$phase.log)"; }
    echo "$phase seeded in $(elapsed_since "$started") s"
  done
  local seeded_at
  seeded_at=$(now)

  local rows
  if [ "$LT_MODE" = node ]; then
    step "wait for the sync worker and Litestream"
    wait_drain "$seeded_at"
    echo "sync worker drained after $(elapsed_since "$seeded_at") s"
    wait_litestream
    echo "Litestream has uploaded everything ($(elapsed_since "$seeded_at") s after the last write)"
    rows=$(db_rows)
    echo "rows: $rows"
    [ "$(jq -c '[.books, .members, .loans]' <<<"$rows")" = "$(jq -c '[.postgres.books, .postgres.members, .postgres.loans]' <<<"$rows")" ] ||
      die "Postgres does not have every row after the drain"
  else
    rows=$(db_rows) || die "cannot read the row counts from the API"
    echo "rows: $rows"
  fi
  printf 'SEED_BOOKS=%s\nSEED_MEMBERS=%s\nSEED_LOANS=%s\n' "$BOOKS" "$MEMBERS" "$LOANS" >"$STATE_DIR/seed.env"

  step "limit proof"
  if [ "$LT_MODE" = web ]; then
    web_limits_proof
    printf '\nload-test box ready (mode web, %s): k6 target %s, web %s\n' "$LT_DB" "$BASE_URL" "$(host_url)"
  else
    node_limits_proof
    printf '\nload-test cluster ready: web http://127.0.0.1:%s, Prometheus %s, Postgres 127.0.0.1:%s\n' "$HTTP_PORT" "$PROM_URL" "$POSTGRES_HOST_PORT"
  fi
}

cmd_down() {
  # Every step runs, even if an earlier one fails.
  # One down cleans whatever exists, whichever mode made it.
  docker compose -p "${PREFIX}web" -f deploy/compose/local/loadtest.web.yml -f deploy/compose/local/loadtest.web-pg.yml down -v || echo "warning: could not remove the ${PREFIX}web project" >&2
  node_compose down -v || echo "warning: could not remove the ${PREFIX}node1 project" >&2
  central down -v || echo "warning: could not remove the ${PREFIX}central project" >&2
  docker rm -f $(docker ps -aq --filter "name=^${PREFIX}k6-") >/dev/null 2>&1 || true
  rm -rf "$STATE_DIR" || echo "warning: could not remove $STATE_DIR" >&2
  echo "load-test cluster removed (results in $RESULTS_ROOT are kept)"
}


# ------------------------------------------------------------------- levels

# Samples on a fixed SAMPLE_S tick, as JSON lines: container CPU and memory (the
# node's, and k6's while it runs), the sync worker's backlog, and Litestream's
# position and upload counter. The queries run at the same time, so a slow
# `docker stats` (about 2 s) does not stretch the tick.
sample_loop() {
  local file=$1 t next=$(now) names pids delay
  while :; do
    t=$next
    names=("${NODE_CONTAINERS[@]}")
    if [ "$(docker inspect -f '{{.State.Running}}' "${PREFIX}k6-$$" 2>/dev/null)" = true ]; then names+=("${PREFIX}k6-$$"); fi
    (docker stats --no-stream --format '{{json .}}' "${names[@]}" 2>/dev/null | jq -cs . >"$WORK/s.stats") &
    if [ "$LT_MODE" = node ]; then
      (prom 'sync_pending_changes' >"$WORK/s.pending") &
      (prom 'sync_lag_seconds' >"$WORK/s.lag") &
      (prom 'sum(litestream_replica_operation_total{operation="PUT"})' >"$WORK/s.puts") &
      (prom 'litestream_txid' >"$WORK/s.txid") &
    else
      echo null | tee "$WORK/s.pending" "$WORK/s.lag" "$WORK/s.puts" "$WORK/s.txid" >/dev/null
    fi
    wait
    jq -cn --arg t "$t" --argjson stats "$(cat "$WORK/s.stats")" \
      --argjson pending "$(cat "$WORK/s.pending")" --argjson lag "$(cat "$WORK/s.lag")" \
      --argjson puts "$(cat "$WORK/s.puts")" --argjson txid "$(cat "$WORK/s.txid")" \
      '{t: ($t | tonumber), stats: $stats, pending: $pending, lag: $lag, puts: $puts, txid: $txid}' >>"$file" || true
    next=$(awk -v n="$next" -v s="$SAMPLE_S" 'BEGIN { printf "%.3f", n + s }')
    delay=$(awk -v n="$next" -v c="$(now)" -v s="$SAMPLE_S" 'BEGIN { d = n - c; if (d < 0) { d = 0; n = c } printf "%.3f", d }')
    # A tick that ran over starts the next one at once; it does not queue up.
    sleep "$delay"
  done
}

restart_state() {
  local name
  for name in "${NODE_CONTAINERS[@]}"; do
    docker inspect -f '{"name":"{{.Name}}","restarts":{{.RestartCount}},"oom_killed":{{.State.OOMKilled}},"running":{{.State.Running}}}' "$name"
  done | jq -cs .
}

# run_level users|rps <value> <dir>: one level. Writes <dir>/level.json and sets
# LEVEL_PASS to true or false. A failing level is a result; anything that stops
# the test from measuring (k6 died, no summary, a failed query or jq program)
# ends the script with `die`, never as a failed level.
run_level() {
  local kind=$1 value=$2 dir=$3 env_args started finished rc=0 nginx_before nginx_after nginx_log drain restarts_before restarts_after rows_before rows_after setup_ms setup_json
  mkdir -p "$dir"
  # shellcheck disable=SC1091
  . "$STATE_DIR/seed.env" 2>/dev/null || die "no seeded cluster: scripts/loadtest.sh up first"
  healthy >/dev/null || die "web is not answering (is the cluster up?)"
  if [ "$kind" = users ]; then env_args=(-e MODE=users -e USERS="$value"); else env_args=(-e MODE=rps -e RPS="$value" -e SESSION_POOL="$SESSION_POOL"); fi

  printf '\n==> level %s %s: ramp %ss, hold %ss\n' "$kind" "$value" "$RAMP" "$HOLD"
  # A node that is still coming back from an earlier crash would blame this
  # level for a restart that is not its own.
  wait_until 180 "web healthy" healthy
  restarts_before=$(restart_state) || die "cannot read the container state"
  nginx_before=$(nginx_log_lines) || die "cannot read nginx's access log"
  rows_before=$(db_rows) || die "cannot read the row counts before the level (API or Postgres)"
  : >"$dir/samples.jsonl"
  sample_loop "$dir/samples.jsonl" &
  SAMPLER_PID=$!
  # Docker's own record of deaths and OOM kills of the node containers, to tell them apart.
  local filters=() name
  for name in "${NODE_CONTAINERS[@]}"; do filters+=(--filter "container=$name"); done
  docker events --filter type=container --filter event=die --filter event=oom "${filters[@]}" --format '{{json .}}' >"$dir/events.jsonl" 2>/dev/null &
  EVENTS_PID=$!
  started=$(now)
  k6_run "$dir" user.js "${env_args[@]}" -e RAMP="$RAMP" -e HOLD="$HOLD" -e SEED_BOOKS="$SEED_BOOKS" >"$dir/k6.log" 2>&1 || rc=$?
  finished=$(now)
  kill "$SAMPLER_PID" 2>/dev/null || true
  wait "$SAMPLER_PID" 2>/dev/null || true
  SAMPLER_PID=
  # k6 exits 99 when a threshold failed. The thresholds here never fail, except
  # a sub-metric with no samples (NaN), so only a missing summary is an error.
  [ -s "$dir/k6-summary.json" ] || { tail -20 "$dir/k6.log" >&2; die "k6 produced no summary (exit $rc, log: $dir/k6.log)"; }

  # A node killed during the level may restart only after k6 has finished (Docker
  # backs off between restarts), so look at the restart count after the node is back.
  if [ "$LT_MODE" = node ]; then
    wait_drain "$finished"
    drain=$(elapsed_since "$finished")
  else
    drain=null # no sync worker in web mode
  fi
  wait_until 180 "web healthy" healthy
  restarts_after=$(restart_state) || die "cannot read the container state"
  kill "$EVENTS_PID" 2>/dev/null || true
  wait "$EVENTS_PID" 2>/dev/null || true
  EVENTS_PID=
  rows_after=$(db_rows) || die "cannot read the row counts after the level (API or Postgres)"
  nginx_after=$(nginx_log_lines) || die "cannot read nginx's access log"
  nginx_log=$(jq -cn --argjson b "$nginx_before" --argjson a "$nginx_after" 'if $b == null then null else {lines_before: $b, lines_after: $a, lines: ($a - $b)} end')
  setup_ms=$(sed -n 's/.*LT_SETUP_DONE \([0-9]*\) .*/\1/p' "$dir/k6.log" | head -1)
  setup_json=$(sed -n 's/.*LT_SETUP_DONE [0-9]* \(.*\)" source=console.*/\1/p' "$dir/k6.log" | head -1 | sed 's/\\"/"/g')
  [ -n "$setup_ms" ] && jq -e . >/dev/null 2>&1 <<<"$setup_json" || die "k6 did not report its start (log: $dir/k6.log)"

  jq -n --slurpfile k6 "$dir/k6-summary.json" --slurpfile samples "$dir/samples.jsonl" --slurpfile events "$dir/events.jsonl" \
    --arg mode "$LT_MODE" --arg prefix "$PREFIX" --argjson ncont "${#NODE_CONTAINERS[@]}" \
    --arg kind "$kind" --argjson value "$value" --argjson ramp "$RAMP" --argjson hold "$HOLD" \
    --argjson start_s "$(awk -v ms="$setup_ms" 'BEGIN { printf "%.3f", ms / 1000 }')" --argjson drain "$drain" \
    --argjson setup "$setup_json" \
    --argjson p95 "$P95_MS" --argjson p99 "$P99_MS" --argjson err "$ERR_PCT" --argjson drop "$DROP_PCT" \
    --argjson before "$restarts_before" --argjson after "$restarts_after" \
    --argjson rows_before "$rows_before" --argjson rows_after "$rows_after" --argjson nginx_log "$nginx_log" \
    -f "loadtest/level.jq" >"$dir/level.json" || die "could not compute the level result (loadtest/level.jq; inputs in $dir)"
  jq -e '.pass | type == "boolean"' "$dir/level.json" >/dev/null 2>&1 || die "level.json is empty or malformed ($dir/level.json)"
  jq -r --arg prefix "$PREFIX" -f loadtest/level-line.jq "$dir/level.json" || die "could not print the level result"
  LEVEL_PASS=$(jq -r .pass "$dir/level.json")
}

pause_between_levels() {
  echo "pausing ${PAUSE_S}s before the next level"
  sleep "$PAUSE_S"
}

# write_summary <dir> <levels-file> [search-result-json] [aborted-reason]
write_summary() {
  local dir=$1 levels=$2 result=${3:-null} aborted=${4:-}
  jq -s --argjson result "$result" --arg aborted "$aborted" --argjson p95 "$P95_MS" --argjson p99 "$P99_MS" --argjson err "$ERR_PCT" --argjson drop "$DROP_PCT" \
    --argjson ramp "$RAMP" --argjson hold "$HOLD" --arg node_cpus "$NODE_CPUS" --arg mode "$LT_MODE" --arg db "$([ "$LT_MODE" = web ] && echo "$LT_DB" || true)" \
    -f loadtest/summary.jq "$levels" >"$dir/summary.json" || return 1
  jq -r --arg prefix "$PREFIX" -f loadtest/summary-md.jq "$dir/summary.json" >"$dir/summary.md" || return 1
  SUMMARY_DONE=true
  echo
  echo "results: $dir (summary.md, summary.json, one directory per level)"
}

new_results_dir() {
  local dir="${RESULTS_DIR:-$RESULTS_ROOT/$(date -u +%Y%m%dT%H%M%SZ)$([ "$LT_MODE" = web ] && echo "-$LT_DB")$([ "$PREFIX" = lt- ] || echo "-${PREFIX%-}")}"
  mkdir -p "$dir" || die "cannot create the results directory $dir"
  echo "$dir"
}

cmd_level() {
  local kind=$1 value=$2 dir
  [[ $value =~ ^[1-9][0-9]*$ ]] || die "usage: loadtest.sh $kind <positive integer>"
  dir=$(new_results_dir)
  RUN_DIR=$dir
  : >"$dir/levels.jsonl"
  run_level "$kind" "$value" "$dir/${kind}-${value}"
  jq -c . "$dir/${kind}-${value}/level.json" >"$dir/levels.jsonl"
  write_summary "$dir" "$dir/levels.jsonl" || die "could not write the summary"
  cat "$dir/summary.md"
  [ "$LEVEL_PASS" = true ]
}

# ------------------------------------------------------------------- search

# level x factor, rounded up, at least level + 1.
scale_up() { awk -v l="$1" -v f="$2" 'BEGIN { n = int(l * f); if (n < l * f) n++; if (n <= l) n = l + 1; print n }'; }
# 90% of a level, rounded up, always below it, at least 1.
step_down_10() { awk -v b="$1" 'BEGIN { n = int(b * 0.9); if (n < b * 0.9) n++; if (n >= b) n = b - 1; if (n < 1) n = 1; print n }'; }
# level / factor, rounded down, always below the level, at least 1.
scale_down() { awk -v l="$1" -v f="$2" 'BEGIN { n = int(l / f); if (n >= l) n = l - 1; if (n < 1) n = 1; print n }'; }

cmd_search() {
  local usage="usage: scripts/loadtest.sh search users|rps [START] [FACTOR]  (START: positive integer, FACTOR: number greater than 1)"
  local kind=${1:-}
  [[ $kind == users || $kind == rps ]] || die "$usage"
  local default_start=100
  [ "$kind" = rps ] && default_start=50
  local start=${2:-$default_start} factor=${3:-2}
  [[ $start =~ ^[1-9][0-9]*$ ]] || die "$usage"
  [[ $factor =~ ^[0-9]+(\.[0-9]+)?$ ]] && awk -v f="$factor" 'BEGIN { exit !(f > 1) }' || die "$usage"
  local dir n=0 last_pass=0 first_fail=0 level status="" reason=""
  dir=$(new_results_dir)
  RUN_DIR=$dir
  : >"$dir/levels.jsonl"
  local rows_start
  rows_start=$(db_rows) || die "cannot read the row counts (API or Postgres)"
  RUN_RESULT=$(jq -cn --arg kind "$kind" --argjson rows_start "$rows_start" '{kind: $kind, status: "not finished", best_pass: null, first_fail: null, db_rows_start: $rows_start}')

  # one <level>: run it and record it. Sets LEVEL_PASS.
  one() {
    n=$((n + 1))
    local sub
    sub="$dir/$(printf '%02d' "$n")-$kind-$1"
    run_level "$kind" "$1" "$sub"
    jq -c --argjson n "$n" '. + {order: $n}' "$sub/level.json" >>"$dir/levels.jsonl"
  }

  step "search $kind: start $start, factor $factor"
  level=$start
  while :; do
    one "$level"
    if [ "$LEVEL_PASS" = true ]; then
      last_pass=$level
      level=$(scale_up "$level" "$factor")
      if ((level > MAX_LEVEL)); then status="no limit found up to $MAX_LEVEL"; break; fi
    else
      first_fail=$level
      break
    fi
    pause_between_levels
  done

  if [ -z "$status" ] && [ "$last_pass" = 0 ]; then
    # The first level already fails: step down until one passes. The level
    # shrinks every round, so this ends at level 1 at the latest.
    while [ "$level" -gt 1 ]; do
      pause_between_levels
      level=$(scale_down "$level" "$factor")
      one "$level"
      if [ "$LEVEL_PASS" = true ]; then last_pass=$level; break; fi
      first_fail=$level
    done
    [ "$last_pass" != 0 ] || status="no passing level (level $level fails)"
  fi

  if [ -z "$status" ]; then
    # Bisect until the gap is at most 10% of the highest pass.
    while awk -v p="$last_pass" -v f="$first_fail" 'BEGIN { exit !(f - p > 0.1 * p) }'; do
      level=$(((last_pass + first_fail) / 2))
      [ "$level" != "$last_pass" ] && [ "$level" != "$first_fail" ] || break
      pause_between_levels
      one "$level"
      if [ "$LEVEL_PASS" = true ]; then last_pass=$level; else first_fail=$level; fi
    done
    # Confirm the best pass; if it fails, step down 10% and confirm again, at most twice.
    local best=$last_pass attempt confirmed=false
    for attempt in 1 2 3; do
      pause_between_levels
      one "$best"
      if [ "$LEVEL_PASS" = true ]; then confirmed=true; break; fi
      reason="the pass did not repeat at $best"
      if [ "$best" -le 1 ] || [ "$attempt" = 3 ]; then break; fi
      best=$(step_down_10 "$best")
    done
    if [ "$confirmed" = true ]; then status="confirmed"; last_pass=$best; else status="no passing level ($reason)"; last_pass=0; fi
  fi

  local rows_end result best_rps
  rows_end=$(db_rows) || die "cannot read the row counts after the search (API or Postgres)"
  best_rps=$(jq -s --argjson best "$last_pass" '[.[] | select(.pass and .level == $best)] | last | .req_per_s // null' "$dir/levels.jsonl")
  result=$(jq -cn --arg status "$status" --argjson best "$last_pass" --argjson first_fail "$first_fail" --argjson best_rps "${best_rps:-null}" \
    --arg kind "$kind" --argjson rows_start "$rows_start" --argjson rows_end "$rows_end" \
    '{kind: $kind, status: $status, best_pass: (if $best == 0 then null else $best end), best_req_per_s: $best_rps, first_fail: (if $first_fail == 0 then null else $first_fail end), db_rows_start: $rows_start, db_rows_end: $rows_end}')
  RUN_RESULT=$result
  write_summary "$dir" "$dir/levels.jsonl" "$result" || die "could not write the summary"
  cat "$dir/summary.md"
  echo
  local unit=users
  [ "$kind" = rps ] && unit=actions/s
  if [ "$last_pass" != 0 ]; then
    echo "SEARCH RESULT ($kind): $status; highest passing level: $last_pass $unit (≈ ${best_rps:-?} req/s); lowest failing level: ${first_fail}"
  else
    echo "SEARCH RESULT ($kind): $status; lowest failing level: ${first_fail}"
  fi
  [ "$last_pass" != 0 ]
}

case "${1:-}" in
  up) cmd_up ;;
  users | rps) cmd_level "$1" "${2:?usage: loadtest.sh $1 N}" ;;
  search) shift; cmd_search "$@" ;;
  down) cmd_down ;;
  *)
    echo "usage: scripts/loadtest.sh up | users N | rps R | search users|rps [START] [FACTOR] | down" >&2
    exit 2
    ;;
esac
