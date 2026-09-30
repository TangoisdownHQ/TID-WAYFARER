-- Chain of custody.
--
-- `fulfillments` records that a shipment happened. It does not record who
-- physically handed what to whom, when, or with whose signature — which is the
-- only evidence that survives a dispute, and the thing a customs officer, an
-- insurer or an accident board actually asks for.
--
-- Each handoff is signed by the *receiving* party's node. The receiver is the
-- one making a claim about the world ("I have it now"), so the receiver is who
-- must attest. A sender-signed receipt proves only that the sender says they
-- sent it.
--
-- This is the same primitive as the custody receipts in
-- Documentation/OfflineSettlement.md, deliberately: one mechanism discharges a
-- payment condition *and* produces the audit trail. Building it twice would
-- guarantee the two disagreed.
--
-- Every field needed to verify a receipt travels inside the receipt. A chain
-- must be checkable on an outpost that has been dark for a month and cannot
-- ask anyone whether a signature was good.

CREATE TABLE IF NOT EXISTS custody_receipts (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    -- What moved. At least one of these anchors the receipt to real cargo.
    order_id        UUID REFERENCES orders(id)           ON DELETE CASCADE,
    fulfillment_id  UUID REFERENCES fulfillments(id)     ON DELETE CASCADE,
    lot_id          UUID REFERENCES inventory_lots(id)   ON DELETE SET NULL,
    capsule_id      UUID REFERENCES capsules(id)         ON DELETE SET NULL,

    -- Position in the chain. Gaps are detectable, which is the point: a
    -- missing leg is exactly what a dispute turns on.
    seq             INT NOT NULL CHECK (seq >= 0),

    from_node_id    UUID,
    to_node_id      UUID NOT NULL,
    from_label      TEXT,
    to_label        TEXT,

    --   pickup     - taken from the origin
    --   transfer   - handed between carriers
    --   delivery   - handed to the final recipient
    --   return     - sent back
    --   inspection - examined without changing hands
    event           TEXT NOT NULL DEFAULT 'transfer',

    quantity        NUMERIC,
    unit            TEXT,
    -- Hash of what was handed over, so the receipt names the goods rather than
    -- merely the transaction.
    item_hash       TEXT,

    -- Condition on receipt. A receiver who signs for damaged goods has said so
    -- on the record, which is the whole value of signing at handoff.
    condition       TEXT NOT NULL DEFAULT 'ok',
    notes           TEXT,

    occurred_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    recorded_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    lat             DOUBLE PRECISION,
    lon             DOUBLE PRECISION,
    body_id         INT,

    -- Ed25519 over the canonical encoding (see services::custody). Nullable
    -- because a receipt from a party with no key is still better evidence than
    -- no receipt — but `verified` records honestly which kind it is.
    signature       TEXT,
    verified        BOOLEAN NOT NULL DEFAULT FALSE,

    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT custody_event_check CHECK (
        event IN ('pickup','transfer','delivery','return','inspection')
    ),
    CONSTRAINT custody_condition_check CHECK (
        condition IN ('ok','damaged','short','contaminated','unknown')
    ),
    -- A receipt that anchors to nothing is not evidence of anything.
    CONSTRAINT custody_has_subject CHECK (
        order_id IS NOT NULL OR fulfillment_id IS NOT NULL
    )
);

-- One receipt per position per order: re-submitting the same leg is an error,
-- not a second handoff.
CREATE UNIQUE INDEX IF NOT EXISTS idx_custody_order_seq
    ON custody_receipts (order_id, seq) WHERE order_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_custody_order       ON custody_receipts (order_id);
CREATE INDEX IF NOT EXISTS idx_custody_fulfillment ON custody_receipts (fulfillment_id);
CREATE INDEX IF NOT EXISTS idx_custody_lot         ON custody_receipts (lot_id) WHERE lot_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_custody_capsule     ON custody_receipts (capsule_id) WHERE capsule_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_custody_to_node     ON custody_receipts (to_node_id);

COMMENT ON TABLE custody_receipts IS
    'Signed handoffs. Each leg is attested by the receiving node; gaps in seq are detectable.';
