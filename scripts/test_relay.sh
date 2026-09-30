#!/usr/bin/env bash
# Smoke test for the Go mesh-relay sidecar.
# Three phases:
#   (1) health + metrics endpoints
#   (2) forwarder picks up a dtn_outbox row and attempts delivery to a
#       dead endpoint → attempts++  (no real peer needed)
#   (3) /inbox accepts a bundle and persists it to dtn_inbox

set -euo pipefail

RELAY="${RELAY_BASE:-http://127.0.0.1:3100}"

pass() { echo "✅ $1"; }
fail() { echo "❌ $1"; exit 1; }

PSQL="sudo docker exec -i tidasone-db-v2 psql -U postgres -d tidasone -v ON_ERROR_STOP=1 -tA"


echo "── phase 1: health & metrics ──"

code=$(curl -s -o /dev/null -w "%{http_code}" "$RELAY/health")
[ "$code" = "200" ] || fail "health endpoint returned $code"
pass "health endpoint (GET $RELAY/health → 200)"

code=$(curl -s -o /dev/null -w "%{http_code}" "$RELAY/metrics")
[ "$code" = "200" ] || fail "metrics endpoint returned $code"
pass "metrics endpoint (GET $RELAY/metrics → 200)"


echo
echo "── phase 2: forwarder picks up dtn_outbox ──"

# Use a tag we can grep for so concurrent tests don't collide.
TAG="forwarder-test-$$-$(date +%s)"

# Endpoint points at a deliberately unreachable port so the POST fails fast
# and the forwarder records an attempt + pushes next_try_at out.
$PSQL -c "
  INSERT INTO dtn_outbox (dest_node_id, endpoint, payload, next_try_at)
  VALUES (
    gen_random_uuid(),
    'http://127.0.0.1:1/dead',
    jsonb_build_object('test', '$TAG'),
    NOW()
  )
" > /dev/null
pass "inserted test bundle into dtn_outbox (tag=$TAG)"

echo "    waiting up to 30s for forwarder to attempt delivery..."
ATTEMPTS=0
for i in $(seq 1 15); do
  ATTEMPTS=$($PSQL -c "SELECT COALESCE(MAX(attempts),0) FROM dtn_outbox WHERE payload->>'test' = '$TAG'")
  if [ "$ATTEMPTS" -ge "1" ]; then
    break
  fi
  sleep 2
done

[ "$ATTEMPTS" -ge "1" ] || fail "forwarder didn't attempt delivery within 30s (attempts=$ATTEMPTS)"
pass "forwarder attempted delivery (attempts=$ATTEMPTS)"

$PSQL -c "DELETE FROM dtn_outbox WHERE payload->>'test' = '$TAG'" > /dev/null
pass "outbox cleaned up"


echo
echo "── phase 3: /inbox accepts and persists ──"

INBOX_TAG="inbox-test-$$-$(date +%s)"
BODY=$(cat <<EOF
{"src_node_id": null, "payload": {"test": "$INBOX_TAG", "from": "test_relay.sh"}}
EOF
)

code=$(curl -s -o /tmp/relay-inbox.out -w "%{http_code}" -X POST "$RELAY/inbox" \
  -H "Content-Type: application/json" \
  -d "$BODY")

[ "$code" = "202" ] || fail "/inbox returned $code (body: $(cat /tmp/relay-inbox.out 2>/dev/null))"
pass "/inbox accepted bundle (POST → 202)"

ROWS=$($PSQL -c "SELECT count(*) FROM dtn_inbox WHERE payload->>'test' = '$INBOX_TAG'")
[ "$ROWS" = "1" ] || fail "expected 1 dtn_inbox row, got $ROWS"
pass "bundle persisted in dtn_inbox"

$PSQL -c "DELETE FROM dtn_inbox WHERE payload->>'test' = '$INBOX_TAG'" > /dev/null
pass "inbox cleaned up"


echo
echo "🎉 mesh-relay sidecar validated end-to-end."
