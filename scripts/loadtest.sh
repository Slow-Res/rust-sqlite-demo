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
#   scripts/loadtest.sh ramp [START] [STEP] [STEP_S]     (WARMUP_S=120: an unjudged warm-up first)
#   scripts/loadtest.sh clockcheck
#   scripts/loadtest.sh seed-cache list|clear   (the seeded-database snapshots, see SEED_CACHE)
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
# SEED_CACHE=1 (web mode): the first `up` of a seed saves the seeded database as a tar under
# SEED_CACHE_DIR and every later `up` of the same seed restores it instead of seeding;
# 0 always seeds and never saves or restores.
SEED_CACHE="${SEED_CACHE:-1}"
[[ $SEED_CACHE == 0 || $SEED_CACHE == 1 ]] || { echo "LOADTEST: usage: SEED_CACHE must be 0 or 1 (got '$SEED_CACHE')" >&2; exit 2; }
SEED_CACHE_DIR=loadtest/.seed-cache
# ramp mode allows 2% errors (a step of many users logging in at once), the others 1%.
P95_MS="${P95_MS:-500}" P99_MS="${P99_MS:-1000}" ERR_PCT="${ERR_PCT:-$([ "${1:-}" = ramp ] && echo 2 || echo 1)}" DROP_PCT="${DROP_PCT:-1}"
PAUSE_S="${PAUSE_S:-30}"           # between levels, after the sync worker has drained
# The run starts only when the CPU package is below COOL_C (degrees C; 0 turns the gate
# off); after COOL_MAX_S seconds it warns and starts anyway.
COOL_C="${COOL_C:-55}" COOL_MAX_S="${COOL_MAX_S:-900}"
[[ $COOL_C =~ ^[0-9]+$ ]] || { echo "LOADTEST: usage: COOL_C must be a whole number of degrees C, 0 to turn the gate off (got '$COOL_C')" >&2; exit 2; }
[[ $COOL_MAX_S =~ ^[1-9][0-9]*$ ]] || { echo "LOADTEST: usage: COOL_MAX_S must be a positive integer (got '$COOL_MAX_S')" >&2; exit 2; }
SAMPLE_S="${SAMPLE_S:-5}"
SESSION_POOL="${SESSION_POOL:-50}"   # rps model: sessions logged in at setup
PAGING="${PAGING:-cursor}"           # browsing: cursor = "next page" (keyset), offset = random deep pages
[[ $PAGING == cursor || $PAGING == offset ]] || { echo "LOADTEST: PAGING must be cursor or offset" >&2; exit 2; }
MIX="${MIX:-nosearch}"               # action mix: nosearch = no book search (browse available + delete book instead), search = the earlier mix
[[ $MIX == nosearch || $MIX == search ]] || { echo "LOADTEST: MIX must be nosearch or search" >&2; exit 2; }
# DB_POOL: the app's database pool size (DATABASE_MAX_CONNECTIONS); empty leaves the app's
# default, so the variable is not passed at all. PG_WORK_MEM: Postgres' work_mem (kB, MB or GB).
DB_POOL="${DB_POOL:-}"
[[ -z $DB_POOL || $DB_POOL =~ ^[1-9][0-9]{0,4}$ ]] || { echo "LOADTEST: usage: DB_POOL must be a positive integer, e.g. 20 (got '$DB_POOL')" >&2; exit 2; }
[[ -z $DB_POOL || $LT_MODE == web ]] || { echo "LOADTEST: usage: DB_POOL works only with LT_MODE=web" >&2; exit 2; }
PG_WORK_MEM="${PG_WORK_MEM:-4MB}"
[[ $PG_WORK_MEM =~ ^[1-9][0-9]*(kB|MB|GB)$ ]] || { echo "LOADTEST: usage: PG_WORK_MEM must be an integer with a unit kB, MB or GB, e.g. 16MB (got '$PG_WORK_MEM')" >&2; exit 2; }
if [ -n "$DB_POOL" ]; then export DATABASE_MAX_CONNECTIONS="$DB_POOL"; else unset DATABASE_MAX_CONNECTIONS; fi
DRAIN_TIMEOUT_S="${DRAIN_TIMEOUT_S:-900}"
MAX_LEVEL="${MAX_LEVEL:-100000}"   # the search gives up above this
MAX_USERS="${MAX_USERS:-5000}"     # ramp stops here if no step has failed
# ramp: WARMUP_S seconds of unjudged warm-up before step 0 (users sign in gradually, then hold START); 0 = none.
WARMUP_S="${WARMUP_S:-0}"
[[ $WARMUP_S == 0 || ( $WARMUP_S =~ ^[1-9][0-9]*$ && $WARMUP_S -ge 30 ) ]] || { echo "LOADTEST: usage: WARMUP_S must be 0 (no warm-up) or an integer of at least 30 seconds (got '$WARMUP_S')" >&2; exit 2; }
RAMP_UP_S=10                       # ramp: each step ramps up over this long, then holds
RAMP_EVAL_DELAY_S=10               # ramp: a step is read this long after it ended (requests still in flight)

# The box's memory split (MiB total must be 4096): nginx, the web app and, with
# LT_DB=postgres, Postgres. Postgres' shared_buffers (25%) and effective_cache_size
# (75%) follow PG_MEM, so a split cannot leave them at a stale size.
BOX_MIB=4096
if [ "$LT_DB" = postgres ]; then DEFAULT_WEB_MEM=1024m; else DEFAULT_WEB_MEM=3840m; fi
export NGINX_MEM="${NGINX_MEM:-256m}" WEB_MEM="${WEB_MEM:-$DEFAULT_WEB_MEM}" PG_MEM="${PG_MEM:-2816m}"
# <integer>m or <integer>g (as in compose) to MiB, or an empty string.
to_mib() {
  [[ $1 =~ ^([1-9][0-9]*)([mg])$ ]] || return 0
  if [ "${BASH_REMATCH[2]}" = g ]; then echo "$((BASH_REMATCH[1] * 1024))"; else echo "${BASH_REMATCH[1]}"; fi
}
for _v in NGINX_MEM WEB_MEM PG_MEM; do
  [ -n "$(to_mib "${!_v}")" ] || { echo "LOADTEST: usage: $_v must be an integer with a unit m or g, e.g. 256m (got '${!_v}')" >&2; exit 2; }
done
if [ "$LT_DB" = postgres ]; then
  export PG_WORK_MEM
  # Every pool connection is a backend, and the runner's own psql calls need a few more.
  # 50 is enough for a pool up to 30; a bigger one raises the limit with it.
  _pool=${DB_POOL:-10}
  export PG_MAX_CONNECTIONS=$((_pool + 20 > 50 ? _pool + 20 : 50))
  export PG_SHARED_BUFFERS="$(awk -v m="$(to_mib "$PG_MEM")" 'BEGIN { printf "%d", m * 0.25 + 0.5 }')MB"
  export PG_EFFECTIVE_CACHE_SIZE="$(awk -v m="$(to_mib "$PG_MEM")" 'BEGIN { printf "%d", m * 0.75 + 0.5 }')MB"
fi

RESULTS_ROOT=loadtest/results
STATE_DIR=$RESULTS_ROOT/.state
[ "$PREFIX" = lt- ] || STATE_DIR=$RESULTS_ROOT/.state-$PREFIX
SAMPLER_PID=
EVENTS_PID=
SEED_PART=        # the snapshot being written, removed if the run ends before it is complete
WORK=$(mktemp -d)
RUN_DIR=          # results of the running command, for the summary of an aborted run
RUN_RESULT=null   # the search result so far, as JSON
RUN_CLOCK=null    # the box cores' clock policy at the start of the run (clock_state), as JSON
RUN_COOL=null     # what the cool-down gate did (cool_down), as JSON
SUMMARY_DONE=false
ABORT_REASON=
LEVEL_PASS=       # set by run_level: true or false
on_exit() {
  local rc=$?
  trap - EXIT
  [ -z "$SAMPLER_PID" ] || kill "$SAMPLER_PID" 2>/dev/null || true
  [ -z "$EVENTS_PID" ] || kill "$EVENTS_PID" 2>/dev/null || true
  docker rm -f "${PREFIX}k6-$$" >/dev/null 2>&1 || true
  [ -z "$SEED_PART" ] || rm -f "$SEED_PART"
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
# the node's Docker network, so no docker-proxy is on the path. K6_ARGS (an array)
# are extra arguments for k6 itself.
K6_ARGS=()
k6_run() {
  local out=$1 script=$2
  shift 2
  [[ $out == /* ]] || out=$PWD/$out
  docker run --rm --name "${PREFIX}k6-$$" --cpuset-cpus "$K6_CPUS" --network "$K6_NETWORK" --user "$(id -u):$(id -g)" \
    -v "$PWD/loadtest:/lt:ro" -v "$out:/out" -e BASE_URL="$BASE_URL" -e OUT=/out \
    -e LT_ADMIN_USER -e LT_ADMIN_PASSWORD "$@" "$K6_IMAGE" run --quiet "${K6_ARGS[@]}" "/lt/$script"
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
  echo "memory limits of the box add up to ${total} MiB (expected $BOX_MIB)"
  expected_cpus=$(expand_cpus "$NODE_CPUS" | wc -l)
  echo "nproc in the web container: $(docker exec "$WEB" nproc) (expected $expected_cpus)"
  for name in "${NODE_CONTAINERS[@]}"; do
    [ "$(docker inspect -f '{{.HostConfig.CpusetCpus}}' "$name")" = "$NODE_CPUS" ] || die "$name has the wrong cpuset"
    [ "$(docker inspect -f '{{.HostConfig.Memory}}' "$name")" = "$(docker inspect -f '{{.HostConfig.MemorySwap}}' "$name")" ] || die "$name may use swap"
  done
  [ "$total" = "$BOX_MIB" ] || die "the box's memory limits add up to ${total} MiB, not $BOX_MIB"
  [ "$(docker exec "$WEB" nproc)" = "$expected_cpus" ] || die "nproc in the web container is not $expected_cpus"
}

# ------------------------------------------------------------ seed snapshot

# Web mode keeps the seeded database in one named volume (Postgres' data, or the SQLite
# file in /data). A snapshot is a tar of that volume, taken with the box stopped so it is
# consistent, and put back before the database first starts. Node mode has its own
# Postgres, Garage and Litestream state and does not use it.
seed_cache_on() { [ "$SEED_CACHE" = 1 ] && [ "$LT_MODE" = web ]; }

# The service whose image runs the tar (it must be on the box already) and the volume it
# archives; GNU tar of the Postgres image needs --numeric-owner (uid 999), busybox does not.
seed_cache_service() { if [ "$LT_DB" = postgres ]; then echo postgres; else echo nginx; fi; }
seed_cache_volume() { if [ "$LT_DB" = postgres ]; then echo "${PREFIX}web_pgdata"; else echo "${PREFIX}web_data"; fi; }
seed_cache_image() { web_compose config --format json | jq -er ".services.$(seed_cache_service).image"; }
seed_cache_tar_opts() { if [ "$LT_DB" = postgres ]; then echo --numeric-owner; fi; }

# The snapshot file of the current settings. The name has the sizes; the hash covers all else
# that changes the data: the seed script and what it imports, the migrations of this
# backend, the image of the database (its ID, so a rebuilt tag is a different key) and the
# staff login (its hash is in the database).
seed_cache_file() {
  local img id migrations=migrations/sqlite hash
  img=$(seed_cache_image) || die "cannot read the compose file for the seed snapshot key"
  docker image inspect "$img" >/dev/null 2>&1 || docker pull "$img" >/dev/null || die "cannot pull $img"
  id=$(docker image inspect -f '{{.Id}}' "$img") || die "cannot inspect $img"
  [ "$LT_DB" = sqlite ] || migrations=migrations/web-postgres
  hash=$({
    printf 'db=%s books=%s members=%s loans=%s\nadmin=%s\npassword=%s\nimage=%s %s\n' "$LT_DB" "$BOOKS" "$MEMBERS" "$LOANS" "$LT_ADMIN_USER" "$LT_ADMIN_PASSWORD" "$img" "$id"
    cat loadtest/seed.js loadtest/common.js
    (cd "$migrations" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum)
  } | sha256sum | cut -c1-16)
  echo "$SEED_CACHE_DIR/seed-$LT_DB-b$BOOKS-m$MEMBERS-l$LOANS-$hash.tar"
}

# Put the snapshot into the (new) volume before the database starts: `create` makes the
# volume with compose's own labels and starts nothing.
seed_cache_restore() {
  local file=$1 started opts=()
  started=$(now)
  read -ra opts <<<"$(seed_cache_tar_opts)"
  web_compose create >/dev/null || die "cannot create the box's volumes"
  docker run --rm -i --user 0 -v "$(seed_cache_volume):/v" "$(seed_cache_image)" tar -xf - -C /v "${opts[@]}" <"$file" ||
    die "could not restore $file (scripts/loadtest.sh down; seed-cache clear to seed again)"
  echo "seed: restored snapshot $file ($(elapsed_since "$started") s)"
}

# Stop the whole box (a clean shutdown: Postgres checkpoints), archive the volume to a
# temp name, rename it, and start the box again. The tar goes through stdout so the file
# belongs to the runner's user.
seed_cache_save() {
  local file=$1 started opts=()
  started=$(now)
  read -ra opts <<<"$(seed_cache_tar_opts)"
  step "save the seeded database as a snapshot"
  mkdir -p "$SEED_CACHE_DIR"
  web_compose stop -t 120 >/dev/null || die "could not stop the box to save the snapshot"
  if [ "$LT_DB" = postgres ] && [ "$(docker inspect -f '{{.State.ExitCode}}' "${PREFIX}web-postgres-1")" != 0 ]; then
    die "Postgres did not shut down cleanly, no snapshot saved"
  fi
  SEED_PART="$file.part"
  docker run --rm --user 0 -v "$(seed_cache_volume):/v:ro" "$(seed_cache_image)" tar -cf - -C /v "${opts[@]}" . >"$SEED_PART" ||
    die "could not write the snapshot $file"
  mv "$SEED_PART" "$file"
  SEED_PART=
  web_compose up -d --wait --wait-timeout 180 || { web_compose logs --tail 20 web >&2 || true; die "the box did not become healthy after the snapshot (web logs above)"; }
  echo "seed: saved snapshot $file ($(du -h "$file" | cut -f1), $(elapsed_since "$started") s)"
}

cmd_seed_cache() {
  case "${1:-}" in
    list)
      [ -n "$(find "$SEED_CACHE_DIR" -maxdepth 1 -type f 2>/dev/null)" ] || { echo "no snapshots in $SEED_CACHE_DIR"; return 0; }
      (cd "$SEED_CACHE_DIR" && ls -lh --time-style=long-iso -- * | awk '{ print $5, $6, $7, $8 }')
      ;;
    clear)
      find "$SEED_CACHE_DIR" -maxdepth 1 -type f -print -delete 2>/dev/null || true
      echo "snapshots cleared ($SEED_CACHE_DIR)"
      ;;
    *) die "usage: scripts/loadtest.sh seed-cache list|clear" ;;
  esac
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
  if [ "$LT_MODE" = web ]; then
    local split_mib=$(($(to_mib "$NGINX_MEM") + $(to_mib "$WEB_MEM")))
    [ "$LT_DB" = sqlite ] || split_mib=$((split_mib + $(to_mib "$PG_MEM")))
    [ "$split_mib" = "$BOX_MIB" ] || die "the memory split adds up to $split_mib MiB, not $BOX_MIB (NGINX_MEM=$NGINX_MEM WEB_MEM=$WEB_MEM$([ "$LT_DB" = sqlite ] || echo " PG_MEM=$PG_MEM"))"
  fi

  # A snapshot of this seed, if there is one (web mode): restore it instead of seeding.
  local cache_file= restore=false
  if seed_cache_on; then
    cache_file=$(seed_cache_file)
    [ ! -f "$cache_file" ] || restore=true
  fi

  local create_admin="printf '%s\n' \"\$PW\" | library-web create-user $LT_ADMIN_USER --password-stdin"
  if [ "$LT_MODE" = web ]; then
    if [ "$restore" = true ]; then
      step "restore the seeded database from a snapshot"
      seed_cache_restore "$cache_file"
    fi
    step "start nginx and the web app ($LT_DB) in one box (cores $NODE_CPUS, 4 GB)"
    # A web that cannot start (for example an image without the postgres
    # feature) restarts forever, so bound the wait and show why.
    web_compose up -d --wait --wait-timeout 180 || { web_compose logs --tail 20 web >&2 || true; die "the box did not become healthy (web logs above)"; }
    # Local-only test credentials, like the e2e test. A snapshot already has the account.
    [ "$restore" = true ] || web_compose exec -T -e PW="$LT_ADMIN_PASSWORD" web sh -c "$create_admin" >/dev/null
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
  if [ "$restore" != true ]; then
    [ -z "$cache_file" ] || echo "seed: no snapshot, seeding (will save $cache_file)"
    for phase in books members loans; do
      case $phase in books) count=$BOOKS ;; members) count=$MEMBERS ;; loans) count=$LOANS ;; esac
      step "seed $count $phase through the API ($SEED_VUS VUs)"
      started=$(now)
      k6_run "$STATE_DIR" seed.js -e PHASE="$phase" -e BOOKS="$BOOKS" -e MEMBERS="$MEMBERS" -e LOANS="$LOANS" -e SEED_VUS="$SEED_VUS" \
        >"$STATE_DIR/seed-$phase.log" 2>&1 || { tail -20 "$STATE_DIR/seed-$phase.log" >&2; die "seeding $phase failed (log: $STATE_DIR/seed-$phase.log)"; }
      echo "$phase seeded in $(elapsed_since "$started") s"
    done
  fi
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
  # A restored box was just checked by the rows line above; a freshly seeded one is saved now.
  if [ -n "$cache_file" ] && [ "$restore" != true ]; then seed_cache_save "$cache_file"; fi

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


# ---------------------------------------------------- clock and temperature

# The package temperature in degrees C (one decimal): the x86_pkg_temp zone, or the
# hottest zone when there is none; empty when no zone can be read. World-readable.
pkg_temp_c() {
  local z t
  for z in /sys/class/thermal/thermal_zone*; do
    [ "$(cat "$z/type" 2>/dev/null || true)" = x86_pkg_temp ] || continue
    t=$(cat "$z/temp" 2>/dev/null || true)
    if [[ $t =~ ^[0-9]+$ ]]; then awk -v t="$t" 'BEGIN { printf "%.1f\n", t / 1000 }'; return 0; fi
  done
  { cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null || true; } |
    awk '$1 ~ /^[0-9]+$/ && $1 > m { m = $1; f = 1 } END { if (f) printf "%.1f\n", m / 1000 }'
}

# The box cores' throttle counters as JSON {core: [per core], package: n}, or null when
# one cannot be read. The package counter is per package: any box core reports it.
read_throttle() {
  local core first= c pkg cores=()
  for core in $(expand_cpus "$NODE_CPUS"); do
    [ -n "$first" ] || first=$core
    c=$(cat "/sys/devices/system/cpu/cpu$core/thermal_throttle/core_throttle_count" 2>/dev/null || true)
    [[ $c =~ ^[0-9]+$ ]] || { echo null; return 0; }
    cores+=("$c")
  done
  pkg=$(cat "/sys/devices/system/cpu/cpu$first/thermal_throttle/package_throttle_count" 2>/dev/null || true)
  [[ $pkg =~ ^[0-9]+$ ]] || { echo null; return 0; }
  jq -cn --argjson pkg "$pkg" '{core: $ARGS.positional, package: $pkg}' --jsonargs "${cores[@]}"
}

# Whether the box cores' clocks are pinned (scaling_min_freq == scaling_max_freq, the same
# on every core), as JSON {locked, khz, min_khz, max_khz, cores, text}. locked is null
# when cpufreq cannot be read. Without a lock the clock follows the laptop's power and
# thermal limits, so the results of two runs are not comparable.
clock_state() {
  local core mn mx rows=
  for core in $(expand_cpus "$NODE_CPUS"); do
    mn=$(cat "/sys/devices/system/cpu/cpu$core/cpufreq/scaling_min_freq" 2>/dev/null || true)
    mx=$(cat "/sys/devices/system/cpu/cpu$core/cpufreq/scaling_max_freq" 2>/dev/null || true)
    rows+="$core ${mn:-x} ${mx:-x}"$'\n'
  done
  printf '%s' "$rows" | jq -Rsc '
    [split("\n")[] | select(length > 0) | split(" ") | {core: (.[0] | tonumber), min: (try (.[1] | tonumber) catch null), max: (try (.[2] | tonumber) catch null)}] as $c
    | ($c | map(.core | tostring) | join(",")) as $cores
    | if any($c[]; .min == null or .max == null) then
        {locked: null, khz: null, min_khz: null, max_khz: null, cores: $cores, text: "unknown (cpufreq of cores \($cores) not readable)"}
      else
        ($c | map(.min) | min) as $lo | ($c | map(.max) | max) as $hi
        | if all($c[]; .min == .max) and $lo == $hi then
            {locked: true, khz: $lo, min_khz: $lo, max_khz: $hi, cores: $cores, text: "locked \($lo / 1000 | round) MHz on cores \($cores)"}
          else
            {locked: false, khz: null, min_khz: $lo, max_khz: $hi, cores: $cores, text: "unlocked (min–max \($lo / 1000 | round)–\($hi / 1000 | round) MHz on cores \($cores))"}
          end
      end'
}

# Wait until the package is below COOL_C, checking every 5 s and printing the temperature
# about every 30 s. After COOL_MAX_S it warns and goes on. Sets RUN_COOL.
cool_down() {
  local start=$SECONDS t t0 last=-30 timed_out=false
  if [ "$COOL_C" = 0 ]; then
    RUN_COOL=$(jq -cn '{enabled: false, text: "cool-down gate: off (COOL_C=0)"}')
    echo "cool-down gate: off (COOL_C=0)"
    return 0
  fi
  t=$(pkg_temp_c)
  if [ -z "$t" ]; then
    RUN_COOL=$(jq -cn --argjson c "$COOL_C" '{enabled: true, threshold_c: $c, start_c: null, end_c: null, waited_s: 0, timed_out: false, text: "cool-down gate: no temperature sensor, skipped"}')
    echo "warning: no CPU temperature sensor under /sys/class/thermal: cool-down gate skipped" >&2
    return 0
  fi
  t0=$t
  while awk -v t="$t" -v c="$COOL_C" 'BEGIN { exit !(t >= c) }'; do
    if ((SECONDS - start >= COOL_MAX_S)); then
      timed_out=true
      echo "warning: still $t C after ${COOL_MAX_S}s (limit $COOL_C C): starting anyway, results may be affected by heat" >&2
      break
    fi
    if ((SECONDS - last >= 30)); then
      echo "cooling: package $t C, waiting for below $COOL_C C ($((SECONDS - start))s of at most ${COOL_MAX_S}s)"
      last=$SECONDS
    fi
    sleep 5
    t=$(pkg_temp_c)
    [ -n "$t" ] || break
  done
  RUN_COOL=$(jq -cn --argjson c "$COOL_C" --argjson max_s "$COOL_MAX_S" --arg t0 "$t0" --arg t "${t:-$t0}" --argjson w "$((SECONDS - start))" --argjson timed_out "$timed_out" '
    {enabled: true, threshold_c: $c, max_s: $max_s, start_c: ($t0 | tonumber), end_c: ($t | tonumber), waited_s: $w, timed_out: $timed_out}
    | .text = (if .timed_out then "cool-down gate: TIMED OUT after \(.waited_s) s at \(.end_c) C (limit \(.threshold_c) C), the run started warm"
        else "cool-down gate: \(.start_c) C at the start, \(.end_c) C after \(.waited_s) s (limit \(.threshold_c) C)" end)')
  jq -r .text <<<"$RUN_COOL"
}

# Before the first load of a run: record the clock lock, then wait for the package to cool.
preflight() {
  RUN_CLOCK=$(clock_state)
  echo "cpu clock: $(jq -r .text <<<"$RUN_CLOCK")"
  [ "$(jq -r .locked <<<"$RUN_CLOCK")" = true ] || echo "warning: the box cores' clock is not locked, so a step can run at a different speed than the next (docs/LOADTEST.md, Reproducible runs)" >&2
  cool_down
}

# ------------------------------------------------------------------- levels

# Samples on a fixed SAMPLE_S tick, as JSON lines: container CPU and memory (the
# node's, and k6's while it runs), the sync worker's backlog, and Litestream's
# position and upload counter. The queries run at the same time, so a slow
# `docker stats` (about 2 s) does not stretch the tick.
sample_loop() {
  local file=$1 t next=$(now) names pids delay core thr temp freq_files=()
  # Each box core's current frequency (kHz): whether two boxes side by side slow
  # each other through the laptop's shared power budget.
  for core in $(expand_cpus "$NODE_CPUS"); do freq_files+=("/sys/devices/system/cpu/cpu$core/cpufreq/scaling_cur_freq"); done
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
    (restart_state >"$WORK/s.state" 2>/dev/null || echo null >"$WORK/s.state") &
    wait
    for core in "${freq_files[@]}"; do cat "$core" 2>/dev/null || echo null; done | jq -cs . >"$WORK/s.freq" || echo null >"$WORK/s.freq"
    # Throttle counters (cumulative) and the package temperature: ramp mode takes each step's delta.
    thr=$(read_throttle)
    temp=$(pkg_temp_c)
    jq -cn --arg t "$t" --argjson stats "$(cat "$WORK/s.stats")" \
      --argjson pending "$(cat "$WORK/s.pending")" --argjson lag "$(cat "$WORK/s.lag")" \
      --argjson puts "$(cat "$WORK/s.puts")" --argjson txid "$(cat "$WORK/s.txid")" \
      --argjson state "$(cat "$WORK/s.state")" --argjson freq "$(cat "$WORK/s.freq")" \
      --argjson throttle "$thr" --argjson temp "${temp:-null}" \
      '{t: ($t | tonumber), stats: $stats, pending: $pending, lag: $lag, puts: $puts, txid: $txid, state: $state, freq: $freq, throttle: $throttle, pkg_temp_c: $temp}' >>"$file" || true
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
  k6_run "$dir" user.js "${env_args[@]}" -e RAMP="$RAMP" -e HOLD="$HOLD" -e SEED_BOOKS="$SEED_BOOKS" -e PAGING="$PAGING" -e MIX="$MIX" >"$dir/k6.log" 2>&1 || rc=$?
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
    --argjson ramp "$RAMP" --argjson hold "$HOLD" --arg paging "$PAGING" --arg mix "$MIX" --arg db_pool "$DB_POOL" --arg pg_work_mem "$PG_WORK_MEM" --arg node_cpus "$NODE_CPUS" --arg mode "$LT_MODE" --arg db "$([ "$LT_MODE" = web ] && echo "$LT_DB" || true)" \
    --argjson clock "$RUN_CLOCK" --argjson cool "$RUN_COOL" \
    -f loadtest/summary.jq "$levels" >"$dir/summary.json" || return 1
  jq -r -L loadtest --arg prefix "$PREFIX" -f loadtest/summary-md.jq "$dir/summary.json" >"$dir/summary.md" || return 1
  SUMMARY_DONE=true
  echo
  echo "results: $dir (summary.md, summary.json, one directory per level)"
}

new_results_dir() {
  local dir="${RESULTS_DIR:-$RESULTS_ROOT/$(date -u +%Y%m%dT%H%M%SZ)$([ "$LT_MODE" = web ] && echo "-$LT_DB")${1:+-$1}$([ "$PREFIX" = lt- ] || echo "-${PREFIX%-}")}"
  mkdir -p "$dir" || die "cannot create the results directory $dir"
  echo "$dir"
}

cmd_level() {
  local kind=$1 value=$2 dir
  [[ $value =~ ^[1-9][0-9]*$ ]] || die "usage: loadtest.sh $kind <positive integer>"
  dir=$(new_results_dir)
  RUN_DIR=$dir
  : >"$dir/levels.jsonl"
  preflight
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
  preflight
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

# --------------------------------------------------------------------- ramp

# The k6 REST API's view of step $1 (a number, or warmup), as a document like k6's
# summary (.metrics), or fail. $2 is k6's address. The warm-up has no re-login metric
# but a login counter.
k6_live_doc() {
  local k=$1 addr=$2 name names=(http_req_duration http_req_failed http_reqs relogins)
  [ "$k" != warmup ] || names=(http_req_duration http_req_failed http_reqs)
  {
    for name in "${names[@]}"; do
      curl -fsS --max-time 5 "http://$addr:6565/v1/metrics/$name%7Bstep%3A$k%7D" || return 1
      echo
    done
    if [ "$k" = warmup ]; then
      curl -fsS --max-time 5 "http://$addr:6565/v1/metrics/warmup_logins" || return 1
      echo
    fi
  } | jq -s 'map(.data | {key: .id, value: {values: .attributes.sample, tainted: .attributes.tainted}}) | {metrics: from_entries}'
}

# ramp_steps <k6 doc file> <from> <to> <live|final>: the step objects, as a JSON array.
# The sample and event files are still being written, so a half-written last
# line is skipped.
ramp_steps() {
  local doc=$1 from=$2 to=$3 source=$4 dir=$RAMP_DIR
  jq -cR 'fromjson? | select(.)' "$dir/samples.jsonl" >"$WORK/r.samples"
  jq -cR 'fromjson? | select(.)' "$dir/events.jsonl" >"$WORK/r.events"
  jq -n -L loadtest --slurpfile k6 "$doc" --slurpfile samples "$WORK/r.samples" --slurpfile events "$WORK/r.events" \
    --arg prefix "$PREFIX" --argjson before "$RAMP_STATE_BEFORE" --argjson t0 "$RAMP_T0" --argjson step_s "$RAMP_STEP_S" \
    --argjson start "$RAMP_START" --argjson step_users "$RAMP_STEP" --argjson from "$from" --argjson to "$to" \
    --argjson ncont "${#NODE_CONTAINERS[@]}" --argjson p95_limit "$P95_MS" --argjson p99_limit "$P99_MS" --argjson err_limit "$ERR_PCT" \
    --argjson clock "$RUN_CLOCK" --arg source "$source" -f loadtest/ramp-steps.jq
}

# ramp_warmup <k6 doc file> <scenario start>: the warm-up object (loadtest/ramp-warmup.jq).
ramp_warmup() {
  local doc=$1 t0=$2 dir=$RAMP_DIR
  jq -cR 'fromjson? | select(.)' "$dir/samples.jsonl" >"$WORK/r.samples"
  jq -cn -L loadtest --slurpfile k6 "$doc" --slurpfile samples "$WORK/r.samples" --arg prefix "$PREFIX" \
    --argjson t0 "$t0" --argjson warmup_s "$WARMUP_S" --argjson start "$RAMP_START" --argjson ncont "${#NODE_CONTAINERS[@]}" \
    -f loadtest/ramp-warmup.jq
}

cmd_ramp() {
  local usage="usage: scripts/loadtest.sh ramp [START] [STEP] [STEP_S]  (positive integers, STEP_S at least 20; MAX_USERS, default 5000, ends the ramp)"
  [ "$LT_MODE" = web ] || die "ramp works with LT_MODE=web only"
  local start=${1:-100} step=${2:-100} step_s=${3:-60} v
  for v in "$start" "$step" "$step_s" "$MAX_USERS"; do [[ $v =~ ^[1-9][0-9]*$ ]] || die "$usage"; done
  ((step_s >= RAMP_UP_S * 2)) || die "$usage"
  ((MAX_USERS >= start)) || die "MAX_USERS ($MAX_USERS) is below START ($start)"
  local nsteps=$(((MAX_USERS - start) / step + 1))
  RAMP_START=$start RAMP_STEP=$step RAMP_STEP_S=$step_s
  # shellcheck disable=SC1091
  . "$STATE_DIR/seed.env" 2>/dev/null || die "no seeded cluster: scripts/loadtest.sh up first"
  local dir
  dir=$(new_results_dir ramp)
  RAMP_DIR=$dir
  healthy >/dev/null || die "web is not answering (is the box up?)"
  wait_until 180 "web healthy" healthy
  RAMP_STATE_BEFORE=$(restart_state) || die "cannot read the container state"
  local rows_before
  rows_before=$(db_rows) || die "cannot read the row counts (API)"
  echo "rows: $rows_before"
  preflight
  : >"$dir/samples.jsonl"
  sample_loop "$dir/samples.jsonl" &
  SAMPLER_PID=$!
  local filters=() name
  for name in "${NODE_CONTAINERS[@]}"; do filters+=(--filter "container=$name"); done
  docker events --filter type=container --filter event=die --filter event=oom "${filters[@]}" --format '{{json .}}' >"$dir/events.jsonl" 2>/dev/null &
  EVENTS_PID=$!

  local warm_args=() warm_note=
  if ((WARMUP_S > 0)); then
    warm_args=(-e WARMUP_S="$WARMUP_S")
    warm_note="warm-up $WARMUP_S s (unjudged), then "
  fi
  printf '\n==> ramp: %s%s users, +%s every %ss up to %s users (%s steps), until a step fails\n' "$warm_note" "$start" "$step" "$step_s" "$MAX_USERS" "$nsteps"
  K6_ARGS=(--address 0.0.0.0:6565)
  k6_run "$dir" user.js -e MODE=ramp -e START="$start" -e STEP="$step" -e STEP_S="$step_s" -e MAX_USERS="$MAX_USERS" "${warm_args[@]}" \
    -e P99_MS="$P99_MS" -e SEED_BOOKS="$SEED_BOOKS" -e PAGING="$PAGING" -e MIX="$MIX" >"$dir/k6.log" 2>&1 &
  local k6_pid=$!
  K6_ARGS=()
  # k6 first starts every VU it may need (MAX_USERS), then runs setup().
  local setup_ms deadline=$((SECONDS + 600))
  until setup_ms=$(sed -n 's/.*LT_SETUP_DONE \([0-9]*\) .*/\1/p' "$dir/k6.log" | head -1) && [ -n "$setup_ms" ]; do
    kill -0 "$k6_pid" 2>/dev/null || { tail -20 "$dir/k6.log" >&2; die "k6 ended before the ramp started (log: $dir/k6.log)"; }
    ((SECONDS < deadline)) || die "k6 did not start the ramp within 600 s (log: $dir/k6.log)"
    sleep 1
  done
  # The scenario starts with the warm-up; RAMP_T0 is where the stairs (step 0) start.
  local warm_t0
  warm_t0=$(awk -v ms="$setup_ms" 'BEGIN { printf "%.3f", ms / 1000 }')
  RAMP_T0=$(awk -v t="$warm_t0" -v w="$WARMUP_S" 'BEGIN { printf "%.3f", t + w }')
  local k6_ip
  k6_ip=$(docker inspect -f "{{(index .NetworkSettings.Networks \"$K6_NETWORK\").IPAddress}}" "${PREFIX}k6-$$") || die "cannot find k6's address"

  # Evaluate each step RAMP_EVAL_DELAY_S after it ended, from k6's REST API; stop
  # k6 at the first step that fails the pass rule. Requests still in flight at
  # that moment are missed, so the report is made from k6's end-of-test summary.
  # The warm-up is read once like a step, but never judged: it prints its own line before
  # the table and never stops the ramp.
  local warm_doc="$WORK/r.warm.json"
  if ((WARMUP_S > 0)); then
    while awk -v n="$(now)" -v d="$RAMP_T0" -v e="$RAMP_EVAL_DELAY_S" 'BEGIN { exit !(n < d + e) }'; do
      kill -0 "$k6_pid" 2>/dev/null || break
      sleep 1
    done
    if k6_live_doc warmup "$k6_ip" >"$warm_doc" 2>/dev/null; then
      echo
      jq -r -L loadtest 'include "ramp-lib"; warmup_line' <<<"$(ramp_warmup "$warm_doc" "$warm_t0")"
    fi
  fi
  echo
  jq -rn -L loadtest 'include "ramp-lib"; header'
  local k=0 live_fail="" stop=null live_doc="$WORK/r.live.json" live_steps step_end sigterm_at exit_at
  : >"$dir/live-steps.jsonl"
  while ((k < nsteps)); do
    step_end=$(awk -v t="$RAMP_T0" -v k="$k" -v s="$step_s" 'BEGIN { printf "%.3f", t + (k + 1) * s }')
    while awk -v n="$(now)" -v d="$step_end" -v e="$RAMP_EVAL_DELAY_S" 'BEGIN { exit !(n < d + e) }'; do
      kill -0 "$k6_pid" 2>/dev/null || break 2
      sleep 1
    done
    k6_live_doc "$k" "$k6_ip" >"$live_doc" || break # k6 is gone or going
    live_steps=$(ramp_steps "$live_doc" "$k" "$k" live) || die "could not compute step $k (loadtest/ramp-steps.jq)"
    jq -c '.[]' <<<"$live_steps" >>"$dir/live-steps.jsonl"
    jq -r -L loadtest -f loadtest/ramp-line.jq <<<"$(jq -c '.[0]' <<<"$live_steps")"
    if [ "$(jq -r '.[0].pass' <<<"$live_steps")" != true ]; then
      live_fail=$k
      sigterm_at=$(now)
      docker kill --signal TERM "${PREFIX}k6-$$" >/dev/null 2>&1 || true
      break
    fi
    k=$((k + 1))
  done
  # k6 either ran every step or was just told to stop; its summary is written
  # on the way out.
  deadline=$((SECONDS + 90))
  while kill -0 "$k6_pid" 2>/dev/null; do
    if ((SECONDS >= deadline)); then docker rm -f "${PREFIX}k6-$$" >/dev/null 2>&1 || true; die "k6 did not stop within 90 s"; fi
    sleep 0.5
  done
  exit_at=$(now)
  local rc=0
  wait "$k6_pid" || rc=$?
  kill "$SAMPLER_PID" "$EVENTS_PID" 2>/dev/null || true
  wait "$SAMPLER_PID" "$EVENTS_PID" 2>/dev/null || true
  SAMPLER_PID= EVENTS_PID=
  # k6 exits 99 when a threshold failed (the p99 one is meant to); only a missing summary is an error.
  [ -s "$dir/k6-summary.json" ] || { tail -20 "$dir/k6.log" >&2; die "k6 produced no summary (exit $rc, log: $dir/k6.log)"; }

  # The steps that are complete: all of them, or up to the step that stopped the run.
  local last=$((nsteps - 1))
  if [ -n "$live_fail" ]; then
    last=$live_fail
    stop=$(jq -cn --argjson step "$live_fail" --argjson a "$(awk -v s="$sigterm_at" -v e="$step_end" 'BEGIN { printf "%.1f", s - e }')" \
      --argjson b "$(awk -v s="$exit_at" -v e="$step_end" 'BEGIN { printf "%.1f", s - e }')" '{step: $step, to_sigterm_s: $a, to_exit_s: $b}')
  elif awk -v n="$exit_at" -v t="$RAMP_T0" -v k="$nsteps" -v s="$step_s" 'BEGIN { exit !(n < t + k * s) }'; then
    die "k6 stopped before the end of step $((nsteps - 1)) (exit $rc, log: $dir/k6.log)"
  fi
  ramp_steps "$dir/k6-summary.json" 0 "$last" final | jq -c '.[]' >"$dir/steps.jsonl" || die "could not compute the steps (loadtest/ramp-steps.jq; inputs in $dir)"
  # Steps that ended with k6 and were not read live (the last one) get their row now.
  local printed=$k
  [ -z "$live_fail" ] || printed=$((k + 1))
  sed -n "$((printed + 1)),\$p" "$dir/steps.jsonl" | while IFS= read -r line; do jq -r -L loadtest -f loadtest/ramp-line.jq <<<"$line"; done
  local warmup=null
  if ((WARMUP_S > 0)); then
    warmup=$(ramp_warmup "$dir/k6-summary.json" "$warm_t0") || die "could not compute the warm-up (loadtest/ramp-warmup.jq; inputs in $dir)"
    jq . <<<"$warmup" >"$dir/warmup.json"
  fi
  local out
  out=$(jq -s -L loadtest --arg db "$LT_DB" --arg paging "$PAGING" --arg mix "$MIX" --arg db_pool "$DB_POOL" --arg pg_work_mem "$PG_WORK_MEM" --arg node_cpus "$NODE_CPUS" --arg k6_cpus "$K6_CPUS" \
    --arg nginx_mem "$NGINX_MEM" --arg web_mem "$WEB_MEM" --arg pg_mem "$PG_MEM" \
    --arg pg_shared "${PG_SHARED_BUFFERS:-}" --arg pg_cache "${PG_EFFECTIVE_CACHE_SIZE:-}" \
    --argjson start "$start" --argjson step_users "$step" --argjson step_s "$step_s" --argjson max_users "$MAX_USERS" \
    --argjson p95 "$P95_MS" --argjson p99 "$P99_MS" --argjson err "$ERR_PCT" --argjson stop "$stop" --argjson live_fail "${live_fail:-null}" \
    --argjson clock "$RUN_CLOCK" --argjson cool "$RUN_COOL" --argjson warmup "$warmup" \
    -f loadtest/ramp-summary.jq "$dir/steps.jsonl") || die "could not write the summary (loadtest/ramp-summary.jq)"
  jq -r .md <<<"$out" >"$dir/summary.md"
  echo
  cat "$dir/summary.md"
  echo "results: $dir (summary.md, steps.jsonl, samples.jsonl, k6-summary.json)"
  [ "$(jq -r .ok <<<"$out")" = true ]
}

case "${1:-}" in
  up) cmd_up ;;
  users | rps) cmd_level "$1" "${2:?usage: loadtest.sh $1 N}" ;;
  search) shift; cmd_search "$@" ;;
  ramp) shift; cmd_ramp "$@" ;;
  seed-cache) shift; cmd_seed_cache "$@" ;;
  clockcheck) preflight; echo "now: package $(pkg_temp_c) C, throttle $(read_throttle)" ;;
  config) web_compose config ;;
  down) cmd_down ;;
  *)
    echo "usage: scripts/loadtest.sh up | users N | rps R | search users|rps [START] [FACTOR] | ramp [START] [STEP] [STEP_S] | clockcheck | seed-cache list|clear | config | down" >&2
    exit 2
    ;;
esac
