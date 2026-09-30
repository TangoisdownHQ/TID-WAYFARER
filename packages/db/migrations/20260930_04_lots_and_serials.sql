-- Lot, batch and serial tracking.
--
-- Inventory was a name and a number. That answers "how much do we have" and
-- nothing else. Real logistics has to answer:
--
--   · which units came from the batch that was just recalled?
--   · what expires before the next resupply window?
--   · which serial is installed in which asset, and what is its pedigree?
--
-- None of those are answerable from a quantity. For space work the serial and
-- its pedigree are not optional — every flight part carries one.
--
-- Lots are an optional refinement, not a replacement: an inventory row may
-- have no lots (untracked bulk) or many. Where lots exist they are the
-- authoritative count and `inventory.quantity` is a cached roll-up, which
-- `/api/inventory/:id/lots` reports a discrepancy against rather than silently
-- reconciling — a mismatch is a physical-world problem (miscount, shrinkage,
-- unrecorded issue) and hiding it would destroy the signal.

CREATE TABLE IF NOT EXISTS inventory_lots (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    inventory_id    UUID NOT NULL REFERENCES inventory(id) ON DELETE CASCADE,

    -- Batch identity. lot_code groups units made or received together;
    -- serial identifies exactly one unit (quantity is then 1).
    lot_code        TEXT,
    serial          TEXT,

    quantity        NUMERIC NOT NULL DEFAULT 1 CHECK (quantity >= 0),

    -- Dates that drive decisions.
    manufactured_at TIMESTAMPTZ,
    expires_at      TIMESTAMPTZ,
    received_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    supplier        TEXT,
    -- Free-form provenance: heat number, cure date, test reports, cert ids,
    -- prior installations. Deliberately JSONB — pedigree fields vary by part
    -- and by customer, and a fixed column set would be wrong within a week.
    pedigree        JSONB NOT NULL DEFAULT '{}'::jsonb,

    --   available   - on the shelf, usable
    --   reserved    - promised to an order, not yet shipped
    --   in_transit  - handed to a carrier
    --   consumed    - used or installed
    --   quarantined - suspect: failed inspection, recall, damaged
    --   expired     - past expires_at
    status          TEXT NOT NULL DEFAULT 'available',

    notes           TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT inventory_lots_status_check CHECK (
        status IN ('available','reserved','in_transit','consumed','quarantined','expired')
    ),
    -- A lot must be identifiable as something. An anonymous lot is just a
    -- quantity, which is what inventory already stores.
    CONSTRAINT inventory_lots_identified CHECK (
        lot_code IS NOT NULL OR serial IS NOT NULL
    ),
    -- A serial names one physical unit.
    CONSTRAINT inventory_lots_serial_is_singular CHECK (
        serial IS NULL OR quantity <= 1
    )
);

-- A serial is globally unique where present: the same physical unit cannot be
-- in two places. Partial so untracked lots are unaffected.
CREATE UNIQUE INDEX IF NOT EXISTS idx_inventory_lots_serial
    ON inventory_lots (serial) WHERE serial IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_inventory_lots_inventory ON inventory_lots (inventory_id);
CREATE INDEX IF NOT EXISTS idx_inventory_lots_status    ON inventory_lots (status);

-- Recall: "where did batch 7 go?" must be one indexed lookup, fabric-wide.
CREATE INDEX IF NOT EXISTS idx_inventory_lots_lot_code
    ON inventory_lots (lot_code) WHERE lot_code IS NOT NULL;

-- Expiry sweep: what goes off before the next resupply window. Partial,
-- because consumed and expired stock is not actionable.
CREATE INDEX IF NOT EXISTS idx_inventory_lots_expiring
    ON inventory_lots (expires_at)
    WHERE expires_at IS NOT NULL AND status IN ('available','reserved');

COMMENT ON TABLE inventory_lots IS
    'Batch/serial level detail under an inventory row. Optional: absent means untracked bulk.';
