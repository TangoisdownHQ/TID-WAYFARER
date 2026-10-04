-- Key custody: surviving a rotation, and knowing whether you could survive a
-- volume loss.
--
-- Two problems, both operational rather than cryptographic.
--
-- === 1. Rotation needed an administrator at every peer ===
--
-- `POST /api/nodes/:id/rotate-key` requires an admin JWT *on the peer*. On a
-- fabric of one organisation that is fine. On a federated fabric it means
-- rotating your own key requires administrative access to other companies'
-- outposts, which you do not have — so in practice nobody rotates.
--
-- Succession fixes it: the outgoing key signs a statement naming its successor,
-- and a peer accepts that without an administrator because the predecessor's
-- authority transfers. No new trust is being created; existing trust is being
-- handed on.
--
-- But a rotation across many peers cannot be atomic, and the order matters in a
-- way that is easy to get backwards:
--
--   Tell every peer FIRST, install the new key LAST.
--
-- A peer that has accepted the succession expects the new key while the
-- outpost is still signing with the old one — so peers accept the *predecessor*
-- for a grace window. The opposite order has no fix at all: a peer that has
-- not been told cannot verify a key it has never seen, and no grace window can
-- invent one.
--
-- === 2. Nobody knows when the keys were last backed up ===
--
-- Recorded here rather than left to a runbook, because a backup nobody can see
-- the age of is a backup nobody notices has stopped.

-- Whether a superseded key may still be accepted during the rollover window.
--
-- Normally yes — that is what makes a multi-peer rotation safe. But a key
-- being rotated *because it was compromised* must stop working immediately:
-- a grace window on a stolen key is a grace window for the thief. So the
-- reason for the rotation decides, and it is recorded per rotation rather
-- than configured globally.
ALTER TABLE node_key_history ADD COLUMN IF NOT EXISTS grace BOOLEAN NOT NULL DEFAULT TRUE;

-- How the rotation was authorised, so an auditor can tell a routine rollover
-- from an administrator overriding one.
--   succession  the outgoing key signed for its successor
--   admin       an administrator on this outpost rotated it
ALTER TABLE node_key_history ADD COLUMN IF NOT EXISTS method TEXT NOT NULL DEFAULT 'admin';

-- The grace lookup runs on any request whose signature fails against the
-- current key, so it has to be indexed or a wrong signature becomes a scan.
CREATE INDEX IF NOT EXISTS idx_node_key_history_grace
  ON node_key_history (node_id, rotated_at DESC) WHERE grace;

-- === Backups ===
--
-- One row per successful backup. Deliberately not a log of attempts: the
-- question an operator needs answered is "is there a recent one", and a table
-- full of failures makes that harder to read, not easier. Failures belong in
-- the log, where they are already loud.
CREATE TABLE IF NOT EXISTS backups (
    id          BIGSERIAL PRIMARY KEY,

    -- Which outpost this is a backup *of*. A restored outpost keeps its node
    -- id, so this stays meaningful across a recovery.
    node_id     UUID,
    outpost_name TEXT,

    -- 'database' | 'keys' | 'full'
    kind        TEXT NOT NULL,

    -- Where it went. A path or a bucket URI — recorded so a restore does not
    -- begin with guessing.
    location    TEXT,
    size_bytes  BIGINT,

    -- SHA-256 of the archive, so a restore can prove it got the bytes that
    -- were taken rather than a truncated copy.
    checksum    TEXT,

    -- Whether the key bundle was included. A database backup without the keys
    -- restores an outpost that cannot authenticate to any peer and cannot open
    -- its own queued DTN traffic, which is a distinction worth being able to
    -- see at a glance.
    includes_keys BOOLEAN NOT NULL DEFAULT FALSE,

    taken_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    note        TEXT,

    CONSTRAINT backups_kind CHECK (kind IN ('database', 'keys', 'full'))
);

CREATE INDEX IF NOT EXISTS idx_backups_recent ON backups (taken_at DESC);

COMMENT ON TABLE backups IS
  'Successful backups only. The question is "is there a recent one", and failures belong in the log.';
COMMENT ON COLUMN node_key_history.grace IS
  'FALSE when the key was compromised: a grace window on a stolen key is a grace window for the thief.';
COMMENT ON COLUMN backups.includes_keys IS
  'A database restored without its keys cannot authenticate to peers or open its queued DTN traffic.';
