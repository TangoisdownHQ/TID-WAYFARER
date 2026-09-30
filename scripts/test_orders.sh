#!/usr/bin/env bash
# Full SupplyLink marketplace lifecycle:
#   1. Requester posts an order
#   2. Bidder submits a bid
#   3. Requester accepts → fulfillment created
#   4. Shipper marks shipped
#   5. Requester marks delivered
#   6. Either party records settlement tx
#
# Uses real JWTs minted via the gen_jwt utility — verifies the entire
# auth-protected path end-to-end.

set -euo pipefail

API="${API_BASE:-http://127.0.0.1:4000}"
HERE="$(dirname "$0")"

pass() { echo "✅ $1"; }
fail() { echo "❌ $1"; exit 1; }

PSQL="sudo docker exec -i tidasone-db-v2 psql -U postgres -d tidasone -v ON_ERROR_STOP=1 -tA"

REQUESTER_ID="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
SHIPPER_ID="11111111-2222-3333-4444-555555555555"

echo "── phase 0: seed test users ──"
$PSQL <<SQL > /dev/null
INSERT INTO users (id, username, email) VALUES
  ('$REQUESTER_ID', 'test-requester', 'requester-orders@tidhq.net'),
  ('$SHIPPER_ID',   'test-shipper',   'shipper-orders@tidhq.net')
ON CONFLICT (id) DO NOTHING;
SQL
pass "users seeded"

echo
echo "── phase 1: mint JWTs ──"
echo "    building gen_jwt (cached after first run)..."
REQUESTER_JWT=$("$HERE/get_test_jwt.sh" "$REQUESTER_ID" user)
SHIPPER_JWT=$("$HERE/get_test_jwt.sh" "$SHIPPER_ID" user)
[ -n "$REQUESTER_JWT" ] || fail "failed to mint requester JWT"
[ -n "$SHIPPER_JWT"   ] || fail "failed to mint shipper JWT"
pass "minted requester + shipper JWTs"


echo
echo "── phase 2: requester posts an order ──"
ORDER_BODY=$(cat <<EOF
{
  "description": "100kg organic seed for Mars greenhouse",
  "item_kind": "inventory",
  "target_part_number": "SEED-WHEAT-ORG",
  "target_meta": {"cultivar": "Hard Red Winter", "organic": true},
  "quantity": 100,
  "unit": "kg",
  "delivery_body_id": 499,
  "delivery_lat": 18.4447,
  "delivery_lon": 77.4508,
  "delivery_address": "Jezero Greenhouse",
  "max_price": 5000.0
}
EOF
)
ORDER_RES=$(curl -sf -X POST "$API/api/orders" \
  -H "Authorization: Bearer $REQUESTER_JWT" \
  -H "Content-Type: application/json" \
  -d "$ORDER_BODY")
ORDER_ID=$(echo "$ORDER_RES" | jq -r .id)
[ "$ORDER_ID" != "null" ] && [ -n "$ORDER_ID" ] || fail "order create returned no id: $ORDER_RES"
pass "order created (id=$ORDER_ID, status=posted)"


echo
echo "── phase 3: shipper submits a bid ──"
BID_BODY='{"price": 4200.50, "transit_days": 210, "notes": "Hohmann window window"}'
BID_RES=$(curl -sf -X POST "$API/api/orders/$ORDER_ID/bids" \
  -H "Authorization: Bearer $SHIPPER_JWT" \
  -H "Content-Type: application/json" \
  -d "$BID_BODY")
BID_ID=$(echo "$BID_RES" | jq -r .id)
[ "$BID_ID" != "null" ] && [ -n "$BID_ID" ] || fail "bid create failed: $BID_RES"
pass "bid submitted (id=$BID_ID, price=4200.50)"

# Order status should have flipped posted → bid
STATUS=$(curl -sf -H "Authorization: Bearer $REQUESTER_JWT" "$API/api/orders/$ORDER_ID" | jq -r .order.status)
[ "$STATUS" = "bid" ] || fail "expected order status=bid after first bid, got $STATUS"
pass "order status auto-advanced to 'bid'"


echo
echo "── phase 4: shipper cannot bid on their own order (negative test) ──"
# A bidder bidding on someone else's order is allowed (verified above).
# Test: requester trying to bid on their own order — should 403.
SELF_BID_CODE=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$API/api/orders/$ORDER_ID/bids" \
  -H "Authorization: Bearer $REQUESTER_JWT" \
  -H "Content-Type: application/json" \
  -d '{"price": 1.0}')
[ "$SELF_BID_CODE" = "403" ] || fail "expected 403 for self-bid, got $SELF_BID_CODE"
pass "requester cannot bid on own order (403)"


echo
echo "── phase 5: requester accepts the bid → fulfillment created ──"
FULFILL=$(curl -sf -X POST "$API/api/orders/$ORDER_ID/bids/$BID_ID/accept" \
  -H "Authorization: Bearer $REQUESTER_JWT")
FULFILL_ID=$(echo "$FULFILL" | jq -r .id)
[ "$FULFILL_ID" != "null" ] && [ -n "$FULFILL_ID" ] || fail "accept failed: $FULFILL"
pass "bid accepted, fulfillment created (id=$FULFILL_ID)"

ORDER_STATE=$(curl -sf -H "Authorization: Bearer $REQUESTER_JWT" "$API/api/orders/$ORDER_ID" | jq -r .order.status)
[ "$ORDER_STATE" = "accepted" ] || fail "expected order=accepted, got $ORDER_STATE"
pass "order status → accepted"


echo
echo "── phase 6: shipper marks shipped ──"
SHIP=$(curl -sf -X POST "$API/api/fulfillments/$FULFILL_ID/ship" \
  -H "Authorization: Bearer $SHIPPER_JWT")
SHIP_STATUS=$(echo "$SHIP" | jq -r .status)
[ "$SHIP_STATUS" = "in_transit" ] || fail "expected status=in_transit, got $SHIP_STATUS"
pass "fulfillment marked shipped (status=in_transit, shipped_at set)"


echo
echo "── phase 7: requester marks delivered ──"
DELIV=$(curl -sf -X POST "$API/api/fulfillments/$FULFILL_ID/deliver" \
  -H "Authorization: Bearer $REQUESTER_JWT")
DELIV_STATUS=$(echo "$DELIV" | jq -r .status)
[ "$DELIV_STATUS" = "delivered" ] || fail "expected status=delivered, got $DELIV_STATUS"
pass "fulfillment marked delivered"


echo
echo "── phase 8: settlement recorded ──"
SETTLE=$(curl -sf -X POST "$API/api/fulfillments/$FULFILL_ID/settle" \
  -H "Authorization: Bearer $REQUESTER_JWT" \
  -H "Content-Type: application/json" \
  -d '{"settlement_tx": "0xdeadbeefcafe1234567890"}')
SETTLE_STATUS=$(echo "$SETTLE" | jq -r .status)
[ "$SETTLE_STATUS" = "settled" ] || fail "expected status=settled, got $SETTLE_STATUS"
pass "settlement recorded (tx=0xdeadbeefcafe...)"

FINAL=$(curl -sf -H "Authorization: Bearer $REQUESTER_JWT" "$API/api/orders/$ORDER_ID" | jq -r .order.status)
[ "$FINAL" = "settled" ] || fail "expected order=settled, got $FINAL"
pass "order lifecycle reached settled"


echo
echo "── phase 9: cleanup ──"
$PSQL <<SQL > /dev/null
DELETE FROM fulfillments WHERE order_id = '$ORDER_ID';
DELETE FROM bids         WHERE order_id = '$ORDER_ID';
DELETE FROM orders       WHERE id       = '$ORDER_ID';
DELETE FROM users        WHERE id IN ('$REQUESTER_ID', '$SHIPPER_ID');
SQL
pass "test fixtures cleaned up"

echo
echo "🎉 SupplyLink lifecycle validated end-to-end."
