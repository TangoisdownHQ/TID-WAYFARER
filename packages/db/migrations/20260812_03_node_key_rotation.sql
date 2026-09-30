-- Node key rotation and revocation.
--
-- register_node rejects any re-registration whose public key changed, which
-- stops an attacker from hijacking a known node_id — but it also made a node's
-- key permanent. With per-node Ed25519 signatures now the fabric's primary
-- credential, "compromised outpost" had no remedy short of hand-editing this
-- table. These columns give the compromise an answer.

ALTER TABLE node_registry ADD COLUMN IF NOT EXISTS revoked BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE node_registry ADD COLUMN IF NOT EXISTS revoked_at TIMESTAMPTZ;
ALTER TABLE node_registry ADD COLUMN IF NOT EXISTS revoked_reason TEXT;
ALTER TABLE node_registry ADD COLUMN IF NOT EXISTS key_rotated_at TIMESTAMPTZ;

-- The auth guard checks this on every signed request.
CREATE INDEX IF NOT EXISTS idx_node_registry_revoked
  ON node_registry (node_id) WHERE revoked = true;

-- === Key history ===
-- A rotation is a security event: keeping the superseded key means a signature
-- captured before the rotation can still be attributed afterwards, which is
-- exactly what an incident review needs.
CREATE TABLE IF NOT EXISTS node_key_history (
  id              BIGSERIAL PRIMARY KEY,
  node_id         UUID NOT NULL,
  old_public_key  TEXT,
  new_public_key  TEXT NOT NULL,
  reason          TEXT,
  -- Which admin performed it (JWT sub), for the audit trail.
  rotated_by      TEXT,
  rotated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_node_key_history_node
  ON node_key_history (node_id, rotated_at DESC);
