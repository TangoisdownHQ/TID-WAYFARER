-- =====================================================================
-- 🛒 SupplyLink — orders, bids, fulfillments
--
-- Turns the asset/inventory tracker into a real marketplace:
-- one outpost posts an order ("I need 100kg of seed by April"), peers
-- bid on it, requester accepts, shipper delivers, parties settle in
-- TIDasToken. Pricing is in TIDasToken units (NUMERIC).
-- =====================================================================

-- ---------------------------------------------------------------------
-- orders — what someone is requesting
-- ---------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS orders (
    id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    requester_id         UUID NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    requester_outpost    UUID,                              -- which outpost issued it (node_registry)
    description          TEXT NOT NULL,
    item_kind            TEXT NOT NULL,                     -- 'asset' | 'inventory' | 'service'
    target_part_number   TEXT,                              -- preferred P/N if specific
    target_meta          JSONB DEFAULT '{}'::jsonb,         -- cultivar, model, hazmat constraints, …
    quantity             NUMERIC NOT NULL DEFAULT 1,
    unit                 TEXT NOT NULL DEFAULT 'each',      -- 'each', 'kg', 'L', 'm³', …
    needed_by            TIMESTAMPTZ,
    delivery_body_id     INT,                               -- NAIF id
    delivery_lat         DOUBLE PRECISION,
    delivery_lon         DOUBLE PRECISION,
    delivery_alt         DOUBLE PRECISION,
    delivery_address     TEXT,                              -- human label
    max_price            NUMERIC,                           -- ceiling in TIDasToken
    status               TEXT NOT NULL DEFAULT 'posted',    -- posted|bid|accepted|shipped|delivered|settled|cancelled
    accepted_bid_id      UUID,                              -- set when a bid is accepted
    created_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_orders_requester  ON orders(requester_id);
CREATE INDEX IF NOT EXISTS idx_orders_status     ON orders(status);
CREATE INDEX IF NOT EXISTS idx_orders_needed_by  ON orders(needed_by);
CREATE INDEX IF NOT EXISTS idx_orders_delivery_body ON orders(delivery_body_id);
CREATE INDEX IF NOT EXISTS idx_orders_part       ON orders(target_part_number);


-- ---------------------------------------------------------------------
-- bids — competing offers against an order
-- ---------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS bids (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    order_id        UUID NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
    bidder_id       UUID NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    bidder_outpost  UUID,                                   -- which outpost (node_registry)
    price           NUMERIC NOT NULL,                       -- in TIDasToken
    transit_days    NUMERIC,                                -- est. delivery time
    notes           TEXT,
    status          TEXT NOT NULL DEFAULT 'submitted',      -- submitted|accepted|rejected|withdrawn
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_bids_order  ON bids(order_id);
CREATE INDEX IF NOT EXISTS idx_bids_bidder ON bids(bidder_id);
CREATE INDEX IF NOT EXISTS idx_bids_status ON bids(status);


-- ---------------------------------------------------------------------
-- fulfillments — the shipping/settlement record for an accepted bid
-- ---------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS fulfillments (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    order_id        UUID NOT NULL REFERENCES orders(id) ON DELETE RESTRICT,
    bid_id          UUID NOT NULL REFERENCES bids(id) ON DELETE RESTRICT,
    shipper_id      UUID NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    asset_id        UUID REFERENCES assets(id),             -- the specific asset being shipped, if any
    package_id      UUID REFERENCES packages(id),           -- ties into existing packages table
    status          TEXT NOT NULL DEFAULT 'preparing',      -- preparing|in_transit|delivered|settled|disputed
    shipped_at      TIMESTAMPTZ,
    delivered_at    TIMESTAMPTZ,
    settled_at      TIMESTAMPTZ,
    settlement_tx   TEXT,                                   -- TIDasToken tx hash
    notes           TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (bid_id)                                          -- one fulfillment per accepted bid
);

CREATE INDEX IF NOT EXISTS idx_fulfillments_order   ON fulfillments(order_id);
CREATE INDEX IF NOT EXISTS idx_fulfillments_shipper ON fulfillments(shipper_id);
CREATE INDEX IF NOT EXISTS idx_fulfillments_status  ON fulfillments(status);


-- ---------------------------------------------------------------------
-- Bidirectional foreign key for the accepted bid pointer on orders.
-- Done after the bids table exists.
-- ---------------------------------------------------------------------
ALTER TABLE orders
  DROP CONSTRAINT IF EXISTS orders_accepted_bid_fk;

ALTER TABLE orders
  ADD CONSTRAINT orders_accepted_bid_fk
  FOREIGN KEY (accepted_bid_id) REFERENCES bids(id) ON DELETE SET NULL;
