-- ✅ Fleet Assets Table (satellites, drones, EV fleets, servers, honeypots)
CREATE TABLE IF NOT EXISTS fleet_assets (
    id UUID PRIMARY KEY,
    node_id TEXT NOT NULL, -- home node / base station ID
    current_node TEXT,     -- ✅ where the asset currently checks in (mesh/dtn)
    asset_type TEXT NOT NULL, -- "drone","satellite","ev","server","honeypot"
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'idle', -- ✅ later migrations expect status
    last_seen TIMESTAMP NOT NULL DEFAULT NOW()
);

-- ✅ Universal Fleet Telemetry Table
CREATE TABLE IF NOT EXISTS fleet_telemetry (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    asset_id UUID REFERENCES fleet_assets(id) ON DELETE CASCADE,
    node_id TEXT NOT NULL,
    timestamp TIMESTAMP DEFAULT NOW(),

    lat DOUBLE PRECISION,
    lon DOUBLE PRECISION,
    alt DOUBLE PRECISION,
    
    speed DOUBLE PRECISION,
    heading DOUBLE PRECISION,
    inclination DOUBLE PRECISION,
    apogee DOUBLE PRECISION,
    perigee DOUBLE PRECISION,

    battery DOUBLE PRECISION,
    temperature DOUBLE PRECISION,

    signal_db DOUBLE PRECISION,
    latency_ms DOUBLE PRECISION,
    packet_loss DOUBLE PRECISION,

    anomaly_score DOUBLE PRECISION,
    tamper BOOLEAN,
    malware_flag BOOLEAN
);

CREATE INDEX IF NOT EXISTS idx_fleet_asset_latest
ON fleet_telemetry (asset_id, timestamp DESC);

-- ✅ Index for mesh routing lookups
CREATE INDEX IF NOT EXISTS idx_fleet_current_node
ON fleet_assets (current_node);

