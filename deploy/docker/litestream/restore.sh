#!/usr/bin/env bash
# The restore gate (spec §7.2). It runs once before anything else on a node.
# Exit 0 lets the node start. Any other exit keeps the app down, which is the
# safe outcome whenever we cannot be sure whether a backup exists.
set -euo pipefail

DB=/data/library.db
CONFIG=/etc/litestream.yml
node="${NODE_ID:-}"

# 1. The node id is the S3 folder: refuse anything odd. Bash's =~ matches the
#    whole string, so a value with a newline cannot sneak past.
if [[ ! "$node" =~ ^[a-z0-9][a-z0-9-]{0,39}$ ]]; then
  echo "restore: NODE_ID '$node' must be 1-40 characters of a-z, 0-9 and '-', starting with a letter or digit" >&2
  exit 1
fi

# 2. The normal redeploy: the database is already on this disk.
if [[ -f "$DB" ]]; then
  echo "restore: database present at $DB; nothing to do"
  exit 0
fi

# 3. Someone deleted only part of the database. SQLite would replay an old WAL
#    into a new file (spec S12), so a person must look first.
for leftover in "$DB-wal" "$DB-shm" "/data/.library.db-litestream"; do
  if [[ -e "$leftover" ]]; then
    echo "restore: $DB is missing but $leftover exists: part of the database was deleted." \
      "Refusing to start; see RUNBOOK 'Database file missing but its WAL is still there'." >&2
    exit 1
  fi
done

# 4. Empty disk: restore the newest backup. A failure (S3 unreachable, bad
#    keys) must never fall through to an empty start.
echo "restore: no database on this disk; restoring the newest backup of node '$node'"
if ! litestream restore -config "$CONFIG" -if-replica-exists -integrity-check quick "$DB"; then
  echo "restore: litestream restore failed (is S3 reachable?). Not starting with an empty database." >&2
  exit 1
fi

# 5. Restored.
if [[ -f "$DB" ]]; then
  echo "restore: restored $DB from the backup"
  exit 0
fi

# 6. No backup exists. Only a brand-new node may start empty, and only when told to.
if [[ "${ALLOW_FRESH_DB:-false}" == "true" ]]; then
  # Create the file in WAL mode now, so Litestream finds a database to watch
  # even before library-web has started.
  sqlite3 "$DB" 'PRAGMA journal_mode = WAL;' >/dev/null
  echo "restore: no backup exists; starting with a new empty database (ALLOW_FRESH_DB=true)"
  exit 0
fi
echo "restore: No database and no backup for node '$node' at ${LITESTREAM_BUCKET:-?}/$node." \
  "If this is a brand-new node, set ALLOW_FRESH_DB=true for its first start." >&2
exit 1
