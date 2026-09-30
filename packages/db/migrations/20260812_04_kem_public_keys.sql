-- ML-KEM public keys in the node registry.
--
-- Registration already exchanged Ed25519 keys, which authenticate a peer.
-- Encrypting to a peer needs its KEM public key too, so the same handshake
-- now carries both: one round trip establishes who a node is *and* how to
-- seal traffic for it.
--
-- Nullable on purpose. A peer running an older build has no KEM key, and DTN
-- falls back to sending that peer plaintext rather than refusing to talk to
-- it — the fabric has to keep working while it is being upgraded.

ALTER TABLE node_registry ADD COLUMN IF NOT EXISTS kem_public_key TEXT;

-- Encryption state per stored DTN message, so an operator can tell at a
-- glance whether anything is still crossing links in the clear.
ALTER TABLE dtn_outbox ADD COLUMN IF NOT EXISTS encrypted BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE dtn_inbox  ADD COLUMN IF NOT EXISTS encrypted BOOLEAN NOT NULL DEFAULT false;
