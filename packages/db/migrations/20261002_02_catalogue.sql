-- Catalogue fields: what a resource *is*, as distinct from how much is here.
--
-- `inventory` described a quantity with a name. That is enough for bulk
-- consumables and useless for a part: a drone motor, a server PSU, an EV cell
-- module and an aircraft actuator are all identified by a manufacturer part
-- number, and nothing in the schema could hold one. The practical consequence
-- was that two rows called "pump" could be entirely different pumps and the
-- system had no way to know.
--
-- Specification is JSONB on purpose. Voltage and cycle count matter for a
-- battery, torque and duty cycle for an actuator, cultivar and germination
-- rate for seed. Any fixed column set would be wrong for most of them within a
-- week, and adding a column per customer is how a schema dies.

ALTER TABLE inventory ADD COLUMN IF NOT EXISTS part_number    TEXT;
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS manufacturer   TEXT;
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS model          TEXT;
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS revision       TEXT;

-- Scannable identity. Kept separate from part_number: a GTIN is assigned by a
-- numbering authority and a part number by the manufacturer, and conflating
-- them makes both unreliable.
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS barcode        TEXT;

ALTER TABLE inventory ADD COLUMN IF NOT EXISTS specification  JSONB NOT NULL DEFAULT '{}'::jsonb;
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS datasheet_url  TEXT;

-- Whether each unit needs its own identity. A serialised part gets one lot row
-- per unit; a bulk consumable gets one row per batch. Recording the intent
-- lets the UI ask for a serial when one is required instead of leaving an
-- operator to guess, and lets a later check notice a serialised part with no
-- serials recorded.
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS serialised     BOOLEAN NOT NULL DEFAULT FALSE;

-- Default shelf life, used to propose an expiry when a new lot is received so
-- the date is not left blank by whoever is in a hurry at the loading dock.
ALTER TABLE inventory ADD COLUMN IF NOT EXISTS shelf_life_days NUMERIC;

-- One part number per organisation. The same P/N appearing twice means two
-- records of one part, which defeats every count and every recall that depends
-- on it. Partial so bulk items without a number are unaffected.
CREATE UNIQUE INDEX IF NOT EXISTS idx_inventory_part_number_per_org
    ON inventory (org_id, part_number) WHERE part_number IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_inventory_barcode
    ON inventory (barcode) WHERE barcode IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_inventory_manufacturer
    ON inventory (manufacturer) WHERE manufacturer IS NOT NULL;

COMMENT ON COLUMN inventory.specification IS
    'Free-form attributes that vary by part type: voltage, torque, cultivar, cycles.';
COMMENT ON COLUMN inventory.serialised IS
    'True when each unit needs its own serial, so lots are created one per unit.';
