#!/usr/bin/env bash
# Prepare the local Garage: a one-node layout, the backup bucket and the fixed
# dev key the nodes use. Safe to run again. LOCAL ONLY.
set -euo pipefail
cd "$(dirname "$0")/../.."
PREFIX="${PREFIX:-}"
BUCKET=litestream
KEY_ID=GK1a0c2d4e6f8091a2b3c4d5e6
KEY_SECRET=6c6f63616c2d6465762d6f6e6c792d6e6f742d612d7265616c2d736563726574

garage() { docker compose -p "${PREFIX}central" -f deploy/compose/central.yml exec -T garage /garage "$@"; }

for _ in $(seq 1 60); do
  garage status >/dev/null 2>&1 && break
  sleep 1
done
garage status >/dev/null

# A fresh Garage has no layout (version 0): give its one node 1 GB.
if garage layout show | grep -qiE 'current cluster layout version: 0'; then
  node=$(garage node id -q | cut -d@ -f1)
  garage layout assign -z local -c 1G "$node"
  garage layout apply --version 1
fi
garage bucket info "$BUCKET" >/dev/null 2>&1 || garage bucket create "$BUCKET"
garage key info "$KEY_ID" >/dev/null 2>&1 || garage key import --yes -n litestream-dev "$KEY_ID" "$KEY_SECRET"
garage bucket allow --read --write --owner "$BUCKET" --key "$KEY_ID"
echo "garage: bucket '$BUCKET' ready for key $KEY_ID"
