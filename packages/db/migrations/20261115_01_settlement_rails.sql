-- Pluggable settlement rails.
--
-- Settlement used to be Solana-or-nothing: `settlement_chain` was written as
-- the literal 'solana', the verifier only knew how to ask a Solana RPC, and an
-- organisation with no wallet and no token could not settle at all. That is an
-- adoption wall for a farm, a shipping company or a remote enterprise that
-- settles by invoice.
--
-- `settlement_chain` becomes the rail discriminator. Recognised values:
--   tidat  - TIDasToken on Solana (the native rail; verified on-chain)
--   usdc   - USDC on Solana (a regulated stablecoin; same on-chain check,
--            against the USDC mint instead of TIDAT_MINT)
--   fiat   - out-of-band fiat: an invoice paid by bank transfer, or an
--            ERP-driven payment. Needs no chain and no network to record.
-- Legacy rows written before this migration carry 'solana' and are read as
-- 'tidat' in code; they are not rewritten because nothing depends on the
-- stored string except the rail resolver, which maps both.
--
-- TIDAT is unchanged and remains the native rail. These rails sit *alongside*
-- it; they do not replace it. The marketplace still settles in TIDAT by
-- default, and an operator opts a fulfilment into another rail per settlement.

-- === settlement_status gains 'attested' ===
-- On-chain rails reach 'verified' — the chain confirmed a payee was credited.
-- A fiat settlement cannot be verified that way; it is *attested* by the
-- counterparty (or an ERP webhook) confirming the invoice was paid. The two
-- are kept distinct on purpose: 'verified' is a cryptographic fact, 'attested'
-- is a human/ERP assertion. Both move the order to 'settled', because the
-- money has changed hands either way — but a reader can always tell which kind
-- of evidence stands behind a given settlement.
--
-- Full domain:
--   pending | verified | attested | rejected | unverifiable | skipped
-- (settlement_status is unconstrained TEXT, so no CHECK to alter.)

-- Who attested a fiat settlement, and when. Null for on-chain rails, which
-- carry their evidence in settlement_verified_at / the chain itself.
ALTER TABLE fulfillments ADD COLUMN IF NOT EXISTS settlement_attested_by UUID REFERENCES users(id);
ALTER TABLE fulfillments ADD COLUMN IF NOT EXISTS settlement_attested_at TIMESTAMPTZ;

-- The on-chain verifier's poll filters to on-chain rails so it never tries to
-- resolve an invoice reference as a Solana signature. Fiat 'pending' rows are
-- left for the confirmation endpoint.
CREATE INDEX IF NOT EXISTS idx_fulfillments_settlement_onchain_pending
  ON fulfillments (settlement_next_try_at)
  WHERE settlement_status = 'pending'
    AND settlement_chain IN ('tidat', 'usdc', 'solana');

COMMENT ON COLUMN fulfillments.settlement_chain IS
  'Settlement rail: tidat | usdc | fiat (legacy solana == tidat).';
