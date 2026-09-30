-- 20251017_create_peer_nodes.sql

CREATE EXTENSION IF NOT EXISTS "pgcrypto";

CREATE TABLE IF NOT EXISTS peer_nodes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    node_id UUID REFERENCES node_registry(node_id) ON DELETE CASCADE,
    url TEXT NOT NULL,
    trust_level TEXT DEFAULT 'trusted',
    last_seen TIMESTAMP DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_peer_nodes_url ON peer_nodes (url);
CREATE INDEX IF NOT EXISTS idx_peer_nodes_trust_level ON peer_nodes (trust_level);

