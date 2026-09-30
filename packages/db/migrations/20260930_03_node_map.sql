-- node_map: the last reported position of each outpost.
--
-- Schema drift, found the same way blockchain_feed_queue was: routes::map
-- reads and writes this table in two `sqlx::query!` calls, and the offline
-- .sqlx cache was generated against a dev database that had it — so the code
-- compiled while a freshly-migrated database had no such table and both
-- /api/map/update and /api/map/nodes failed at runtime.
--
-- Lesson worth repeating: the .sqlx cache can hide a missing migration. Only a
-- clean-database replay catches it.

CREATE TABLE IF NOT EXISTS node_map (
    node_id    UUID PRIMARY KEY REFERENCES node_registry(node_id) ON DELETE CASCADE,
    lat        DOUBLE PRECISION NOT NULL,
    lon        DOUBLE PRECISION NOT NULL,
    updated_at TIMESTAMPTZ      NOT NULL DEFAULT NOW()
);

-- list_nodes orders by recency.
CREATE INDEX IF NOT EXISTS idx_node_map_updated_at ON node_map (updated_at DESC);
