-- Blockchain feed plumbing: reconciles the schema with what `/api/bc`
-- (routes/blockchain.rs) and the blockchain_feeder daemon have always
-- written. Both wrote `threat` and `details` on blockchain_alerts and read
-- from blockchain_feed_queue, but no migration ever created them — the code
-- compiled only because the .sqlx offline cache was generated against a dev
-- database that had drifted ahead of the migration set.

-- ── blockchain_alerts: columns the writers already bind ──
-- `threat` is the normalized threat label; blockchain.rs currently reuses it
-- as alert_type. Existing rows predate the column, so it backfills from
-- alert_type rather than taking a bare NOT NULL that would fail on them.
ALTER TABLE blockchain_alerts ADD COLUMN IF NOT EXISTS threat TEXT;
UPDATE blockchain_alerts SET threat = alert_type WHERE threat IS NULL;
ALTER TABLE blockchain_alerts ALTER COLUMN threat SET NOT NULL;

-- `details` is TEXT, not JSONB: both writers stringify the JSON and the
-- feeder's insert does COALESCE($9, '{}'::text).
ALTER TABLE blockchain_alerts ADD COLUMN IF NOT EXISTS details TEXT;

-- ── blockchain_feed_queue: the feeder's input side ──
-- Raw chain events land here (from an external indexer/relay); the feeder
-- normalizes each into blockchain_alerts and flips `processed`.
CREATE TABLE IF NOT EXISTS blockchain_feed_queue (
  id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  chain       TEXT NOT NULL,
  payload     JSONB NOT NULL,
  processed   BOOLEAN NOT NULL DEFAULT false,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- The feeder polls exactly this predicate/order every cycle.
CREATE INDEX IF NOT EXISTS idx_blockchain_feed_queue_unprocessed
ON blockchain_feed_queue (created_at ASC)
WHERE processed = false;
