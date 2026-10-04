#!/usr/bin/env bash
# Load a realistic scenario, so the tool can be shown rather than described.
#
# Until now the only way to see any of this work was to hand-roll curl calls,
# which meant nobody else could evaluate it and the person who could kept
# polluting their own development database doing it.
#
# What this builds: two organisations trading with each other, a catalogue with
# the awkward cases in it (a lithium pack no carrier will fly, a part past its
# cycle limit), stock across several locations with some of it short, an order
# with competing bids, a carrier shipment with its custody handover, a dark
# outpost whose last-known stock still counts, and a conversation on each side.
#
# Users are created through the API so their password hashes are real; bulk data
# goes in by SQL because it is faster and deterministic. Re-running is safe: it
# removes its own previous data first, and touches nothing else.
#
# Usage:
#   scripts/seed_demo.sh [--api URL] [--db-container NAME] [--db NAME] [--reset]

set -euo pipefail

API="http://localhost:4000/api"
DB_CONTAINER="tidasone-db-v2"
DB_NAME="tidasone"
RESET_ONLY=0
PASSWORD="demo-outpost-password"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --api)          API="$2"; shift 2 ;;
    --db-container) DB_CONTAINER="$2"; shift 2 ;;
    --db)           DB_NAME="$2"; shift 2 ;;
    --reset)        RESET_ONLY=1; shift ;;
    -h|--help)      sed -n '2,18p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

die() { echo "✗ $*" >&2; exit 1; }
note() { echo "  $*"; }
psql_q() { docker exec -i "$DB_CONTAINER" psql -U postgres -d "$DB_NAME" -qtA -v ON_ERROR_STOP=1; }

command -v docker >/dev/null || die "docker is required"
command -v jq     >/dev/null || die "jq is required"
curl -sf "$API/health" >/dev/null || die "no outpost at $API — is the stack up?"

# Everything this script creates is tagged, so a re-run can remove exactly its
# own rows and nothing else. A seed script that cannot be run twice gets run
# once and then worked around.
TAG="seed-demo"

echo "▸ Clearing any previous demo data"
# Demo rows are identified three ways, because they no longer all live in one
# organisation: the buying organisation is now the outpost's own, so deleting
# "the demo org's data" would either miss them or take real data with it.
#
#   1. Explicit ids of the form dN000000-0000-4000-8000-… — every row this
#      script inserts with a fixed id uses that shape, and gen_random_uuid
#      will not produce one.
#   2. Users at @demo.invalid.
#   3. The carrier organisation, which this script does create (slug demo-%).
#      The outpost's own organisation must never match, or removing the demo
#      would unenrol the outpost and take its rollup down with it.
psql_q <<SQL
BEGIN;
CREATE TEMP TABLE demo_orgs  AS SELECT id FROM organisations WHERE slug LIKE 'demo-%';
CREATE TEMP TABLE demo_users AS SELECT id FROM users WHERE email LIKE '%@demo.invalid';

CREATE TEMP TABLE demo_ship AS
  SELECT id FROM carrier_shipments WHERE id::text LIKE 'd_000000-0000-4000-8000-%';
CREATE TEMP TABLE demo_items AS
  SELECT id FROM inventory WHERE id::text LIKE 'd_000000-0000-4000-8000-%';
CREATE TEMP TABLE demo_orders AS
  SELECT id FROM orders WHERE id::text LIKE 'd_000000-0000-4000-8000-%';
CREATE TEMP TABLE demo_threads AS
  SELECT id FROM chat_threads WHERE id::text LIKE 'd_000000-0000-4000-8000-%'
     OR created_by IN (SELECT id FROM demo_users);

DELETE FROM carrier_tracking_events WHERE shipment_id IN (SELECT id FROM demo_ship);
DELETE FROM custody_receipts WHERE carrier_shipment_id IN (SELECT id FROM demo_ship)
   OR order_id IN (SELECT id FROM demo_orders);
DELETE FROM carrier_shipments  WHERE id IN (SELECT id FROM demo_ship);
DELETE FROM carrier_rate_quotes WHERE org_id IN (SELECT id FROM demo_orgs)
   OR from_address_id::text LIKE 'd_000000-0000-4000-8000-%';
DELETE FROM carrier_accounts WHERE account_ref = 'A1B2C3';

DELETE FROM chat_messages     WHERE thread_id IN (SELECT id FROM demo_threads);
DELETE FROM chat_participants WHERE thread_id IN (SELECT id FROM demo_threads);
DELETE FROM chat_threads      WHERE id IN (SELECT id FROM demo_threads);

DELETE FROM compliance_holds WHERE order_id IN (SELECT id FROM demo_orders);
DELETE FROM fulfillments     WHERE order_id IN (SELECT id FROM demo_orders);
UPDATE orders SET accepted_bid_id = NULL WHERE id IN (SELECT id FROM demo_orders);
DELETE FROM bids   WHERE order_id IN (SELECT id FROM demo_orders);
DELETE FROM orders WHERE id IN (SELECT id FROM demo_orders);

DELETE FROM inventory_lots WHERE inventory_id IN (SELECT id FROM demo_items);
DELETE FROM supplier_parts WHERE inventory_id IN (SELECT id FROM demo_items);
DELETE FROM part_relations WHERE from_id IN (SELECT id FROM demo_items)
   OR to_id IN (SELECT id FROM demo_items);
DELETE FROM inventory WHERE id IN (SELECT id FROM demo_items);

DELETE FROM addresses WHERE id::text LIKE 'd_000000-0000-4000-8000-%';

DELETE FROM replica_inventory WHERE node_id::text LIKE 'd_000000-0000-4000-8000-%';
DELETE FROM replica_sources   WHERE node_id::text LIKE 'd_000000-0000-4000-8000-%';
DELETE FROM node_registry     WHERE api_endpoint LIKE '%demo.invalid%';

DELETE FROM org_members   WHERE user_id IN (SELECT id FROM demo_users)
   OR org_id IN (SELECT id FROM demo_orgs);
DELETE FROM org_outposts  WHERE org_id IN (SELECT id FROM demo_orgs);
DELETE FROM organisations WHERE id IN (SELECT id FROM demo_orgs);
DELETE FROM auth_attempts WHERE email LIKE '%@demo.invalid';
DELETE FROM users WHERE id IN (SELECT id FROM demo_users);
COMMIT;
SQL
note "cleared"

if [[ "$RESET_ONLY" -eq 1 ]]; then
  echo "✓ Demo data removed. Nothing seeded (--reset)."
  exit 0
fi

# --- Users, through the API so the password hashes are real ---
#
# Not SQL: a seeded user who cannot actually sign in is a demo that fails at the
# first step, and getting an Argon2 hash right by hand in shell is not worth it.
echo "▸ Creating people"
make_user() {
  local email="$1" name="$2" kind="$3"
  local out
  out="$(curl -s -X POST "$API/local-auth/register" -H 'Content-Type: application/json' \
    -d "{\"username\":\"$name\",\"email\":\"$email\",\"password\":\"$PASSWORD\",\"account_type\":\"$kind\"}")"
  local id
  id="$(echo "$out" | jq -r '.user_id // empty')"
  [[ -n "$id" ]] || die "could not create $email: $out"
  echo "$id"
}

BUYER_ID="$(make_user   "depot@demo.invalid"   "Jess (Chicago depot)" both)"
PICKER_ID="$(make_user  "picker@demo.invalid"  "Ade (bay 3)"          buyer)"
CARRIER_ID="$(make_user "orbital@demo.invalid" "Rae (Orbital Freight)" seller)"
note "depot@demo.invalid, picker@demo.invalid, orbital@demo.invalid"

# The rollup is admin-gated, so the depot lead needs the outpost-level role.
# Done in SQL deliberately: there is no self-service path to admin, and there
# should not be.
psql_q <<SQL
UPDATE users SET role = 'admin', full_name = 'Jess Okonkwo' WHERE id = '$BUYER_ID';
UPDATE users SET full_name = 'Ade Fernandes'  WHERE id = '$PICKER_ID';
UPDATE users SET full_name = 'Rae Lindqvist'  WHERE id = '$CARRIER_ID';
SQL

# --- Which organisation does this outpost belong to? ---
#
# Exactly one: `idx_org_outposts_node` is unique on node_id. So the demo cannot
# invent a rival buying organisation and enrol it — an earlier version tried,
# and `ON CONFLICT DO NOTHING` swallowed the failure, leaving a demo org whose
# rollup never fanned out to peers because `own_org()` resolved to the other one.
#
# Instead the buying organisation *is* whichever the outpost already belongs to,
# and only the carrier is added. Non-destructive, and a more accurate picture:
# an outpost is one company's, and the second company is a counterparty on it.
echo "▸ Building the scenario"
CORE_NODE="$(curl -s "$API/outpost" -H "Authorization: Bearer $(curl -s -X POST "$API/local-auth/login" \
  -H 'Content-Type: application/json' \
  -d "{\"email\":\"depot@demo.invalid\",\"password\":\"$PASSWORD\"}" | jq -r .token)" \
  | jq -r '.nodeId // empty')"
[[ -n "$CORE_NODE" ]] || die "could not read this outpost's node id from $API/outpost"

BUYER_ORG="$(psql_q <<< "SELECT org_id FROM org_outposts WHERE node_id = '$CORE_NODE' LIMIT 1" | head -1)"
if [[ -z "$BUYER_ORG" ]]; then
  note "this outpost belongs to no organisation yet; enrolling a demo one"
  BUYER_ORG="d0000000-0000-4000-8000-000000000001"
  psql_q <<SQL
INSERT INTO organisations (id, name, slug, root_public_key, status)
VALUES ('$BUYER_ORG', 'Northwind Mining', 'demo-northwind', 'demo-root-northwind', 'active');
INSERT INTO org_outposts (org_id, node_id, node_public_key, certificate, issued_at)
VALUES ('$BUYER_ORG', '$CORE_NODE', 'demo-cert-key', '{}'::jsonb, NOW());
SQL
else
  note "buying organisation: $(psql_q <<< "SELECT name FROM organisations WHERE id='$BUYER_ORG'" | head -1)"
fi

psql_q <<SQL
BEGIN;

-- Only the carrier is created here. The buying organisation is whichever this
-- outpost already belongs to — see above. Two organisations on one outpost,
-- seeing nothing of each other's stock, is the whole point of the boundary
-- model and is what the smoke test checks.
INSERT INTO organisations (id, name, slug, root_public_key, status) VALUES
  ('d0000000-0000-4000-8000-000000000002', 'Orbital Freight Co','demo-orbital', 'demo-root-orbital', 'active');

INSERT INTO org_members (org_id, user_id, role) VALUES
  ('$BUYER_ORG', '$BUYER_ID',   'owner'),
  ('$BUYER_ORG', '$PICKER_ID',  'operator'),
  ('d0000000-0000-4000-8000-000000000002', '$CARRIER_ID', 'owner');


-- === Catalogue: the awkward cases, because those are what the tool is for ===
INSERT INTO inventory
  (id, owner_id, org_id, name, description, part_number, manufacturer, category, unit,
   quantity, threshold, location, serialised, unit_mass_kg, unit_volume_m3, lead_time_days,
   hs_code, country_of_origin, hazard_class, un_number, storage_temp_min_c, storage_temp_max_c,
   life_limit_cycles, shelf_life_days)
VALUES
  -- A lithium pack: UN3480, which no carrier will fly and most will refuse
  -- outright. The reason the catalogue records UN numbers at all.
  ('d1000000-0000-4000-8000-000000000001', '$BUYER_ID', '$BUYER_ORG',
   'EV traction pack 4.8kWh', 'Lithium-ion module for the haul fleet', 'NW-BAT-4800', 'Northwind',
   'battery', 'each', 6, 4, 'bay-3', true, 38.5, 0.09, 45,
   '8507.60', 'DE', '9', 'UN3480', 5, 35, 3000, NULL),

  -- Short at this location even though there is stock elsewhere: the thing a
  -- fabric-wide total hides.
  ('d1000000-0000-4000-8000-000000000002', '$BUYER_ID', '$BUYER_ORG',
   'Hydraulic hose 3/4in', 'Replacement for the loader arms', 'NW-HYD-075', 'Northwind',
   'consumable', 'each', 2, 12, 'bay-1', false, 4.2, 0.01, 10,
   '4009.31', 'US', NULL, NULL, NULL, NULL, NULL, NULL),

  ('d1000000-0000-4000-8000-000000000003', '$BUYER_ID', '$BUYER_ORG',
   'Drive motor 11kW', 'Traction motor, life limited by running hours', 'NW-MOT-11K', 'Northwind',
   'assembly', 'each', 3, 2, 'bay-3', true, 62.0, 0.14, 90,
   '8501.52', 'JP', NULL, NULL, NULL, NULL, NULL, NULL),

  ('d1000000-0000-4000-8000-000000000004', '$BUYER_ID', '$BUYER_ORG',
   'Coolant 50/50 premix', 'Bulk, decants to 20L', 'NW-CLT-050', 'Northwind',
   'consumable', 'L', 400, 100, 'tank-2', false, 1.04, 0.001, 7,
   '3820.00', 'US', NULL, NULL, -35, 60, NULL, 730);

-- Supersession: the old hose was replaced, so ordering it should offer the new.
INSERT INTO inventory
  (id, owner_id, org_id, name, part_number, manufacturer, category, unit, quantity, threshold, location)
VALUES ('d1000000-0000-4000-8000-000000000005', '$BUYER_ID', '$BUYER_ORG',
        'Hydraulic hose 3/4in (superseded)', 'NW-HYD-074', 'Northwind', 'consumable', 'each', 0, 0, 'bay-1');
INSERT INTO part_relations (from_id, to_id, kind, note)
VALUES ('d1000000-0000-4000-8000-000000000005', 'd1000000-0000-4000-8000-000000000002',
        'supersedes', 'NW-HYD-074 was replaced by NW-HYD-075 in 2025');

INSERT INTO supplier_parts
  (inventory_id, supplier_name, supplier_part_number, minimum_order_qty, unit_cost, currency, lead_time_days, preferred)
VALUES
  ('d1000000-0000-4000-8000-000000000002', 'Prairie Hydraulics', 'PH-0750-R2', 5, 41.50, 'TIDAT', 10, true),
  ('d1000000-0000-4000-8000-000000000002', 'Dockside Supply',    'DS-HH-34',  1, 48.00, 'TIDAT', 21, false),
  ('d1000000-0000-4000-8000-000000000001', 'Northwind Direct',   'NW-BAT-4800', 1, 2180.00, 'TIDAT', 45, true);

-- Serialised units, one of them past its cycle limit and still marked usable —
-- which is the alarm the lots page exists to raise.
-- A lot has no location of its own: it inherits the inventory row's. One
-- place for "where is this", which is why the lots page joins rather than
-- carrying a second copy that can disagree.
INSERT INTO inventory_lots
  (inventory_id, lot_code, serial, quantity, status, cycles_used, received_at, expires_at)
VALUES
  ('d1000000-0000-4000-8000-000000000001', 'NW-B-2601', 'SN-PACK-001', 1, 'available', 2940, NOW() - INTERVAL '400 days', NULL),
  ('d1000000-0000-4000-8000-000000000001', 'NW-B-2601', 'SN-PACK-002', 1, 'available', 1500, NOW() - INTERVAL '300 days', NULL),
  ('d1000000-0000-4000-8000-000000000001', 'NW-B-2602', 'SN-PACK-003', 1, 'available', 3050, NOW() - INTERVAL '500 days', NULL),
  -- Coolant has a shelf life, and this batch lapses inside the fortnight —
  -- so the expiry rail has something real on it too.
  ('d1000000-0000-4000-8000-000000000004', 'CLT-2609',  NULL,          200, 'available', 0, NOW() - INTERVAL '700 days', NOW() + INTERVAL '11 days');

-- === Addresses ===
INSERT INTO addresses (id, org_id, label, body_id, name, company, line1, city, region, postcode, country, phone, residential)
VALUES
  ('d2000000-0000-4000-8000-000000000001', '$BUYER_ORG',
   'Chicago depot', 399, 'Goods in', 'Northwind Mining', '2500 S Ashland Ave', 'Chicago', 'IL', '60608', 'US', '+13125550142', false),
  ('d2000000-0000-4000-8000-000000000002', '$BUYER_ORG',
   'Phoenix site', 399, 'Site office', 'Northwind Mining', '4100 E Broadway Rd', 'Phoenix', 'AZ', '85040', 'US', '+16025550199', false);

-- An off-Earth destination, to show the body model is not decorative.
INSERT INTO addresses (id, org_id, label, body_id, lat, lon, alt)
VALUES ('d2000000-0000-4000-8000-000000000003', '$BUYER_ORG',
        'Jezero cache', 499, 18.4447, 77.4508, -2540);

INSERT INTO carrier_accounts (org_id, carrier, provider, nickname, account_ref)
VALUES ('$BUYER_ORG', 'ups', 'manual', 'Northwind UPS', 'A1B2C3');

-- === An order with competing bids ===
INSERT INTO orders
  (id, requester_id, org_id, description, item_kind, target_part_number, quantity, unit,
   needed_by, delivery_address_id, mass_kg, volume_m3, max_price, status)
VALUES ('d3000000-0000-4000-8000-000000000001', '$BUYER_ID', '$BUYER_ORG',
        'Hydraulic hose 3/4in — 10 off, Phoenix site', 'part', 'NW-HYD-075', 10, 'each',
        NOW() + INTERVAL '9 days', 'd2000000-0000-4000-8000-000000000002', 42.0, 0.1, 900, 'bid');

-- bidder_outpost is the node that would carry it, not the company name. The
-- outpost and the organisation are deliberately separate things: one company
-- can bid from several outposts, and one outpost can host several companies.
INSERT INTO bids (id, order_id, bidder_id, bidder_outpost, price, transit_days, notes, status)
VALUES
  ('d4000000-0000-4000-8000-000000000001', 'd3000000-0000-4000-8000-000000000001', '$CARRIER_ID',
   '$CORE_NODE'::uuid, 640, 4, 'Ground via our Denver hub.', 'submitted'),
  ('d4000000-0000-4000-8000-000000000002', 'd3000000-0000-4000-8000-000000000001', '$CARRIER_ID',
   '$CORE_NODE'::uuid, 880, 2, 'Expedited. Air leg from Chicago.', 'submitted');

-- A second order, already delivered and awaiting settlement, so the token path
-- has something to look at.
INSERT INTO orders
  (id, requester_id, org_id, description, item_kind, quantity, unit, delivery_address_id, status)
VALUES ('d3000000-0000-4000-8000-000000000002', '$BUYER_ID', '$BUYER_ORG',
        'Coolant premix — 200L to Phoenix', 'consumable', 200, 'L',
        'd2000000-0000-4000-8000-000000000002', 'delivered');
INSERT INTO bids (id, order_id, bidder_id, bidder_outpost, price, transit_days, status)
VALUES ('d4000000-0000-4000-8000-000000000003', 'd3000000-0000-4000-8000-000000000002', '$CARRIER_ID',
        '$CORE_NODE'::uuid, 310, 5, 'accepted');
UPDATE orders SET accepted_bid_id = 'd4000000-0000-4000-8000-000000000003'
 WHERE id = 'd3000000-0000-4000-8000-000000000002';
INSERT INTO fulfillments (id, order_id, bid_id, shipper_id, status)
VALUES ('d5000000-0000-4000-8000-000000000001', 'd3000000-0000-4000-8000-000000000002',
        'd4000000-0000-4000-8000-000000000003', '$CARRIER_ID', 'delivered');

-- === A carrier leg, with its unverified custody handover ===
INSERT INTO carrier_shipments
  (id, org_id, order_id, carrier, service, provider, from_address_id, to_address_id,
   cost_amount, cost_currency, parcel_weight_kg, tracking_number, status, shipped_at, created_by)
VALUES ('d6000000-0000-4000-8000-000000000001', '$BUYER_ORG',
        'd3000000-0000-4000-8000-000000000002', 'ups', 'ups_ground', 'manual',
        'd2000000-0000-4000-8000-000000000001', 'd2000000-0000-4000-8000-000000000002',
        38.42, 'USD', 208.0, '1Z0DEM0E0123456789', 'in_transit', NOW() - INTERVAL '2 days', '$BUYER_ID');

-- Signed handover inside the fabric, then an unverified one to the carrier.
-- Side by side, which is the point: a reader can see which is which.
INSERT INTO custody_receipts
  (order_id, carrier_shipment_id, seq, from_node_id, to_node_id, from_label, to_label,
   event, condition, notes, occurred_at, verified)
VALUES
  ('d3000000-0000-4000-8000-000000000002', NULL, 1, NULL, '$CORE_NODE'::uuid,
   'Northwind bay 3', 'Chicago depot', 'pickup', 'ok',
   'Picked and sealed. Signed for inside the fabric.', NOW() - INTERVAL '3 days', true),
  ('d3000000-0000-4000-8000-000000000002', 'd6000000-0000-4000-8000-000000000001', 2,
   '$CORE_NODE'::uuid, NULL, 'Chicago depot', 'UPS 1Z0DEM0E0123456789', 'transfer', 'unknown',
   'Handed to a commercial carrier. No signature is possible; tracking number 1Z0DEM0E0123456789 is the evidence.',
   NOW() - INTERVAL '2 days', false);

INSERT INTO carrier_tracking_events (shipment_id, status, detail, location, occurred_at, source_ref)
VALUES
  ('d6000000-0000-4000-8000-000000000001', 'pre_transit', 'Label created', 'Chicago, IL, US', NOW() - INTERVAL '2 days', 'seed-1'),
  ('d6000000-0000-4000-8000-000000000001', 'in_transit',  'Departed facility', 'Chicago, IL, US', NOW() - INTERVAL '44 hours', 'seed-2'),
  ('d6000000-0000-4000-8000-000000000001', 'in_transit',  'Arrived at facility', 'Denver, CO, US', NOW() - INTERVAL '20 hours', 'seed-3');

-- === A dark outpost whose last-known stock still counts ===
--
-- Port 1 refuses instantly, so the rollup finds it unreachable without a
-- timeout and falls back to the snapshot — which is the behaviour worth seeing.
INSERT INTO node_registry (node_id, name, api_endpoint, public_key, status, last_seen)
VALUES ('d7000000-0000-4000-8000-000000000001', 'Jezero-Farm',
        'http://127.0.0.1:1/jezero.demo.invalid/api', 'demo-jezero-key', 'dark',
        NOW() - INTERVAL '3 days');

INSERT INTO replica_sources (node_id, org_id, outpost_name, body_id, region, generated_at, item_count)
VALUES ('d7000000-0000-4000-8000-000000000001', '$BUYER_ORG',
        'Jezero-Farm', 499, 'Jezero', NOW() - INTERVAL '3 days', 2);
INSERT INTO replica_inventory
  (node_id, org_id, name, unit, category, location, quantity, threshold, below_threshold, generated_at)
VALUES
  ('d7000000-0000-4000-8000-000000000001', '$BUYER_ORG',
   'Hydraulic hose 3/4in', 'each', 'consumable', 'dome-2', 18, 6, false, NOW() - INTERVAL '3 days'),
  ('d7000000-0000-4000-8000-000000000001', '$BUYER_ORG',
   'Coolant 50/50 premix', 'L', 'consumable', 'dome-2', 90, 150, true, NOW() - INTERVAL '3 days');

-- === Conversations ===
INSERT INTO chat_threads (id, kind, subject, org_id, created_by, last_message_at)
VALUES ('d8000000-0000-4000-8000-000000000001', 'direct', 'Bay 3 handover',
        '$BUYER_ORG', '$BUYER_ID', NOW() - INTERVAL '2 hours');
INSERT INTO chat_participants (thread_id, user_id, org_id) VALUES
  ('d8000000-0000-4000-8000-000000000001', '$BUYER_ID',  '$BUYER_ORG'),
  ('d8000000-0000-4000-8000-000000000001', '$PICKER_ID', '$BUYER_ORG');
INSERT INTO chat_messages (thread_id, sender_id, body, created_at) VALUES
  ('d8000000-0000-4000-8000-000000000001', '$BUYER_ID',
   'SN-PACK-003 is past its cycle limit and still showing available. Pull it before the morning shift.', NOW() - INTERVAL '3 hours'),
  ('d8000000-0000-4000-8000-000000000001', '$PICKER_ID',
   'Pulled and tagged. Quarantined it against NW-B-2602.', NOW() - INTERVAL '2 hours');

-- A deal thread: the only kind that crosses an organisation boundary, and it is
-- anchored to the bid that authorises it.
INSERT INTO chat_threads (id, kind, subject, bid_id, created_by, last_message_at)
VALUES ('d8000000-0000-4000-8000-000000000002', 'deal', NULL,
        'd4000000-0000-4000-8000-000000000001', '$BUYER_ID', NOW() - INTERVAL '40 minutes');
INSERT INTO chat_participants (thread_id, user_id, org_id) VALUES
  ('d8000000-0000-4000-8000-000000000002', '$BUYER_ID',   '$BUYER_ORG'),
  ('d8000000-0000-4000-8000-000000000002', '$CARRIER_ID', 'd0000000-0000-4000-8000-000000000002');
INSERT INTO chat_messages (thread_id, sender_id, body, created_at) VALUES
  ('d8000000-0000-4000-8000-000000000002', '$BUYER_ID',
   'Can you hold the ground rate if we add the battery pack to the same consignment?', NOW() - INTERVAL '1 hour'),
  ('d8000000-0000-4000-8000-000000000002', '$CARRIER_ID',
   'Not by air — UN3480. Ground only, and it needs a dangerous-goods declaration.', NOW() - INTERVAL '40 minutes');

-- A compliance hold, blocking by default.
INSERT INTO compliance_holds (order_id, reason, failed_kind, status)
VALUES ('d3000000-0000-4000-8000-000000000001',
        'Dangerous-goods declaration missing for UN3480 on the air service', 'hazmat', 'active');

COMMIT;
SQL

cat <<EOF

✓ Seeded.

  Sign in at ${API%/api}/ui/console.html

    depot@demo.invalid    $PASSWORD   Northwind owner, outpost admin
    picker@demo.invalid   $PASSWORD   Northwind operator
    orbital@demo.invalid  $PASSWORD   Orbital Freight owner (the other side)

  Worth looking at, and why:

    /ui/resources.html   Jezero-Farm is dark, and its stock still counts —
                         marked "as of 3 days ago" rather than silently dropped.
    /ui/lots.html        SN-PACK-003 is past its cycle limit and still available.
    /ui/market.html      Two bids on one order: 640 in 4 days against 880 in 2.
    /ui/shipping.html    A UPS leg mid-transit, and a custody chain with one
                         signed handover and one only attested.
    /ui/compliance.html  A hold blocking the air service over UN3480.
    /ui/messages.html    An internal thread, and a buyer-seller thread anchored
                         to the bid that authorises it.

  Sign in as orbital@demo.invalid to see the boundary from the other side: the
  same outpost, none of Northwind's stock.

  Remove it all:  scripts/seed_demo.sh --reset
EOF
