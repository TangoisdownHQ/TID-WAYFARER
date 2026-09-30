-- Mass and volume budgets, and the shared capsules they make possible.
--
-- Every launch sells two things: mass and volume. A manifest that exceeds
-- either is worthless, and nothing in the schema knew what anything weighed.
-- Orders carried `quantity` and `unit` — 40 "each" of a thing tells you
-- nothing about whether it flies.
--
-- This is also the missing half of capsule sharing. Pooling several parties'
-- cargo into one hull is only possible if you can answer "does it fit", which
-- is a mass *and* a volume question, and settlement has to split by the share
-- each party actually consumed.

-- === Per-unit physical properties ===
-- Nullable on purpose: a farm tracking seed by the kilo does not need a
-- volume, and forcing one would be noise. Absence means "unknown", which the
-- manifest treats differently from zero.
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS unit_mass_kg    NUMERIC;
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS unit_volume_m3  NUMERIC;

ALTER TABLE inventory_lots ADD COLUMN IF NOT EXISTS unit_mass_kg   NUMERIC;
ALTER TABLE inventory_lots ADD COLUMN IF NOT EXISTS unit_volume_m3 NUMERIC;

-- Orders state what they need moved. Filled from the inventory item where one
-- is known, or given directly for a service/bulk order.
ALTER TABLE orders ADD COLUMN IF NOT EXISTS mass_kg   NUMERIC;
ALTER TABLE orders ADD COLUMN IF NOT EXISTS volume_m3 NUMERIC;


-- === Capsules ===
-- A hull with a departure, a route and a budget. "Capsule" is the space case;
-- the same row models a container, a truck or a pallet — anything with a
-- finite mass and volume leaving at a known time.
CREATE TABLE IF NOT EXISTS capsules (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name            TEXT NOT NULL,
    operator_id     UUID REFERENCES users(id) ON DELETE SET NULL,
    operator_outpost UUID,

    -- The budget. Both required: a capsule with no limit is not a capsule,
    -- and cargo that fits by mass routinely fails to fit by volume.
    mass_capacity_kg   NUMERIC NOT NULL CHECK (mass_capacity_kg   > 0),
    volume_capacity_m3 NUMERIC NOT NULL CHECK (volume_capacity_m3 > 0),

    origin_body_id      INT,
    destination_body_id INT,
    origin_address      TEXT,
    destination_address TEXT,

    departs_at      TIMESTAMPTZ,
    arrives_at      TIMESTAMPTZ,

    --   open      - accepting cargo
    --   sealed    - manifest closed, not yet departed
    --   in_transit
    --   arrived
    --   cancelled
    status          TEXT NOT NULL DEFAULT 'open',

    notes           TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT capsules_status_check CHECK (
        status IN ('open','sealed','in_transit','arrived','cancelled')
    )
);

CREATE INDEX IF NOT EXISTS idx_capsules_status  ON capsules (status);
CREATE INDEX IF NOT EXISTS idx_capsules_departs ON capsules (departs_at);


-- === Manifest: which order is riding in which capsule ===
-- One row per order per capsule. Mass and volume are snapshotted at booking
-- rather than joined live: what was agreed is what settles, and an order
-- edited after booking must not silently change what the capsule owes.
CREATE TABLE IF NOT EXISTS capsule_manifest (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    capsule_id    UUID NOT NULL REFERENCES capsules(id) ON DELETE CASCADE,
    order_id      UUID NOT NULL REFERENCES orders(id)   ON DELETE RESTRICT,
    -- Who booked the slot, for split settlement.
    shipper_id    UUID REFERENCES users(id) ON DELETE SET NULL,

    mass_kg       NUMERIC NOT NULL CHECK (mass_kg   >= 0),
    volume_m3     NUMERIC NOT NULL CHECK (volume_m3 >= 0),

    -- What this party pays, and its share of the hull. share_of_mass is
    -- recorded so a cost split can be reproduced after the fact even if the
    -- capsule's capacity is later corrected.
    price         NUMERIC,
    share_of_mass NUMERIC,

    booked_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    notes         TEXT,

    -- An order rides once per capsule.
    UNIQUE (capsule_id, order_id)
);

CREATE INDEX IF NOT EXISTS idx_manifest_capsule ON capsule_manifest (capsule_id);
CREATE INDEX IF NOT EXISTS idx_manifest_order   ON capsule_manifest (order_id);
CREATE INDEX IF NOT EXISTS idx_manifest_shipper ON capsule_manifest (shipper_id);

COMMENT ON TABLE capsules IS
    'A hull with a finite mass and volume budget: capsule, container, truck or pallet.';
COMMENT ON TABLE capsule_manifest IS
    'Cargo booked into a capsule. Mass and volume are snapshotted at booking.';
