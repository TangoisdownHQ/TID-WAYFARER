-- Reservations — a first-class claim on stock, promised but not yet shipped.
--
-- Until now "reserved" was only a status string on a lot that nothing ever
-- wrote. There was no record of *who* promised *what* to *which* order, so two
-- offline outposts could each allocate the last unit and nothing would notice
-- on reconnect (see services::replication's own warning).
--
-- This table is the substrate the conflict-resolution work needs. It is
-- deliberately a log of claim *events*, not a mutable counter: a reservation is
-- appended, then released or consumed, and the row keeps its origin. That shape
-- is what lets the future CRDT reconcile offline claims —
--   * fungible stock: the net of active reservations is a PN-Counter fold;
--   * a serialised unit: concurrent claims are an OR-Set whose duplicates are
--     *detected and surfaced for a human*, never silently merged, because two
--     outposts both legitimately claiming the last unit is a physical-world
--     conflict with no correct automatic tiebreak.
--
-- No endpoints write this yet; committing the schema is the design decision
-- that a reservation is first-class. The reserve/release flow and the merge
-- logic land with the CRDT work.

CREATE TABLE IF NOT EXISTS reservations (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    -- What is claimed. A reservation is against an item, and optionally a
    -- specific lot/serial when the claim is on an identified unit.
    inventory_id   UUID NOT NULL REFERENCES inventory(id)       ON DELETE CASCADE,
    lot_id         UUID          REFERENCES inventory_lots(id)  ON DELETE SET NULL,

    -- Who it is promised to, and which tenant made the promise.
    order_id       UUID          REFERENCES orders(id)          ON DELETE SET NULL,
    org_id         UUID          REFERENCES organisations(id)   ON DELETE SET NULL,

    -- Which outpost authored the claim. The CRDT's replica identity: a claim
    -- carries where it was made so concurrent claims are attributable, not
    -- anonymous. Scoped like dtn_seen's (src_node_id, msg_id) — a sender owns
    -- its own ids so no peer can deny another a slot.
    node_id        UUID NOT NULL,
    -- Monotonic per (node_id, inventory_id) claim series. A gap or a duplicate
    -- is detectable the moment two outposts' logs meet — the same mechanism
    -- custody receipts use to prove a chain is whole.
    nonce          BIGINT NOT NULL,

    quantity       NUMERIC NOT NULL CHECK (quantity > 0),

    -- active    - the claim stands, stock is spoken for
    -- released  - the claim was given up (order cancelled, bid lost)
    -- consumed  - the goods shipped against it; it becomes a movement
    -- expired   - the hold lapsed without being consumed
    status         TEXT NOT NULL DEFAULT 'active',

    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at     TIMESTAMPTZ,
    released_at    TIMESTAMPTZ,

    CONSTRAINT reservation_status_check CHECK (
        status IN ('active', 'released', 'consumed', 'expired')
    )
);

-- The sender-owned claim id, enforced. A replayed or reused slot — the offline
-- double-claim signature — collides here, exactly as the voucher nonce does.
CREATE UNIQUE INDEX IF NOT EXISTS idx_reservations_claim
  ON reservations (node_id, inventory_id, nonce);

-- The active net per item, for the reconciliation fold and for "how much is
-- really free".
CREATE INDEX IF NOT EXISTS idx_reservations_active
  ON reservations (inventory_id) WHERE status = 'active';

-- A serialised unit can be claimed by at most one active reservation. Two
-- concurrent active claims on one serial are the conflict the CRDT must surface
-- — within a single database this unique partial index makes it impossible;
-- across a partition it becomes the detectable duplicate on merge.
CREATE UNIQUE INDEX IF NOT EXISTS idx_reservations_active_serial
  ON reservations (lot_id) WHERE status = 'active' AND lot_id IS NOT NULL;
