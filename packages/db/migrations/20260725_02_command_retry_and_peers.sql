-- Command delivery retry: previously one failed POST marked a command
-- 'failed' forever. Give the command engine the same store-and-forward
-- semantics as the DTN outbox: exponential backoff, capped attempts.
ALTER TABLE command_queue
  ADD COLUMN IF NOT EXISTS attempts    INTEGER     NOT NULL DEFAULT 0,
  ADD COLUMN IF NOT EXISTS next_try_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

CREATE INDEX IF NOT EXISTS idx_command_queue_status_next_try
  ON command_queue (status, next_try_at);

-- Peers are managed via /api/peers; a duplicate URL means the sync daemon
-- dials the same outpost twice per cycle. Dedupe then enforce uniqueness.
DELETE FROM peer_nodes a
USING peer_nodes b
WHERE a.url = b.url AND a.id > b.id;

CREATE UNIQUE INDEX IF NOT EXISTS uq_peer_nodes_url ON peer_nodes (url);
