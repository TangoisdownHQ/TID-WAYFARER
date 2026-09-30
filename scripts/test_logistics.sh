#!/usr/bin/env bash
# Smoke test for the BOM / tags / kits layer.
#
# Two phases:
#   (1) API surface  — hit the new endpoints unauthenticated and confirm we
#       get 401/UNAUTHORIZED (not 404). 404 would mean the route isn't
#       registered. 401 means the route exists and the auth gate is enforced.
#   (2) DB layer     — exec into postgres and exercise the new tables +
#       the recursive BOM CTE end-to-end with throwaway data.
#
# Doesn't require a JWT. Run after `docker compose up -d --build tidasone-core-api`.

set -euo pipefail

API="${API_BASE:-http://127.0.0.1:4000}"

pass() { echo "✅ $1"; }
fail() { echo "❌ $1"; exit 1; }

# ─── 1. API surface ────────────────────────────────────────────────────────
echo "── phase 1: route registration ──"

expect_401() {
  local method="$1" path="$2" label="$3"
  local code
  code=$(curl -s -o /dev/null -w "%{http_code}" -X "$method" "$API$path")
  case "$code" in
    401|403) pass "$label ($method $path → $code)" ;;
    404)     fail "$label missing — got 404 (route not registered)" ;;
    *)       fail "$label unexpected code $code for $method $path" ;;
  esac
}

ZERO="00000000-0000-0000-0000-000000000000"
# Note: asset endpoints sit at /api/assets/assets/... — the inner Router inside
# routes/assets.rs prefixes its routes with /assets, and main.rs nests that under
# /assets, so the merged logistics routes inherit the same double-nesting.
# /api/kits/... is clean because it has its own .nest("/kits", kit_routes()) mount.
expect_401 GET    "/api/assets/assets/$ZERO/parts"             "BOM list"
expect_401 GET    "/api/assets/assets/$ZERO/parts/tree"        "BOM tree (recursive)"
expect_401 POST   "/api/assets/assets/$ZERO/parts"             "BOM attach"
expect_401 DELETE "/api/assets/assets/$ZERO/parts/$ZERO"       "BOM detach"
expect_401 GET    "/api/assets/assets/$ZERO/used-in"           "ancestors (used-in)"
expect_401 GET    "/api/assets/assets/$ZERO/tags"              "tag list"
expect_401 POST   "/api/assets/assets/$ZERO/tags"              "tag add"
expect_401 DELETE "/api/assets/assets/$ZERO/tags/urgent"       "tag remove"
expect_401 GET    "/api/assets/assets/by-tag/urgent"           "assets-by-tag"
expect_401 GET    "/api/kits"                                  "kit list"
expect_401 POST   "/api/kits"                                  "kit create"
expect_401 GET    "/api/kits/$ZERO"                            "kit get"
expect_401 POST   "/api/kits/$ZERO/items"                      "kit add item"
expect_401 DELETE "/api/kits/$ZERO/items/$ZERO"                "kit remove item"


# ─── 2. DB layer ────────────────────────────────────────────────────────────
echo
echo "── phase 2: schema + recursive CTE ──"

PSQL="sudo docker exec -i tidasone-db-v2 psql -U postgres -d tidasone -v ON_ERROR_STOP=1 -tA"

# Confirm the four new tables exist
COUNTS=$($PSQL -c "
  SELECT
    (SELECT count(*) FROM asset_parts)     ||','||
    (SELECT count(*) FROM asset_tags)      ||','||
    (SELECT count(*) FROM asset_kits)      ||','||
    (SELECT count(*) FROM asset_kit_items)
")
echo "    rowcounts (parts,tags,kits,kit_items): $COUNTS"
pass "logistics tables queryable"

# Seed a 3-level BOM tree + tag + kit, then probe it, then clean up.
$PSQL <<'SQL'
BEGIN;

-- minimal user (FK target)
INSERT INTO users (id, username, email)
VALUES ('11111111-1111-1111-1111-111111111111', 'testuser', 'test-logistics@tidhq.net')
ON CONFLICT (id) DO NOTHING;

-- three assets:  rover ─┬─ battery_pack ── cell_A
--                       └─ wheel_module
INSERT INTO assets (id, owner_id, name, location, part_number, lot_number) VALUES
  ('aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa', '11111111-1111-1111-1111-111111111111', 'TEST Rover',          'Jezero', 'ROVER-T1', NULL),
  ('bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb', '11111111-1111-1111-1111-111111111111', 'TEST Battery Pack',   'Jezero', 'BAT-PACK', 'PACK-2026-001'),
  ('cccccccc-cccc-cccc-cccc-cccccccccccc', '11111111-1111-1111-1111-111111111111', 'TEST 18650 Cell',     'Jezero', 'CELL-18650', 'LG-2024-0042'),
  ('dddddddd-dddd-dddd-dddd-dddddddddddd', '11111111-1111-1111-1111-111111111111', 'TEST Wheel Module',   'Jezero', 'WHEEL-M1', NULL)
ON CONFLICT (id) DO NOTHING;

INSERT INTO asset_parts (parent_id, child_id, qty, position, criticality) VALUES
  ('aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa', 'bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb', 1, 'bay-1', 'safety'),
  ('aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa', 'dddddddd-dddd-dddd-dddd-dddddddddddd', 6, 'wheels', 'operational'),
  ('bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb', 'cccccccc-cccc-cccc-cccc-cccccccccccc', 96, 'pack-cells', 'safety')
ON CONFLICT (parent_id, child_id, position) DO NOTHING;

INSERT INTO asset_tags (asset_id, tag) VALUES
  ('aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa', 'mars-bound'),
  ('aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa', 'urgent')
ON CONFLICT DO NOTHING;

INSERT INTO asset_kits (id, name, description, owner_id) VALUES
  ('eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee', 'TEST Mars Repair Kit #3', 'Spare cells + tools', '11111111-1111-1111-1111-111111111111')
ON CONFLICT (id) DO NOTHING;

INSERT INTO asset_kit_items (kit_id, asset_id, qty) VALUES
  ('eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee', 'cccccccc-cccc-cccc-cccc-cccccccccccc', 12)
ON CONFLICT DO NOTHING;

COMMIT;
SQL
pass "test fixtures inserted"

# Recursive BOM: from rover should return 3 nodes (battery, wheel, cell)
BOM_NODES=$($PSQL -c "
  WITH RECURSIVE bom AS (
    SELECT parent_id, child_id, 0::int AS depth FROM asset_parts
    WHERE parent_id = 'aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa'::uuid AND removed_at IS NULL
    UNION ALL
    SELECT ap.parent_id, ap.child_id, b.depth + 1 FROM asset_parts ap
    JOIN bom b ON ap.parent_id = b.child_id WHERE ap.removed_at IS NULL AND b.depth < 10
  )
  SELECT count(*) FROM bom
")
echo "    BOM tree from rover: $BOM_NODES descendants"
[ "$BOM_NODES" = "3" ] || fail "expected 3 BOM descendants, got $BOM_NODES"
pass "recursive BOM query returns 3 (battery → cell, wheel)"

# Recall-by-lot: every assembly that transitively contains lot LG-2024-0042
RECALL=$($PSQL -c "
  WITH RECURSIVE ancestors AS (
    SELECT parent_id, child_id FROM asset_parts ap
    JOIN assets a ON a.id = ap.child_id
    WHERE a.lot_number = 'LG-2024-0042' AND ap.removed_at IS NULL
    UNION ALL
    SELECT ap.parent_id, ap.child_id FROM asset_parts ap
    JOIN ancestors anc ON ap.child_id = anc.parent_id WHERE ap.removed_at IS NULL
  )
  SELECT string_agg(DISTINCT a.name, ',' ORDER BY a.name)
  FROM ancestors anc JOIN assets a ON a.id = anc.parent_id
")
echo "    lot LG-2024-0042 is inside: $RECALL"
echo "$RECALL" | grep -q "TEST Battery Pack" || fail "lot recall missing battery pack"
echo "$RECALL" | grep -q "TEST Rover"        || fail "lot recall missing rover"
pass "lot-recall walk through BOM returns all containing assemblies"

# Tag join
TAG_HITS=$($PSQL -c "
  SELECT count(*) FROM asset_tags t JOIN assets a ON a.id = t.asset_id WHERE t.tag = 'urgent'
")
[ "$TAG_HITS" = "1" ] || fail "expected 1 'urgent' asset, got $TAG_HITS"
pass "tag → asset join works"

# Kit join
KIT_ITEMS=$($PSQL -c "
  SELECT count(*) FROM asset_kit_items WHERE kit_id = 'eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee'
")
[ "$KIT_ITEMS" = "1" ] || fail "expected 1 kit item, got $KIT_ITEMS"
pass "kit item insert + read works"

# Cleanup fixtures
$PSQL <<'SQL'
BEGIN;
DELETE FROM asset_kit_items WHERE kit_id = 'eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee';
DELETE FROM asset_kits      WHERE id     = 'eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee';
DELETE FROM asset_tags      WHERE asset_id = 'aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa';
DELETE FROM asset_parts     WHERE parent_id IN (
  'aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa',
  'bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb');
DELETE FROM assets          WHERE owner_id = '11111111-1111-1111-1111-111111111111';
DELETE FROM users           WHERE id       = '11111111-1111-1111-1111-111111111111';
COMMIT;
SQL
pass "test fixtures cleaned up"

echo
echo "🎉 BOM + tags + kits layer validated end-to-end."
