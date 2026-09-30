-- === Ops events for autonomous operations ===
CREATE TABLE IF NOT EXISTS ops_events (
  id           BIGSERIAL PRIMARY KEY,
  asset_id     UUID,
  node_id      TEXT,
  kind         TEXT NOT NULL,     -- anomaly|tamper|malware|telemetry_ok
  severity     TEXT NOT NULL DEFAULT 'info',
  details      JSONB,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- === Command queue ===
CREATE TABLE IF NOT EXISTS command_queue (
  id           BIGSERIAL PRIMARY KEY,
  asset_id     UUID,
  node_id      TEXT,
  command      JSONB NOT NULL,
  status       TEXT NOT NULL DEFAULT 'queued',
  created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  sent_at      TIMESTAMPTZ,
  acked_at     TIMESTAMPTZ,
  last_error   TEXT
);

CREATE INDEX IF NOT EXISTS idx_command_queue_status_created
  ON command_queue (status, created_at);

-- Add HMAC secret to nodes for signed telemetry
ALTER TABLE node_registry
  ADD COLUMN IF NOT EXISTS hmac_secret TEXT;

-- === Delay-Tolerant Networking (DTN) mailboxes ===
CREATE TABLE IF NOT EXISTS dtn_outbox (
  id           BIGSERIAL PRIMARY KEY,
  dest_node_id UUID NOT NULL,
  endpoint     TEXT NOT NULL,
  payload      JSONB NOT NULL,
  attempts     INT NOT NULL DEFAULT 0,
  next_try_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS dtn_inbox (
  id           BIGSERIAL PRIMARY KEY,
  src_node_id  UUID,
  payload      JSONB NOT NULL,
  received_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_dtn_outbox_next_try
  ON dtn_outbox (next_try_at, attempts);

