-- Node Registry: Master list of all Outposts known to this node

CREATE TABLE IF NOT EXISTS node_registry (
    node_id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    api_endpoint TEXT UNIQUE NOT NULL,
    public_key TEXT NOT NULL,
    location TEXT,
    last_seen TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
    status TEXT DEFAULT 'active'
);

CREATE INDEX IF NOT EXISTS idx_node_registry_name ON node_registry (name);
CREATE INDEX IF NOT EXISTS idx_node_registry_status ON node_registry (status);
CREATE INDEX IF NOT EXISTS idx_node_registry_last_seen ON node_registry (last_seen);

