-- Offline settlement: Conditional Transfer Vouchers.
--
-- A CTV is a signed promise to pay that closes a trade locally and settles
-- later — the same store-and-forward shape as DTN, the command queue and the
-- settlement verifier. The chain (when there is one) is an eventual anchor,
-- not the transaction medium. See Documentation/OfflineSettlement.md.
--
-- This migration is steps 1–2 of that design: the schema plus IOU-mode
-- commit/present. Escrow backing (the Anchor program + escrow_accounts sync)
-- is step 4 and lands later; the escrow_accounts table is created now so the
-- voucher row can reference an escrow series even while that is unused.

CREATE TABLE IF NOT EXISTS transfer_vouchers (
    id              UUID PRIMARY KEY,
    order_id        UUID NOT NULL REFERENCES orders(id)       ON DELETE RESTRICT,
    fulfillment_id  UUID          REFERENCES fulfillments(id) ON DELETE SET NULL,

    -- The payer is a node AND a user: which outpost signed, and on whose
    -- behalf. Taken from the verified principal, never from the request body.
    payer_node_id   UUID NOT NULL,
    payer_user_id   UUID NOT NULL REFERENCES users(id)        ON DELETE RESTRICT,

    payee_wallet    TEXT   NOT NULL,          -- where payment must land
    amount          NUMERIC NOT NULL,
    mint            TEXT   NOT NULL,          -- which token counts as payment

    -- The on-chain escrow this voucher draws against. NULL is an IOU: a debt
    -- settled on reconnect, worth exactly the counterparty.
    escrow_ref      TEXT,

    -- Ordered list of Condition (see services::voucher): delivery_receipt,
    -- not_after:<unix>. Covered by the signature.
    conditions      JSONB  NOT NULL DEFAULT '[]'::jsonb,

    -- Monotonic per (payer, escrow series). A gap or duplicate is detectable
    -- the moment two vouchers meet, which makes over-commitment provable after
    -- the fact even though it cannot be prevented offline.
    nonce           BIGINT NOT NULL,

    anchor_by       TIMESTAMPTZ NOT NULL,     -- the settlement horizon
    payer_sig       TEXT   NOT NULL,          -- Ed25519 over the canonical CTV

    -- committed  - signed and exchanged; the trade is locally final (a resting
    --              state, potentially for weeks — NOT a pending failure)
    -- discharged - conditions met; the payee may claim
    -- anchored   - claimed and confirmed on chain
    -- disputed   - conditions contested, or the anchor contradicted the voucher
    -- expired    - anchor_by passed without anchoring
    state           TEXT   NOT NULL DEFAULT 'committed',
    anchor_tx       TEXT,

    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT transfer_voucher_state_check CHECK (
        state IN ('committed','discharged','anchored','disputed','expired')
    )
);

-- The monotonic-nonce guarantee, enforced. NULL escrow_ref groups as one IOU
-- series per payer (COALESCE to '' so NULLs collide rather than slip past a
-- UNIQUE that ignores them). A replayed or reused slot hits this.
CREATE UNIQUE INDEX IF NOT EXISTS idx_transfer_vouchers_nonce
  ON transfer_vouchers (payer_node_id, COALESCE(escrow_ref, ''), nonce);

-- The anchoring daemon's poll (step 4): vouchers whose conditions are met.
CREATE INDEX IF NOT EXISTS idx_transfer_vouchers_discharged
  ON transfer_vouchers (anchor_by) WHERE state = 'discharged';

-- Horizon expiry: still-open vouchers ordered by when they must anchor.
CREATE INDEX IF NOT EXISTS idx_transfer_vouchers_open
  ON transfer_vouchers (anchor_by) WHERE state IN ('committed', 'discharged');

CREATE INDEX IF NOT EXISTS idx_transfer_vouchers_order ON transfer_vouchers (order_id);

-- On-chain escrow references and their last-known balance. Populated by the
-- escrow sync in step 4; present now so a voucher can name its series.
CREATE TABLE IF NOT EXISTS escrow_accounts (
    escrow_ref      TEXT PRIMARY KEY,
    owner_node_id   UUID NOT NULL,
    mint            TEXT NOT NULL,
    last_balance    NUMERIC,
    last_synced_at  TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Tie a fulfilment to the voucher that settles it.
ALTER TABLE fulfillments ADD COLUMN IF NOT EXISTS ctv_id UUID REFERENCES transfer_vouchers(id);
