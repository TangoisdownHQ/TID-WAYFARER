-- Extend fleet assets with dynamic fields
ALTER TABLE fleet_assets
  ADD COLUMN IF NOT EXISTS dynamic_location TEXT,
  ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ DEFAULT NOW();

-- Extend fleet telemetry (must happen BEFORE ops_brain!)
ALTER TABLE fleet_telemetry
  ADD COLUMN IF NOT EXISTS processed BOOLEAN DEFAULT FALSE,
  ADD COLUMN IF NOT EXISTS received_at TIMESTAMPTZ DEFAULT NOW();

-- Helpful index for telemetry processing queue
CREATE INDEX IF NOT EXISTS idx_fleet_telemetry_processed_received
  ON fleet_telemetry (processed, received_at);

