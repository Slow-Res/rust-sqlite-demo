# syntax=docker/dockerfile:1
#
# Images for the two Rust programs (spec §3). Pick one with --target:
#   docker build -f deploy/docker/Dockerfile --target web -t sqlite-production/web:dev .
#   docker build -f deploy/docker/Dockerfile --target sync-worker -t sqlite-production/sync-worker:dev .
# Both targets share the build stages, so the second build is quick.

# ---- React build -> /app/apps/frontend/dist --------------------------------------
FROM node:22-bookworm-slim AS frontend
WORKDIR /app/apps/frontend
# Dependencies first, so editing source does not reinstall them.
COPY apps/frontend/package.json apps/frontend/package-lock.json ./
RUN npm ci
# src/generated/ (TypeScript types from the shared crate) is committed: no Rust needed here.
COPY apps/frontend/ ./
RUN npm run build

# ---- Rust build -> /out/library-web and /out/sync-worker -------------------------
FROM rust:1.96-bookworm AS rust
WORKDIR /app
# apps/web's compile-time checked SQL reads the committed .sqlx/ data, not a database.
ENV SQLX_OFFLINE=true
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY apps/web/ apps/web/
COPY apps/sync-worker/ apps/sync-worker/
COPY migrations/ migrations/
# Empty by default. The load-test image passes `postgres` (make loadtest-image).
ARG WEB_FEATURES=""
# Two separate builds on purpose: built alone, library-web gets only its own
# SQLx features, so the web binary contains no Postgres driver (spec §4),
# unless WEB_FEATURES asks for the load-test backend.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked -p library-web ${WEB_FEATURES:+--features "$WEB_FEATURES"} \
    && cargo build --release --locked -p sync-worker \
    && mkdir /out \
    && cp target/release/library-web target/release/sync-worker /out/

# ---- Runtime base shared by both programs ----------------------------------------
FROM debian:bookworm-slim AS runtime
# curl is only for the health checks. uid 10001 is shared by every container
# that opens the node's database (spec §3).
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --user-group --home-dir /nonexistent --shell /usr/sbin/nologin app \
    && mkdir /data \
    && chown app:app /data

# ---- web ---------------------------------------------------------------------------
FROM runtime AS web
COPY --from=rust /out/library-web /usr/local/bin/library-web
COPY --from=frontend /app/apps/frontend/dist /srv/frontend
# Set COOKIE_SECURE=true when the app is served over HTTPS (node.yml defaults to true).
ENV DATABASE_URL=sqlite:///data/library.db \
    BIND_ADDR=0.0.0.0:4000 \
    FRONTEND_DIR=/srv/frontend \
    COOKIE_SECURE=false \
    LOG_FORMAT=json
USER app
EXPOSE 4000
HEALTHCHECK --interval=10s --timeout=3s --start-period=30s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:4000/api/health || exit 1
# No arguments means `serve`; other subcommands pass through (seed, create-user, check-db).
ENTRYPOINT ["library-web"]

# ---- sync-worker -------------------------------------------------------------------
FROM runtime AS sync-worker
COPY --from=rust /out/sync-worker /usr/local/bin/sync-worker
ENV DATABASE_URL=sqlite:///data/library.db \
    METRICS_ADDR=0.0.0.0:9100 \
    LOG_FORMAT=json
USER app
HEALTHCHECK --interval=10s --timeout=3s --start-period=60s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:9100/health || exit 1
ENTRYPOINT ["sync-worker"]
