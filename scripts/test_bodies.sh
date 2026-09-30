#!/usr/bin/env bash
# Smoke test for the bodies catalog endpoints.
# Default targets the Core outpost exposed on host port 4000 by docker-compose.
# Override: API_BASE=http://127.0.0.1:3000 ./test_bodies.sh

set -euo pipefail

API="${API_BASE:-http://127.0.0.1:4000}/api/bodies"

pass() { echo "✅ $1"; }
fail() { echo "❌ $1"; exit 1; }

echo "[1] List all bodies (expect at least 25 solar-system entries)..."
ALL=$(curl -sf "$API")
COUNT=$(echo "$ALL" | jq 'length')
echo "    returned: $COUNT bodies"
[ "$COUNT" -ge 25 ] || fail "expected >= 25 bodies, got $COUNT"
pass "catalog reachable"

echo "[2] GET /api/bodies/399 (Earth)..."
EARTH=$(curl -sf "$API/399")
NAME=$(echo "$EARTH" | jq -r .name)
FRAME=$(echo "$EARTH" | jq -r .frame)
[ "$NAME" = "Earth" ] || fail "expected Earth, got '$NAME'"
[ "$FRAME" = "IAU_EARTH" ] || fail "expected IAU_EARTH frame, got '$FRAME'"
pass "Earth resolves correctly"

echo "[3] GET /api/bodies/499 (Mars) + check radius..."
MARS_R=$(curl -sf "$API/499" | jq -r .radius_km)
[ "$MARS_R" = "3389.5" ] || fail "expected Mars radius 3389.5, got '$MARS_R'"
pass "Mars radius matches NAIF"

echo "[4] Filter: ?class=moon&parent_id=499 (Phobos + Deimos)..."
MOONS=$(curl -sf "$API?class=moon&parent_id=499")
MOON_NAMES=$(echo "$MOONS" | jq -r '.[].name' | sort | tr '\n' ',')
echo "    martian moons: $MOON_NAMES"
echo "$MOON_NAMES" | grep -q "Deimos" || fail "Deimos missing"
echo "$MOON_NAMES" | grep -q "Phobos" || fail "Phobos missing"
pass "parent-filter works"

echo "[5] Filter: ?class=exoplanet (expect 16+ entries)..."
EXO_COUNT=$(curl -sf "$API?class=exoplanet" | jq 'length')
[ "$EXO_COUNT" -ge 16 ] || fail "expected >= 16 exoplanets, got $EXO_COUNT"
pass "exoplanet catalog seeded ($EXO_COUNT entries)"

echo "[6] GET /api/bodies/301/features (Moon → Apollo sites + craters)..."
LUNAR=$(curl -sf "$API/301/features")
LUNAR_COUNT=$(echo "$LUNAR" | jq 'length')
echo "    returned: $LUNAR_COUNT features"
[ "$LUNAR_COUNT" -ge 12 ] || fail "expected >= 12 lunar features, got $LUNAR_COUNT"
echo "$LUNAR" | jq -r '.[].name' | grep -q "Tranquility Base" || fail "Tranquility Base missing"
echo "$LUNAR" | jq -r '.[].name' | grep -q "Shackleton Crater" || fail "Shackleton Crater missing"
pass "lunar feature list complete"

echo "[7] GET /api/bodies/499/features?type=landing_site (Mars rover sites)..."
SITES=$(curl -sf "$API/499/features?type=landing_site")
SITE_NAMES=$(echo "$SITES" | jq -r '.[].name' | sort | tr '\n' ',')
echo "    sites: $SITE_NAMES"
echo "$SITE_NAMES" | grep -q "Jezero" || fail "Jezero missing"
echo "$SITE_NAMES" | grep -q "Gale" || fail "Gale missing"
pass "Mars landing-site filter works"

echo "[8] GET /api/bodies/999999 (nonexistent → 404)..."
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" "$API/999999")
[ "$HTTP_CODE" = "404" ] || fail "expected 404, got $HTTP_CODE"
pass "missing body returns 404"

echo
echo "🎉 All bodies endpoints smoke-tested OK."
