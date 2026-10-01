-- Inventory movements: the consumption ledger.
--
-- `inventory.quantity` is a level. A level answers "how much is there" and
-- nothing about rate, so it cannot answer the question that actually matters
-- for resupply: *how long until it runs out*. "5 kg of seed" is reassuring or
-- an emergency depending entirely on whether the farm burns 1 kg a month or
-- 12 kg a week, and nothing in the schema knew which.
--
-- Movements are the primitive; the level is the derivative. Recording each
-- change rather than only the result also makes shrinkage visible: a level
-- that drops with no issue recorded is a different problem from one that drops
-- because something was used, and overwriting the level loses that distinction
-- forever.

CREATE TABLE IF NOT EXISTS inventory_movements (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    inventory_id  UUID NOT NULL REFERENCES inventory(id)      ON DELETE CASCADE,
    lot_id        UUID          REFERENCES inventory_lots(id) ON DELETE SET NULL,

    -- Signed. Negative is stock leaving, positive is stock arriving. One
    -- column rather than separate in/out so a running balance is a plain SUM
    -- and cannot disagree with itself.
    delta         NUMERIC NOT NULL CHECK (delta <> 0),

    --   issue      - consumed, used, installed        (negative)
    --   receipt    - delivered in                     (positive)
    --   adjustment - a count correction               (either)
    --   transfer   - moved to or from another location
    --   spoilage   - expired, damaged, lost           (negative)
    reason        TEXT NOT NULL DEFAULT 'issue',

    -- What caused it, when there is one.
    order_id       UUID REFERENCES orders(id)       ON DELETE SET NULL,
    fulfillment_id UUID REFERENCES fulfillments(id) ON DELETE SET NULL,
    actor_id       UUID REFERENCES users(id)        ON DELETE SET NULL,

    note          TEXT,
    occurred_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT inventory_movements_reason_check CHECK (
        reason IN ('issue','receipt','adjustment','transfer','spoilage')
    )
);

-- The forecast query: movements for one item, newest first, inside a window.
CREATE INDEX IF NOT EXISTS idx_movements_item_time
    ON inventory_movements (inventory_id, occurred_at DESC);

-- Consumption only, which is what a burn rate is computed from.
CREATE INDEX IF NOT EXISTS idx_movements_consumption
    ON inventory_movements (inventory_id, occurred_at DESC)
    WHERE delta < 0;

CREATE INDEX IF NOT EXISTS idx_movements_lot ON inventory_movements (lot_id) WHERE lot_id IS NOT NULL;

-- How long resupply takes for this item, when it is known. Used to decide
-- whether the stock on hand outlasts the order it would take to replace it —
-- the comparison that turns a level into a decision.
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS lead_time_days NUMERIC;

COMMENT ON TABLE inventory_movements IS
    'Signed stock changes. The level is derived from these, not the other way round.';
