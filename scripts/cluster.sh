#!/usr/bin/env bash
# The local cluster (spec §10): central services plus three nodes. Each node
# is the same deploy/compose/node.yml a real VM runs.
#   scripts/cluster.sh up | down | reset [--yes] | ps | logs <node> | exec <node> <service> <cmd...>
# PREFIX (default empty) names the compose projects, so scripts/e2e.sh can run
# a second, separate cluster. HTTP_PORT_BASE (default 4000) gives node N the
# port BASE+N.
set -euo pipefail
cd "$(dirname "$0")/.."
PREFIX="${PREFIX:-}"
export SHARED_NETWORK="${SHARED_NETWORK:-${PREFIX}sqlite-prod}"
HTTP_PORT_BASE="${HTTP_PORT_BASE:-4000}"
NODES=(node1 node2 node3)

central() { docker compose -p "${PREFIX}central" -f deploy/compose/central.yml "$@"; }
node() {
  local n=$1
  shift
  HTTP_PORT=$((HTTP_PORT_BASE + ${n#node})) docker compose -p "${PREFIX}${n}" \
    -f deploy/compose/node.yml -f deploy/compose/local/node.override.yml \
    --env-file "deploy/compose/local/${n}.env" "$@"
}

case "${1:-}" in
  up)
    central up -d --wait postgres
    central up -d
    PREFIX="$PREFIX" deploy/garage/init.sh
    for n in "${NODES[@]}"; do
      # A brand-new node needs ALLOW_FRESH_DB=true once (spec §7.2). A node
      # whose volume exists keeps its data, or restores it from S3.
      if docker volume inspect "${PREFIX}${n}_data" >/dev/null 2>&1; then fresh=false; else fresh=true; fi
      ALLOW_FRESH_DB=$fresh node "$n" up -d
    done
    echo "cluster up: nodes on ports $((HTTP_PORT_BASE + 1))-$((HTTP_PORT_BASE + 3)), Prometheus on ${PROMETHEUS_HOST_PORT:-9090}"
    ;;
  down)
    for n in "${NODES[@]}"; do node "$n" down; done
    central down
    ;;
  reset)
    if [[ "${2:-}" != "--yes" ]]; then
      read -r -p "Delete ALL data of the '${PREFIX}' cluster (databases, backups, Postgres)? [y/N] " answer
      [[ "$answer" == y ]] || exit 1
    fi
    for n in "${NODES[@]}"; do node "$n" down -v; done
    central down -v
    ;;
  ps)
    central ps
    for n in "${NODES[@]}"; do node "$n" ps; done
    ;;
  logs)
    node "${2:?usage: cluster.sh logs <node>}" logs -f
    ;;
  exec)
    n=${2:?usage: cluster.sh exec <node> <service> <cmd...>}
    shift 2
    node "$n" exec "$@"
    ;;
  *)
    echo "usage: scripts/cluster.sh up | down | reset [--yes] | ps | logs <node> | exec <node> <service> <cmd...>" >&2
    exit 2
    ;;
esac
