#!/usr/bin/env bash
# Start (or reuse) a throwaway Mailpit (an SMTP sink with an HTTP API) for the
# email job tests and print the variables they read:
#   eval "$(./scripts/test-mailpit.sh)"
# Chaos is enabled so a test can make it answer 4xx or 5xx (tests/jobs_email_chaos.rs);
# this script switches every trigger off again. The email tests share this one
# Mailpit and its global chaos state, so they assume `cargo test` (test binaries run
# one after another), not nextest.
set -euo pipefail
NAME=sqlite-production-test-mailpit
SMTP_PORT=55025
HTTP_PORT=58025
IMAGE=axllent/mailpit:v1.31.2

if ! docker ps --format '{{.Names}}' | grep -qx "$NAME"; then
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker run -d --name "$NAME" -e MP_ENABLE_CHAOS=true -e MP_MAX_MESSAGES=5000 \
    -p "127.0.0.1:$SMTP_PORT:1025" -p "127.0.0.1:$HTTP_PORT:8025" "$IMAGE" >/dev/null
fi

for _ in $(seq 1 60); do
  if curl -fsS "http://127.0.0.1:$HTTP_PORT/api/v1/info" >/dev/null 2>&1; then
    # A killed chaos test may have left a trigger on, failing every later send.
    curl -fsS -X PUT -H 'Content-Type: application/json' \
      -d '{"Sender":{"ErrorCode":451,"Probability":0},"Recipient":{"ErrorCode":451,"Probability":0},"Authentication":{"ErrorCode":535,"Probability":0}}' \
      "http://127.0.0.1:$HTTP_PORT/api/v1/chaos" >/dev/null
    echo "export TEST_SMTP_URL=smtp://127.0.0.1:$SMTP_PORT"
    echo "export TEST_MAILPIT_URL=http://127.0.0.1:$HTTP_PORT"
    exit 0
  fi
  sleep 0.5
done
echo "test Mailpit did not become ready" >&2
exit 1
