-- Signed DTN envelopes: senders sign payloads with their Ed25519 node key;
-- receivers verify against the sender's registered public key and record the
-- outcome. Unverifiable messages are still stored (store-and-forward first),
-- but consumers can filter on verified.
ALTER TABLE dtn_inbox
  ADD COLUMN IF NOT EXISTS signature TEXT,
  ADD COLUMN IF NOT EXISTS verified  BOOLEAN NOT NULL DEFAULT false;
