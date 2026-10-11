# syntax=docker/dockerfile:1
#
# Litestream plus our config and the restore gate (spec §7). Each node uses it
# for two containers: `restore` (runs restore.sh once) and `litestream`.
#   docker build -f deploy/docker/litestream.Dockerfile -t sqlite-production/litestream:dev .
FROM litestream/litestream:0.5.17
# sqlite3 lets the restore gate create a brand-new database in WAL mode.
# uid 10001 is shared by every container that opens the database (spec §3).
RUN apt-get update \
    && apt-get install -y --no-install-recommends sqlite3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --user-group --home-dir /nonexistent --shell /usr/sbin/nologin app \
    && mkdir /data \
    && chown app:app /data
COPY deploy/docker/litestream/litestream.yml /etc/litestream.yml
COPY --chmod=0755 deploy/docker/litestream/restore.sh /usr/local/bin/restore.sh
USER app
# The image's entrypoint is `litestream`; `replicate` is the long-running mode.
CMD ["replicate", "-config", "/etc/litestream.yml"]
