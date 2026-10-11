# 1. SQLite as each node's primary database

Status: accepted (2026-10-11)

## Context

We want a web app that is cheap to run, fast, and still safe with real data. The
requirements:

- Many VMs, each running the web app, a database and a sync worker. Each VM must
  work on its own.
- A user always lands on the same VM ("sticky" users).
- The app must keep working even if Postgres does not exist.
- Postgres is only a backup. Data flows one way, from the node to Postgres.

## Decision

Each node uses one SQLite file as its production database. The app reads and writes
only that file, so a request never touches the network. Two backups protect it:

- Litestream copies the file to S3 (a physical copy, used to restore the node).
- A sync worker copies rows to a central Postgres (a logical copy, readable with SQL).

Each node is one library branch with its own books, members, loans and staff
accounts. Users stay on their node, so nothing is shared between nodes and nothing
needs two-way sync.

Alternatives we looked at:

| Option | Why not |
|---|---|
| One Postgres with read replicas | Every request needs the network and one central server. It does not match "each VM is self-sufficient" or "works without Postgres". |
| Litestream alone | It is a good physical backup, but nobody can query the data across branches, and there is no second, independent copy. |
| PowerSync or ElectricSQL | They are built for two-way sync between many clients and a server. We only need one direction, and each node owns its data, so they add more than the job needs. |

## Consequences

- Reads and writes are fast, and the app survives Postgres or S3 being down.
- A lost disk can lose about one second of writes (Litestream's interval).
- Only one host may use a database file, and it must be on local disk.
- Users cannot move between nodes. Production needs a fixed rule that maps each
  branch to its node.
- User accounts and sessions stay on the node. Password hashes are not copied to a
  central database.
- Postgres holds the current state of each row, not a history. A history table can
  be added later (see [ARCHITECTURE](../ARCHITECTURE.md#future-work)).
