# 2. One container per job

Status: accepted (2026-10-11)

## Context

A node needs four jobs done: restore the database if the disk is empty, stream it
to S3, serve the web app, and ship changes to Postgres. We could run them all in
one container (Litestream's `-exec` mode can launch the app) or in four.

## Decision

Each node runs four containers from [node.yml](../../deploy/compose/node.yml):

| Container | Job |
|---|---|
| `restore` | Runs once at start. Restores the database from S3 if the disk has none. |
| `litestream` | Streams every change to S3. |
| `web` | Serves the app. Runs the SQLite migrations. |
| `sync-worker` | Ships changes to Postgres. The only container with Postgres credentials. |

They share one volume and all run as uid 10001. `web` starts after `litestream`,
so on shutdown `web` stops first and Litestream uploads the last writes before it
stops.

## Consequences

- Each job has its own logs, health check and restart. If Postgres is down, only
  `sync-worker` is unhappy; the app keeps serving.
- The web binary has no Postgres code at all. A CI check fails if it gains a
  Postgres driver.
- It costs a little more compose configuration than one container would.
- Running everything under Litestream's `-exec` mode was considered and rejected:
  one container would give up the separate logs, health checks and restarts above.
