-- Supersession, supplier part numbers, life limits, customs codes and storage
-- conditions: the five things a catalogue needs before it can be trusted to
-- run a fleet.

-- === Customs ===
-- The declaration already generates; these are the two fields a border
-- actually demands and the document could not supply, so it printed a blank
-- and listed itself incomplete.
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS hs_code           TEXT;
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS country_of_origin TEXT;   -- ISO 3166-1 alpha-2

-- === Storage conditions ===
-- Decides where something may be kept and what it may travel beside. A
-- temperature range is two columns rather than free text because the one
-- question asked of it — "is this store cold enough" — is a comparison.
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS storage_temp_min_c     NUMERIC;
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS storage_temp_max_c     NUMERIC;
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS storage_humidity_max_pct NUMERIC;
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS hazard_class           TEXT;   -- e.g. "2.2", "9"
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS un_number              TEXT;   -- e.g. "UN3480"
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS storage_notes          TEXT;

-- ADD CONSTRAINT has no IF NOT EXISTS, so it needs the same guard the
-- account_type migration uses. Without it this file aborted on every container
-- start after the first — and it aborted here, at line 23, which meant the life
-- limits and supplier tables below never ran on a fresh deploy.
DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint WHERE conname = 'inventory_temp_range_sane'
  ) THEN
    ALTER TABLE inventory ADD CONSTRAINT inventory_temp_range_sane
        CHECK (storage_temp_min_c IS NULL OR storage_temp_max_c IS NULL
               OR storage_temp_min_c <= storage_temp_max_c) NOT VALID;
  END IF;
END $$;

-- === Life limits ===
-- A part retired by use rather than by date: cycles for a battery or an
-- actuator, hours for a motor, calendar months for a seal that ages on the
-- shelf. All three can apply at once and whichever is reached first retires
-- the unit, so they are separate columns rather than one "limit" with a unit.
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS life_limit_cycles NUMERIC;
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS life_limit_hours  NUMERIC;
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS life_limit_days   NUMERIC;

-- Usage accrues against the individual unit, not the catalogue entry — which
-- is the whole reason serialised parts exist.
ALTER TABLE inventory_lots ADD COLUMN IF NOT EXISTS cycles_used      NUMERIC NOT NULL DEFAULT 0;
ALTER TABLE inventory_lots ADD COLUMN IF NOT EXISTS hours_used       NUMERIC NOT NULL DEFAULT 0;
ALTER TABLE inventory_lots ADD COLUMN IF NOT EXISTS in_service_since TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_lots_in_service
    ON inventory_lots (in_service_since)
    WHERE in_service_since IS NOT NULL AND status IN ('available','reserved');

-- === Supplier part numbers ===
-- A manufacturer's P/N is not a distributor's SKU, and multi-sourcing is the
-- norm: the same part arrives under different codes, in different pack sizes,
-- at different prices and lead times.
CREATE TABLE IF NOT EXISTS supplier_parts (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    inventory_id   UUID NOT NULL REFERENCES inventory(id) ON DELETE CASCADE,

    supplier_name  TEXT NOT NULL,
    supplier_part_number TEXT,

    -- Ordering realities that decide what a reorder actually costs.
    minimum_order_qty NUMERIC,
    pack_size         NUMERIC,
    unit_cost         NUMERIC,
    currency          TEXT NOT NULL DEFAULT 'TIDAT',
    lead_time_days    NUMERIC,

    -- The source to reorder from unless told otherwise.
    preferred      BOOLEAN NOT NULL DEFAULT FALSE,
    active         BOOLEAN NOT NULL DEFAULT TRUE,
    note           TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    UNIQUE (inventory_id, supplier_name, supplier_part_number)
);

CREATE INDEX IF NOT EXISTS idx_supplier_parts_item ON supplier_parts (inventory_id);
-- Scanning a distributor's label has to find the part too.
CREATE INDEX IF NOT EXISTS idx_supplier_parts_number
    ON supplier_parts (supplier_part_number) WHERE supplier_part_number IS NOT NULL;
-- At most one preferred source per part; two would make "reorder from the
-- preferred supplier" ambiguous at the moment it matters.
CREATE UNIQUE INDEX IF NOT EXISTS idx_supplier_parts_one_preferred
    ON supplier_parts (inventory_id) WHERE preferred;

-- === Supersession and alternates ===
-- Two different relationships that look alike and behave differently.
--
--   supersedes  directional and historical: A was replaced by B. Ordering A
--               should offer B. B does not imply A.
--   alternate   symmetric: either will do. Stored once and read both ways,
--               because storing it twice invites the two rows to disagree.
CREATE TABLE IF NOT EXISTS part_relations (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    from_id     UUID NOT NULL REFERENCES inventory(id) ON DELETE CASCADE,
    to_id       UUID NOT NULL REFERENCES inventory(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,
    note        TEXT,
    effective_from TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT part_relations_kind_check CHECK (kind IN ('supersedes','alternate')),
    -- A part cannot replace itself, and the pair is recorded once.
    CONSTRAINT part_relations_distinct CHECK (from_id <> to_id),
    UNIQUE (from_id, to_id, kind)
);

CREATE INDEX IF NOT EXISTS idx_part_relations_from ON part_relations (from_id);
CREATE INDEX IF NOT EXISTS idx_part_relations_to   ON part_relations (to_id);

COMMENT ON TABLE supplier_parts IS 'Where a part can be bought, under which code, in what pack.';
COMMENT ON TABLE part_relations IS 'supersedes is directional and historical; alternate is symmetric.';
COMMENT ON COLUMN inventory_lots.cycles_used IS 'Usage accrues per unit, which is why serialised parts exist.';
