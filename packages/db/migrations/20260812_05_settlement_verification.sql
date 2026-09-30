-- Settlement verification.
--
-- `fulfillments.settlement_tx` was free text: any non-empty string moved an
-- order to 'settled'. Every other transition in the order state machine is
-- guarded; the one where value changes hands was not.
--
-- Verification cannot be synchronous. An outpost may be hours from a usable
-- link, and a Mars-side settlement still has to be recordable during a
-- blackout. So settlement becomes a two-phase thing: the tx is *recorded*
-- immediately (status 'settling'), and a daemon promotes it to 'settled' once
-- it has confirmed the transaction — the same store-and-forward shape as DTN
-- and command delivery.

-- === Payee identity ===
-- Verifying a payment needs someone to have been paid. Nothing in the schema
-- tied a user to an on-chain address.
ALTER TABLE users ADD COLUMN IF NOT EXISTS wallet_address TEXT;

-- === Settlement state on the fulfillment ===
-- settlement_status: pending | verified | rejected | unverifiable | skipped
--   pending      - recorded, awaiting confirmation
--   verified     - confirmed on-chain against amount and payee
--   rejected     - confirmed and WRONG (bad amount, wrong payee, failed tx)
--   unverifiable - no RPC reachable/configured; recorded but never confirmed
--   skipped      - verification deliberately disabled (SETTLEMENT_VERIFY=off)
ALTER TABLE fulfillments ADD COLUMN IF NOT EXISTS settlement_status TEXT;
ALTER TABLE fulfillments ADD COLUMN IF NOT EXISTS settlement_amount NUMERIC;
ALTER TABLE fulfillments ADD COLUMN IF NOT EXISTS settlement_payee TEXT;
ALTER TABLE fulfillments ADD COLUMN IF NOT EXISTS settlement_chain TEXT;
ALTER TABLE fulfillments ADD COLUMN IF NOT EXISTS settlement_verified_at TIMESTAMPTZ;
ALTER TABLE fulfillments ADD COLUMN IF NOT EXISTS settlement_error TEXT;
ALTER TABLE fulfillments ADD COLUMN IF NOT EXISTS settlement_attempts INT NOT NULL DEFAULT 0;
ALTER TABLE fulfillments ADD COLUMN IF NOT EXISTS settlement_next_try_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

-- The verifier daemon's poll: pending settlements whose backoff has elapsed.
CREATE INDEX IF NOT EXISTS idx_fulfillments_settlement_pending
  ON fulfillments (settlement_next_try_at)
  WHERE settlement_status = 'pending';

-- A settlement tx must not be reusable across fulfillments — replaying one
-- payment to settle several orders is the obvious attack.
CREATE UNIQUE INDEX IF NOT EXISTS idx_fulfillments_settlement_tx_unique
  ON fulfillments (settlement_tx)
  WHERE settlement_tx IS NOT NULL;
