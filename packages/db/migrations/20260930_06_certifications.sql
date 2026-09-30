-- Certifications and compliance holds.
--
-- Space and defence logistics live on this: export control (ITAR/EAR), hazmat
-- classification, flight certification, calibration expiry, customs status. A
-- part that is out of cert must not ship, and "someone will remember" is not a
-- control — it is the absence of one.
--
-- Design decision worth stating: a hold BLOCKS by default. The alternative —
-- record the problem and let the shipment proceed — is how compliance systems
-- become decoration. An override exists, but it must be deliberate, attributed
-- and reasoned, because the times you genuinely must ship anyway are real and
-- pretending otherwise just teaches people to work around the system.

-- === What a thing is certified for ===
CREATE TABLE IF NOT EXISTS certifications (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    -- What this certificate covers. Exactly one of these is set.
    inventory_id  UUID REFERENCES inventory(id)      ON DELETE CASCADE,
    lot_id        UUID REFERENCES inventory_lots(id) ON DELETE CASCADE,
    asset_id      UUID REFERENCES assets(id)         ON DELETE CASCADE,

    --   export      - ITAR / EAR / dual-use licence
    --   hazmat      - UN class, packing group
    --   flight      - qualified for flight
    --   calibration - instrument within calibration
    --   customs     - cleared for a destination
    --   quality     - inspection or acceptance
    kind          TEXT NOT NULL,

    -- The authority and the document. `identifier` is the licence or cert
    -- number an auditor will ask for.
    authority     TEXT,
    identifier    TEXT,

    -- Scope. NULL destination means "anywhere"; a NAIF id or country code
    -- restricts it, which is what export control actually looks like.
    destination_body_id INT,
    destination_region  TEXT,

    issued_at     TIMESTAMPTZ,
    expires_at    TIMESTAMPTZ,

    --   valid | expired | suspended | revoked
    status        TEXT NOT NULL DEFAULT 'valid',

    details       JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT certifications_kind_check CHECK (
        kind IN ('export','hazmat','flight','calibration','customs','quality')
    ),
    CONSTRAINT certifications_status_check CHECK (
        status IN ('valid','expired','suspended','revoked')
    ),
    -- Exactly one subject. A certificate covering "everything" is not a
    -- certificate.
    CONSTRAINT certifications_one_subject CHECK (
        (inventory_id IS NOT NULL)::int
      + (lot_id       IS NOT NULL)::int
      + (asset_id     IS NOT NULL)::int = 1
    )
);

CREATE INDEX IF NOT EXISTS idx_certs_inventory ON certifications (inventory_id) WHERE inventory_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_certs_lot       ON certifications (lot_id)       WHERE lot_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_certs_asset     ON certifications (asset_id)     WHERE asset_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_certs_expiring  ON certifications (expires_at)
    WHERE expires_at IS NOT NULL AND status = 'valid';

-- === Requirements: what a route demands ===
-- A destination can require certain kinds of certificate. This is what turns a
-- pile of documents into a check: without it, nobody knows which certificate
-- was supposed to exist.
CREATE TABLE IF NOT EXISTS compliance_requirements (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- NULL body means "every destination".
    destination_body_id INT,
    -- NULL category means "everything shipped there".
    item_category       TEXT,
    required_kind       TEXT NOT NULL,
    note                TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT compliance_req_kind_check CHECK (
        required_kind IN ('export','hazmat','flight','calibration','customs','quality')
    ),
    UNIQUE (destination_body_id, item_category, required_kind)
);

-- === Holds: a shipment stopped, and why ===
CREATE TABLE IF NOT EXISTS compliance_holds (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    order_id      UUID REFERENCES orders(id)   ON DELETE CASCADE,
    capsule_id    UUID REFERENCES capsules(id) ON DELETE CASCADE,

    reason        TEXT NOT NULL,
    -- Which requirement failed, for the audit trail.
    failed_kind   TEXT,
    details       JSONB NOT NULL DEFAULT '{}'::jsonb,

    --   active   - blocking
    --   cleared  - the underlying problem was fixed
    --   overridden - shipped anyway, deliberately
    status        TEXT NOT NULL DEFAULT 'active',

    -- Overrides are attributed. An unattributable override is indistinguishable
    -- from the check not existing.
    overridden_by     UUID REFERENCES users(id) ON DELETE SET NULL,
    override_reason   TEXT,
    resolved_at       TIMESTAMPTZ,

    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT compliance_holds_status_check CHECK (
        status IN ('active','cleared','overridden')
    ),
    CONSTRAINT compliance_holds_subject CHECK (
        order_id IS NOT NULL OR capsule_id IS NOT NULL
    ),
    -- An override without a reason and an author is not an override.
    CONSTRAINT compliance_holds_override_attributed CHECK (
        status <> 'overridden'
        OR (overridden_by IS NOT NULL AND override_reason IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_holds_order   ON compliance_holds (order_id)   WHERE order_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_holds_capsule ON compliance_holds (capsule_id) WHERE capsule_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_holds_active  ON compliance_holds (status) WHERE status = 'active';

COMMENT ON TABLE certifications IS 'What an item, lot or asset is certified for, and until when.';
COMMENT ON TABLE compliance_requirements IS 'Which certificate kinds a destination demands.';
COMMENT ON TABLE compliance_holds IS 'A shipment stopped on compliance grounds. Overrides are attributed.';
