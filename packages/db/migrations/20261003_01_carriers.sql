-- Commercial carriers: UPS, USPS, FedEx, DHL — as *legs*, not as participants.
--
-- The marketplace models a shipment as one outpost bidding to carry another
-- outpost's order. That only works when both sides run Wayfarer, which is a
-- cold-start problem: a single organisation with one outpost gets no shipping
-- out of the tool at all.
--
-- A commercial carrier will never run an outpost, hold a TIDAT wallet, or sign
-- a custody receipt. So it is not modelled as a bidder. It is modelled as a
-- subcontracted **leg** of a shipment that a real participant stays
-- accountable for: the depot that bid still signed the bid, still gets paid in
-- TIDAT, and still owes the delivery. UPS is how they fulfil it.
--
-- That choice is what makes this fit the existing custody chain instead of
-- needing a parallel one. `custody_receipts` already carries `from_label` and
-- `to_label` beside the node ids, and a `verified` flag — so a handover to a
-- party that cannot sign is already expressible: `to_node_id` null,
-- `to_label` the carrier and tracking number, `verified` false. The tracking
-- number is the evidence in place of a signature, and the receipt says so
-- rather than pretending otherwise.

-- === Addresses ===
--
-- The destination model was planetary: a body id, lat/lon/alt, and a free-text
-- address. Carriers reject malformed addresses, and free text is malformed by
-- default — no country to rate against, no postcode to zone, no phone, which
-- international services require.
--
-- This does not replace the body model, it extends it. `body_id` selects which
-- half of the row is meaningful: Earth gets a postal address, anywhere else
-- gets coordinates. A tool whose whole premise is cross-domain logistics
-- should not have to choose.
CREATE TABLE IF NOT EXISTS addresses (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      UUID REFERENCES organisations(id) ON DELETE CASCADE,

    -- What an operator calls it: "Chicago depot", "Acme receiving dock".
    label       TEXT,

    -- 399 is Earth (the convention already used by orders and rate cards).
    body_id     BIGINT NOT NULL DEFAULT 399,

    -- --- Postal, meaningful on Earth ---
    name        TEXT,
    company     TEXT,
    line1       TEXT,
    line2       TEXT,
    city        TEXT,
    region      TEXT,                       -- state / province / county
    postcode    TEXT,
    country     TEXT,                       -- ISO 3166-1 alpha-2, uppercase
    phone       TEXT,                       -- required by most international services
    email       TEXT,

    -- Residential delivery costs materially more and is a separate service on
    -- every carrier. Null means "not known", which is different from
    -- commercial and must not be rated as though it were.
    residential BOOLEAN,

    -- --- Coordinates, meaningful anywhere ---
    lat         DOUBLE PRECISION,
    lon         DOUBLE PRECISION,
    alt         DOUBLE PRECISION,

    -- Carrier address validation, when it has been run. `validated_at` null
    -- means unvalidated, not invalid — an outpost with no link cannot validate
    -- and must still be able to record an address.
    validated_at    TIMESTAMPTZ,
    validation_note TEXT,

    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Enforced here rather than in a handler, because the handler is not the
    -- only thing that will ever write a row. An Earth address with no country
    -- cannot be rated or cleared through customs; an off-Earth destination
    -- with no coordinates cannot be routed to at all.
    CONSTRAINT addresses_locatable CHECK (
        (body_id = 399 AND line1 IS NOT NULL AND city IS NOT NULL AND country IS NOT NULL)
        OR (body_id <> 399 AND lat IS NOT NULL AND lon IS NOT NULL)
    ),
    CONSTRAINT addresses_country_is_iso CHECK (country IS NULL OR country ~ '^[A-Z]{2}$')
);

CREATE INDEX IF NOT EXISTS idx_addresses_org ON addresses (org_id);
-- Finding "where do we ship to in Germany" without a scan.
CREATE INDEX IF NOT EXISTS idx_addresses_country ON addresses (country) WHERE country IS NOT NULL;

-- Orders gain a structured destination. The legacy columns stay: they hold
-- live data, and an off-Earth order expressed as lat/lon is still perfectly
-- valid. New orders should set delivery_address_id.
ALTER TABLE orders ADD COLUMN IF NOT EXISTS delivery_address_id UUID
    REFERENCES addresses(id) ON DELETE SET NULL;

-- === Carrier accounts ===
--
-- Which carriers this organisation can actually buy from, and through what.
-- `provider` is how we reach them:
--
--   manual    no API. An operator rates and buys on the carrier's own site and
--             records the tracking number here. This is the one that always
--             works, including on an outpost with no link, and it is why it is
--             the default rather than an afterthought.
--   easypost  an aggregator. One integration reaches UPS, USPS, FedEx and DHL,
--             which is the right trade for a first implementation — four
--             direct integrations is four auth schemes and four sets of
--             quirks for the same feature.
CREATE TABLE IF NOT EXISTS carrier_accounts (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      UUID NOT NULL REFERENCES organisations(id) ON DELETE CASCADE,

    carrier     TEXT NOT NULL,              -- ups | usps | fedex | dhl | other
    provider    TEXT NOT NULL DEFAULT 'manual',
    nickname    TEXT,

    -- The carrier's own account number, for reference on an invoice. Not a
    -- credential.
    account_ref TEXT,

    -- Credentials are NOT stored here. The provider's API key comes from the
    -- environment (CARRIER_EASYPOST_KEY), so a database dump does not hand
    -- somebody the ability to buy labels on this account.
    active      BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT carrier_accounts_provider CHECK (provider IN ('manual', 'easypost')),
    UNIQUE (org_id, carrier, provider)
);

CREATE INDEX IF NOT EXISTS idx_carrier_accounts_org ON carrier_accounts (org_id) WHERE active;

-- === A rate quote ===
--
-- Kept rather than recomputed, because a bid composed against a quote has to
-- be able to say what it was quoted. A carrier rate moves; a bid someone
-- accepted does not.
CREATE TABLE IF NOT EXISTS carrier_rate_quotes (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      UUID NOT NULL REFERENCES organisations(id) ON DELETE CASCADE,
    order_id    UUID REFERENCES orders(id) ON DELETE CASCADE,

    carrier     TEXT NOT NULL,
    service     TEXT NOT NULL,              -- e.g. ups_ground, usps_priority
    provider    TEXT NOT NULL,
    -- The provider's own handle for this quote, needed to buy it later.
    provider_rate_id TEXT,

    from_address_id UUID REFERENCES addresses(id) ON DELETE SET NULL,
    to_address_id   UUID REFERENCES addresses(id) ON DELETE SET NULL,

    -- Carrier money is fiat, not TIDAT. The marketplace leg settles in TIDAT;
    -- a carrier invoices in a currency and the bidding outpost recovers it in
    -- the bid price. Conflating the two would make a bid uncomparable.
    amount      NUMERIC NOT NULL,
    currency    TEXT NOT NULL DEFAULT 'USD',

    transit_days     NUMERIC,
    estimated_delivery TIMESTAMPTZ,

    billable_weight_kg NUMERIC,             -- dimensional weight, if the carrier applied it
    quoted_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- A quote nobody bought, past this, is not a price any more.
    expires_at  TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_rate_quotes_order ON carrier_rate_quotes (order_id);
CREATE INDEX IF NOT EXISTS idx_rate_quotes_org ON carrier_rate_quotes (org_id, quoted_at DESC);

-- === The leg itself ===
CREATE TABLE IF NOT EXISTS carrier_shipments (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      UUID NOT NULL REFERENCES organisations(id) ON DELETE CASCADE,

    -- What this leg is part of. Both nullable: a shipment can exist before an
    -- order is raised against it, and an organisation shipping to its own
    -- customers has no marketplace order at all — which is the single-org case
    -- this whole feature exists to serve.
    order_id       UUID REFERENCES orders(id) ON DELETE SET NULL,
    fulfillment_id UUID REFERENCES fulfillments(id) ON DELETE SET NULL,

    carrier     TEXT NOT NULL,
    service     TEXT,
    provider    TEXT NOT NULL DEFAULT 'manual',
    provider_shipment_id TEXT,
    quote_id    UUID REFERENCES carrier_rate_quotes(id) ON DELETE SET NULL,

    from_address_id UUID REFERENCES addresses(id) ON DELETE SET NULL,
    to_address_id   UUID REFERENCES addresses(id) ON DELETE SET NULL,

    -- What it actually cost, which can differ from the quote once the carrier
    -- reweighs. Null until purchased.
    cost_amount   NUMERIC,
    cost_currency TEXT NOT NULL DEFAULT 'USD',

    parcel_weight_kg NUMERIC,
    parcel_length_cm NUMERIC,
    parcel_width_cm  NUMERIC,
    parcel_height_cm NUMERIC,

    -- The evidence that replaces a signature. A commercial carrier cannot
    -- sign a custody receipt, so the tracking number is what makes the
    -- handover checkable by a third party.
    tracking_number TEXT,
    tracking_url    TEXT,

    -- Labels can be large and are fetched rarely; the URL is kept and the
    -- bytes only when a provider returns them inline.
    label_format TEXT,                      -- pdf | zpl | png
    label_url    TEXT,

    status      TEXT NOT NULL DEFAULT 'draft',

    shipped_at         TIMESTAMPTZ,
    delivered_at       TIMESTAMPTZ,
    estimated_delivery TIMESTAMPTZ,
    last_tracked_at    TIMESTAMPTZ,

    -- Set when a declaration was generated for this leg, so an international
    -- shipment can be told from a domestic one without re-deriving it.
    customs_document_id UUID,

    notes       TEXT,
    created_by  UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT carrier_shipments_status CHECK (status IN
        ('draft', 'purchased', 'in_transit', 'delivered', 'exception', 'cancelled')),

    -- A purchased leg without a tracking number is not purchased. This is the
    -- one invariant the whole feature rests on: without it, the custody chain
    -- records a handover with no evidence behind it.
    CONSTRAINT carrier_shipments_tracked CHECK (
        status IN ('draft', 'cancelled') OR tracking_number IS NOT NULL
    )
);

CREATE INDEX IF NOT EXISTS idx_carrier_shipments_org ON carrier_shipments (org_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_carrier_shipments_order ON carrier_shipments (order_id)
    WHERE order_id IS NOT NULL;
-- One tracking number is one shipment. Recording it twice would make the
-- custody chain show two handovers for one parcel.
CREATE UNIQUE INDEX IF NOT EXISTS idx_carrier_shipments_tracking
    ON carrier_shipments (carrier, tracking_number) WHERE tracking_number IS NOT NULL;
-- The tracking daemon's scan: anything live, oldest check first.
CREATE INDEX IF NOT EXISTS idx_carrier_shipments_live
    ON carrier_shipments (last_tracked_at NULLS FIRST)
    WHERE status IN ('purchased', 'in_transit');

-- === Tracking events ===
--
-- Kept as a chain rather than only a current status, because "it was in
-- Memphis for four days" is the thing an operator needs and a status field
-- cannot say.
CREATE TABLE IF NOT EXISTS carrier_tracking_events (
    id          BIGSERIAL PRIMARY KEY,
    shipment_id UUID NOT NULL REFERENCES carrier_shipments(id) ON DELETE CASCADE,

    status      TEXT NOT NULL,
    detail      TEXT,
    location    TEXT,
    occurred_at TIMESTAMPTZ NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- The carrier's own id for the event, so re-polling does not duplicate it.
    -- Same idempotency problem as DTN, one layer up: polling is at-least-once.
    source_ref  TEXT
);

CREATE INDEX IF NOT EXISTS idx_tracking_events_shipment
    ON carrier_tracking_events (shipment_id, occurred_at DESC);
CREATE UNIQUE INDEX IF NOT EXISTS idx_tracking_events_dedupe
    ON carrier_tracking_events (shipment_id, source_ref) WHERE source_ref IS NOT NULL;

-- === Carrier service rules ===
--
-- What a carrier will refuse to carry. The catalogue already records
-- `hazard_class` and `un_number` — UN3480 (lithium batteries) is the single
-- most common carrier rejection, and it is already modelled — so the data to
-- check against exists. This is what to check it against.
--
-- Seeded with the rules that catch most real refusals rather than attempting
-- to be exhaustive: an incomplete table that blocks the common case beats no
-- table, provided it does not claim to be complete. `compliance_requirements`
-- stays the place destination-side paperwork lives; this is carrier-side
-- acceptance.
CREATE TABLE IF NOT EXISTS carrier_service_rules (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    carrier     TEXT NOT NULL,
    service     TEXT,                       -- null = the whole carrier
    -- What is being restricted.
    hazard_class TEXT,
    un_number    TEXT,
    -- 'forbidden' blocks; 'requires_declaration' lets it through with paperwork.
    rule        TEXT NOT NULL,
    note        TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT carrier_service_rules_rule CHECK (rule IN ('forbidden', 'requires_declaration')),
    CONSTRAINT carrier_service_rules_subject CHECK (hazard_class IS NOT NULL OR un_number IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS idx_carrier_rules_lookup
    ON carrier_service_rules (carrier, COALESCE(service, ''));

INSERT INTO carrier_service_rules (carrier, service, un_number, hazard_class, rule, note)
SELECT * FROM (VALUES
    -- Lithium batteries by air. The reason EV cells and drone packs get
    -- refused, and the reason the catalogue records UN numbers at all.
    ('ups',   'ups_next_day_air', 'UN3480', NULL, 'forbidden',
     'Standalone lithium-ion cells are not accepted on passenger air service.'),
    ('usps',  'usps_priority_air', 'UN3480', NULL, 'forbidden',
     'USPS does not accept standalone lithium-ion by air.'),
    ('usps',  NULL, NULL, '1', 'forbidden', 'USPS does not carry class 1 explosives.'),
    ('ups',   NULL, 'UN3481', NULL, 'requires_declaration',
     'Lithium cells packed with equipment need a dangerous-goods declaration.'),
    ('fedex', NULL, 'UN3480', NULL, 'requires_declaration',
     'Accepted on ground with a dangerous-goods declaration and trained shipper.'),
    ('dhl',   NULL, NULL, '7', 'forbidden', 'Radioactive material is not accepted.')
) AS v(carrier, service, un_number, hazard_class, rule, note)
WHERE NOT EXISTS (SELECT 1 FROM carrier_service_rules);

COMMENT ON TABLE addresses IS
  'body_id selects which half is meaningful: Earth gets postal, elsewhere gets coordinates.';
COMMENT ON TABLE carrier_shipments IS
  'A leg handed to a commercial carrier. The bidding participant stays accountable for it.';
COMMENT ON COLUMN carrier_shipments.tracking_number IS
  'Stands in for a signature: a commercial carrier cannot sign a custody receipt.';
COMMENT ON TABLE carrier_service_rules IS
  'Carrier-side acceptance. Destination-side paperwork lives in compliance_requirements.';
