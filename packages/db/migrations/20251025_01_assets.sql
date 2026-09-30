-- assets + asset_events
CREATE TABLE IF NOT EXISTS assets (
  id           UUID PRIMARY KEY,
  name         TEXT NOT NULL,
  kind         TEXT NOT NULL,                -- satellite|ev|drone|gateway|sensor|other
  owner_id     UUID,                          -- optional linkage to users
  meta         JSONB DEFAULT '{}'::jsonb,     -- free-form attributes (VIN, orbit, etc.)
  created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_assets_kind ON assets(kind);

CREATE TABLE IF NOT EXISTS asset_events (
  id           UUID PRIMARY KEY,
  asset_id     UUID NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
  node_id      UUID,                          -- the outpost that sent this
  event_type   TEXT NOT NULL,                 -- status|location|telemetry|custody|fault
  payload      JSONB NOT NULL,                -- detail blob (lat/lon, orbit params, etc.)
  created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_asset_events_asset_id ON asset_events(asset_id);
CREATE INDEX IF NOT EXISTS idx_asset_events_created ON asset_events(created_at DESC);

