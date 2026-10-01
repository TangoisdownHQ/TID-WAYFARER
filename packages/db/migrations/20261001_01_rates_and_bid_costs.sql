-- Rate cards and bid cost structure.
--
-- A bid carried one number. Real freight decisions are never one number: the
-- cheapest bid is routinely the wrong one, because it is slower, or it is
-- quoted by a carrier that has damaged the last three consignments, or its
-- price is per-shipment and collapses once you account for what it actually
-- weighs.
--
-- Two additions:
--
--   rate_cards  what a carrier charges on a route, so a price can be quoted
--               before anyone bids
--   bid costing a breakdown on each bid, so "4,000 TIDAT" can be compared
--               against "3,800 plus a hazmat surcharge"
--
-- Deliberately NOT added: a composite "score" column. The weighting between
-- cost, speed and reliability belongs to the operator placing the order, and
-- a stored score would bake one answer in while hiding the weights that
-- produced it.

CREATE TABLE IF NOT EXISTS rate_cards (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    carrier_id      UUID REFERENCES users(id) ON DELETE CASCADE,
    carrier_outpost UUID,
    name            TEXT NOT NULL,

    -- Route. NULL on either end means "from/to anywhere", which is how a
    -- general-purpose carrier is modelled.
    origin_body_id      INT,
    destination_body_id INT,

    -- The two dimensions a hull actually sells. A carrier may price on one,
    -- the other, or both — the quote charges whichever yields more, which is
    -- how dimensional weight works in real freight.
    price_per_kg    NUMERIC CHECK (price_per_kg   >= 0),
    price_per_m3    NUMERIC CHECK (price_per_m3   >= 0),
    minimum_charge  NUMERIC CHECK (minimum_charge >= 0),
    -- Flat fee applied once per consignment, on top of the dimensional rate.
    handling_fee    NUMERIC CHECK (handling_fee   >= 0),

    transit_days    NUMERIC CHECK (transit_days >= 0),

    -- Priced in TIDasToken like everything else in the marketplace.
    currency        TEXT NOT NULL DEFAULT 'TIDAT',

    valid_from      TIMESTAMPTZ,
    valid_to        TIMESTAMPTZ,
    active          BOOLEAN NOT NULL DEFAULT TRUE,

    notes           TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- A card that prices nothing cannot quote anything.
    CONSTRAINT rate_cards_prices_something CHECK (
        price_per_kg IS NOT NULL OR price_per_m3 IS NOT NULL OR minimum_charge IS NOT NULL
    )
);

CREATE INDEX IF NOT EXISTS idx_rate_cards_carrier ON rate_cards (carrier_id);
CREATE INDEX IF NOT EXISTS idx_rate_cards_route
    ON rate_cards (origin_body_id, destination_body_id) WHERE active;

-- === Bid cost structure ===
-- The breakdown behind a bid's price. Stored rather than recomputed because a
-- quote is an offer made at a moment: the rate card may change afterwards, and
-- what was offered is what binds.
ALTER TABLE bids ADD COLUMN IF NOT EXISTS rate_card_id   UUID REFERENCES rate_cards(id) ON DELETE SET NULL;
ALTER TABLE bids ADD COLUMN IF NOT EXISTS mass_charge    NUMERIC;
ALTER TABLE bids ADD COLUMN IF NOT EXISTS volume_charge  NUMERIC;
ALTER TABLE bids ADD COLUMN IF NOT EXISTS handling_fee   NUMERIC;
-- Named extras: hazmat, cold chain, expedite, insurance. JSONB because the set
-- varies per carrier and a fixed column list would be wrong immediately.
ALTER TABLE bids ADD COLUMN IF NOT EXISTS surcharges     JSONB NOT NULL DEFAULT '{}'::jsonb;

COMMENT ON TABLE rate_cards IS
    'What a carrier charges on a route. Quotes charge the greater of mass and volume.';
