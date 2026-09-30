CREATE TABLE IF NOT EXISTS commands (
  id            UUID PRIMARY KEY,
  target_node   UUID NOT NULL,       -- who should execute
  asset_id      UUID,                -- optional: commands for a specific asset
  cmd_type      TEXT NOT NULL,       -- e.g., "telemetry.pull", "ota.update"
  payload       JSONB NOT NULL,
  status        TEXT NOT NULL DEFAULT 'queued', -- queued|sent|acked|failed
  created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_commands_target ON commands(target_node, status);

CREATE TABLE IF NOT EXISTS command_receipts (
  id            UUID PRIMARY KEY,
  command_id    UUID NOT NULL REFERENCES commands(id) ON DELETE CASCADE,
  node_id       UUID NOT NULL,       -- who reports back
  status        TEXT NOT NULL,       -- acked|failed
  info          JSONB DEFAULT '{}'::jsonb,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

