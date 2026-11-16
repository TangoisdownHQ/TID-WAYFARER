-- Damage evidence.
--
-- When goods arrive damaged in an isolated place, "it came broken" is worth
-- nothing in a dispute a week later on another outpost. A damage report is a
-- party's SIGNED statement of the condition on arrival, binding the hash of the
-- supporting evidence (photos, a sensor log) rather than the blob — authored,
-- timestamped and tamper-evident, and able to travel over a blackout the way a
-- custody receipt does (see services::damage).
--
-- The point of the feature is the suspension it triggers: while a report is
-- 'open', settlement on that fulfilment is refused and an offline voucher
-- cannot be discharged. Money does not move onto contested goods. This is the
-- delivery-side analogue of a compliance_hold, which already blocks a shipment
-- the same way before it leaves.

CREATE TABLE IF NOT EXISTS damage_reports (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    order_id          UUID NOT NULL REFERENCES orders(id)       ON DELETE CASCADE,
    fulfillment_id    UUID NOT NULL REFERENCES fulfillments(id) ON DELETE CASCADE,
    lot_id            UUID          REFERENCES inventory_lots(id) ON DELETE SET NULL,

    -- Who attested, and from which outpost. The outpost signs; the user is the
    -- party on whose behalf (taken from the verified principal, never the body).
    reporter_node_id  UUID NOT NULL,
    reporter_user_id  UUID NOT NULL REFERENCES users(id) ON DELETE RESTRICT,

    condition         TEXT NOT NULL,
    severity          TEXT NOT NULL,
    quantity_affected NUMERIC,
    description       TEXT NOT NULL DEFAULT '',

    -- The cryptographic anchor: a hash of the evidence file, computed client-
    -- side. The blob is NOT stored here; evidence_uri is an optional pointer.
    evidence_hash     TEXT NOT NULL,
    evidence_uri      TEXT,

    occurred_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    reporter_sig      TEXT NOT NULL,          -- Ed25519 over the canonical report

    -- open      - stands; suspends settlement/discharge on the fulfilment
    -- resolved  - the parties settled the dispute (optionally a partial credit)
    -- withdrawn - the reporter retracted it
    status            TEXT NOT NULL DEFAULT 'open',
    resolution        TEXT,
    resolved_by       UUID REFERENCES users(id),
    resolved_at       TIMESTAMPTZ,

    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT damage_condition_check CHECK (
        condition IN ('damaged', 'short', 'contaminated', 'spoiled')
    ),
    CONSTRAINT damage_severity_check CHECK (
        severity IN ('minor', 'major', 'total')
    ),
    CONSTRAINT damage_status_check CHECK (
        status IN ('open', 'resolved', 'withdrawn')
    )
);

-- The suspension lookup: is there an open report on this fulfilment? The
-- settlement and voucher-discharge paths check this.
CREATE INDEX IF NOT EXISTS idx_damage_open
  ON damage_reports (fulfillment_id)
  WHERE status = 'open';

CREATE INDEX IF NOT EXISTS idx_damage_order ON damage_reports (order_id);
