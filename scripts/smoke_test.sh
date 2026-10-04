#!/usr/bin/env bash
# End-to-end check against a running outpost. Exits non-zero on the first real
# failure.
#
# Different from the Rust test suites, deliberately. Those run against the
# router in-process; this runs against the shipped container over HTTP, so it
# catches the things only assembly can break: a route not mounted, a page not
# served, a migration not applied, a daemon that did not start, a static asset
# reaching for a CDN the CSP will refuse.
#
# Expects `scripts/seed_demo.sh` to have run — it asserts against that scenario,
# because an assertion with no data behind it passes for the wrong reason.
#
# Usage:
#   scripts/seed_demo.sh && scripts/smoke_test.sh

set -uo pipefail

API="${API:-http://localhost:4000/api}"
UI="${UI:-http://localhost:4000/ui}"
PASSWORD="${PASSWORD:-demo-outpost-password}"

PASS=0
FAIL=0

ok()   { echo "  ✓ $*"; PASS=$((PASS+1)); }
bad()  { echo "  ✗ $*"; FAIL=$((FAIL+1)); }
head_() { echo; echo "▸ $*"; }

# Assert a value, showing what was actually found — a bare "failed" sends
# somebody back to the script to work out what it even checked.
is() {
  local what="$1" want="$2" got="$3"
  if [[ "$got" == "$want" ]]; then ok "$what"; else bad "$what (wanted '$want', got '$got')"; fi
}
at_least() {
  local what="$1" want="$2" got="$3"
  if [[ "${got:-0}" =~ ^-?[0-9]+$ ]] && (( got >= want )); then ok "$what ($got)"
  else bad "$what (wanted >= $want, got '${got:-}')"; fi
}

code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }

head_ "Service is up"
is "GET /api/health" 200 "$(code "$API/health")"
is "GET /metrics"    200 "$(code "${API%/api}/metrics")"

head_ "Sign-in works and the throttle is not in the way"
TOKEN="$(curl -s -X POST "$API/local-auth/login" -H 'Content-Type: application/json' \
  -d "{\"email\":\"depot@demo.invalid\",\"password\":\"$PASSWORD\"}" | jq -r '.token // empty')"
if [[ -n "$TOKEN" ]]; then ok "signed in as depot@demo.invalid"
else bad "could not sign in — has scripts/seed_demo.sh been run?"; echo; echo "$FAIL failure(s)"; exit 1; fi
AUTH=(-H "Authorization: Bearer $TOKEN")

OTHER="$(curl -s -X POST "$API/local-auth/login" -H 'Content-Type: application/json' \
  -d "{\"email\":\"orbital@demo.invalid\",\"password\":\"$PASSWORD\"}" | jq -r '.token // empty')"
[[ -n "$OTHER" ]] && ok "signed in as the other organisation" || bad "could not sign in as orbital@demo.invalid"

head_ "A wrong password is refused, and recorded"
is "wrong password → 401" 401 "$(code -X POST "$API/local-auth/login" \
  -H 'Content-Type: application/json' -d '{"email":"depot@demo.invalid","password":"not-it"}')"

head_ "Every UI page is served"
for p in console resources catalogue forecast lots market capsules compliance map messages settings shipping; do
  is "/ui/$p.html" 200 "$(code "$UI/$p.html")"
done

head_ "Pages do not reach for a CDN"
# The artifact CSP blocks external hosts, and an offline-first tool should not
# be loading anything over the network anyway. Known exceptions are named, not
# ignored, so a new one shows up here.
CDN_HITS=0
for p in console resources catalogue forecast lots market capsules compliance messages settings shipping; do
  if curl -s "$UI/$p.html" | grep -qE 'src="https?://|href="https?://.*\.css'; then
    bad "/ui/$p.html loads something external"
    CDN_HITS=$((CDN_HITS+1))
  fi
done
(( CDN_HITS == 0 )) && ok "no external script or stylesheet on the operator pages"
# map.html and bodies-map.html still use Leaflet/Cesium from a CDN. Named here
# rather than silently skipped, so it reads as outstanding work.
echo "    (map.html and bodies-map.html still use a CDN — known, and on the list)"

head_ "The catalogue carries the fields a carrier and a border ask for"
CAT="$(curl -s "$API/catalogue?limit=50" "${AUTH[@]}")"
at_least "catalogue entries" 4 "$(echo "$CAT" | jq 'length')"
is "lithium pack has its UN number" "UN3480" \
  "$(echo "$CAT" | jq -r '[.[]|select(.part_number=="NW-BAT-4800")][0].un_number // "missing"')"
is "and an HS code" "8507.60" \
  "$(echo "$CAT" | jq -r '[.[]|select(.part_number=="NW-BAT-4800")][0].hs_code // "missing"')"

head_ "Life limits surface the unit that is past its clock"
LIFE="$(curl -s "$API/lots/life?at_pct=0.8" "${AUTH[@]}")"
at_least "units near or past a life limit" 2 "$(echo "$LIFE" | jq -r '.count')"
at_least "units past a limit" 1 "$(echo "$LIFE" | jq -r '.pastLimit')"
# Membership, not position. The demo's buying organisation is the outpost's
# own, so anything else already in it legitimately appears here too — asserting
# which unit sorts first would fail on a deployment that simply has more stock.
if echo "$LIFE" | jq -e '[.units[]|select(.pastLimit).serial]|index("SN-PACK-003")' >/dev/null; then
  ok "the seeded over-limit unit is reported"
else
  bad "SN-PACK-003 is not among the past-limit units"
fi

head_ "A dark site still counts, and says it is stale"
ROLL="$(curl -s "$API/rollup/inventory" "${AUTH[@]}")"
at_least "sources answering live" 1 "$(echo "$ROLL" | jq -r '.sources_live')"
at_least "sources answering from a snapshot" 1 "$(echo "$ROLL" | jq -r '.sources_stale')"
is "the total is not claimed complete" "false" "$(echo "$ROLL" | jq -r '.complete')"
is "and it is not a floor either" "false" "$(echo "$ROLL" | jq -r '.floor')"
at_least "hose counted from the dark site" 1 \
  "$(echo "$ROLL" | jq -r '[.items[]|select(.name|test("Hydraulic hose 3/4in$"))][0].stale_quantity // 0')"

head_ "fresh_only gives the number nobody has to caveat"
FRESH="$(curl -s "$API/rollup/inventory?fresh_only=true" "${AUTH[@]}")"
is "snapshots excluded" 0 "$(echo "$FRESH" | jq -r '.sources_stale')"
is "and the total becomes a floor" "true" "$(echo "$FRESH" | jq -r '.floor')"

head_ "Carrier acceptance is checked off the catalogue"
CELL="$(echo "$CAT" | jq -r '[.[]|select(.un_number=="UN3480")][0].id')"
USPS="$(curl -s -X POST "$API/carriers/check" "${AUTH[@]}" -H 'Content-Type: application/json' \
  -d "{\"carrier\":\"usps\",\"service\":\"usps_priority_air\",\"inventory_ids\":[\"$CELL\"]}")"
is "USPS air refuses the lithium pack" "false" "$(echo "$USPS" | jq -r '.acceptable')"
FEDEX="$(curl -s -X POST "$API/carriers/check" "${AUTH[@]}" -H 'Content-Type: application/json' \
  -d "{\"carrier\":\"fedex\",\"inventory_ids\":[\"$CELL\"]}")"
is "FedEx accepts it with paperwork" "true" "$(echo "$FEDEX" | jq -r '.acceptable')"
at_least "and says a declaration is needed" 1 "$(echo "$FEDEX" | jq -r '.requiresDeclaration|length')"

head_ "A carrier handover is recorded as attested, not signed"
SHIP="$(curl -s "$API/carriers/shipments" "${AUTH[@]}")"
at_least "shipments" 1 "$(echo "$SHIP" | jq -r '.count')"
is "one is in flight" 1 "$(echo "$SHIP" | jq -r '.inFlight')"
SHIP_ID="$(echo "$SHIP" | jq -r '.shipments[0].id')"
at_least "tracking events" 2 "$(curl -s "$API/carriers/shipments/$SHIP_ID" "${AUTH[@]}" | jq -r '.events|length')"

head_ "Organisations cannot see each other"
MINE="$(echo "$CAT" | jq 'length')"
THEIRS="$(curl -s "$API/catalogue?limit=50" -H "Authorization: Bearer $OTHER" | jq 'length')"
is "the other organisation sees none of this catalogue" 0 "$THEIRS"
at_least "while we see ours" 4 "$MINE"
is "their address book is empty too" 0 \
  "$(curl -s "$API/addresses" -H "Authorization: Bearer $OTHER" | jq -r '.count')"

head_ "Key custody knows whether this outpost could be restored"
KEYS="$(curl -s "$API/ops/keys/status" "${AUTH[@]}")"
is "keys are on disk" "true" "$(echo "$KEYS" | jq -r '.keysOnDisk')"
STANDING="$(echo "$KEYS" | jq -r '.standing')"
case "$STANDING" in
  ok)     ok "a recent key backup exists" ;;
  stale)  ok "standing reported as stale (a backup exists but is old)" ;;
  never)  ok "standing reported as 'never' — correct, and the thing to fix" ;;
  *)      bad "unexpected standing '$STANDING'" ;;
esac

head_ "DTN refuses what it should"
is "a user token cannot deliver a bundle" 403 \
  "$(code -X POST "$API/dtn/receive" "${AUTH[@]}" -H 'Content-Type: application/json' \
     -d '{"v":2,"payload":{"x":1},"src_node_id":"00000000-0000-4000-8000-000000000000"}')"
is "the relay no longer accepts bundles" 410 \
  "$(code -X POST "${API%:4000/api}:3100/inbox" -H 'Content-Type: application/json' \
     -d '{"src_node_id":null,"payload":{"forged":1}}' 2>/dev/null || echo 000)"

head_ "Conversations are visible to their participants only"
CHAT="$(curl -s "$API/chat" "${AUTH[@]}")"
at_least "threads for the depot lead" 2 "$(echo "$CHAT" | jq -r '.count')"
DEAL="$(echo "$CHAT" | jq -r '[.threads[]|select(.kind=="deal")][0].id // empty')"
[[ -n "$DEAL" ]] && ok "a deal thread exists, anchored to a bid" || bad "no deal thread found"
is "the carrier can see the deal thread too" 200 \
  "$(code "$API/chat/$DEAL" -H "Authorization: Bearer $OTHER")"
THIRD="$(curl -s -X POST "$API/local-auth/login" -H 'Content-Type: application/json' \
  -d "{\"email\":\"picker@demo.invalid\",\"password\":\"$PASSWORD\"}" | jq -r '.token // empty')"
is "a colleague who is not a participant cannot" 404 \
  "$(code "$API/chat/$DEAL" -H "Authorization: Bearer $THIRD")"

head_ "A compliance hold blocks by default"
at_least "active holds" 1 \
  "$(curl -s "$API/compliance/holds" "${AUTH[@]}" | jq -r 'if type=="array" then length else (.count // 0) end')"

echo
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
if (( FAIL == 0 )); then
  echo "✓ $PASS checks passed"
  exit 0
else
  echo "✗ $FAIL of $((PASS+FAIL)) checks failed"
  exit 1
fi
