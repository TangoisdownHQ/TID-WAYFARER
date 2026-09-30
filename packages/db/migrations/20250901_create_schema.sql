-- ====================================================
-- 🛰️ TIDasONE Unified Base Schema
-- Sovereign Cyber-Physical + Blockchain + Fleet Graph
-- ====================================================

-- ==========================
-- ENUMS
-- ==========================

-- Asset classification — core primitive
CREATE TYPE asset_kind AS ENUM (
    'node',         -- compute node, core HQ brain or outpost
    'wallet',       -- crypto wallet as asset identity
    'contract',     -- smart contract endpoint
    'satellite',    -- orbital or high-alt asset
    'fleet',        -- EV / UAV / robot / rover / ground unit
    'ops'           -- control / ops systems / command interface
);

-- ==========================
-- USERS
-- ==========================

CREATE TABLE IF NOT EXISTS users (
    id UUID PRIMARY KEY,
    username TEXT NOT NULL,
    email TEXT NOT NULL UNIQUE,
    role TEXT NOT NULL DEFAULT 'user',
    nft_token_id TEXT,
    nft_image_url TEXT,
    identity_hash TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);

-- ==========================
-- INVENTORY
-- ==========================

CREATE TABLE IF NOT EXISTS inventory (
    id UUID PRIMARY KEY,
    owner_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    description TEXT,
    quantity INT NOT NULL,
    location TEXT,
    token_id TEXT,
    token_image_url TEXT,
    category TEXT NOT NULL DEFAULT 'general',
    unit TEXT NOT NULL DEFAULT 'units',
    threshold INT NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);

-- ==========================
-- PACKAGES
-- ==========================

CREATE TABLE IF NOT EXISTS packages (
    id UUID PRIMARY KEY,
    owner_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    inventory_item_id UUID REFERENCES inventory(id) ON DELETE SET NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    location TEXT,
    nft_token TEXT,
    nft_image_url TEXT,
    description TEXT,
    eta TIMESTAMP,
    completed_at TIMESTAMP,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);

-- ==========================
-- ASSETS (Cyber-Physical)
-- ==========================

CREATE TABLE IF NOT EXISTS assets (
    id UUID PRIMARY KEY,
    owner_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    description TEXT,
    location TEXT NOT NULL,
    kind asset_kind NOT NULL DEFAULT 'node', -- ✅ asset ontology
    status TEXT NOT NULL DEFAULT 'in_transit',
    nft_token TEXT,
    nft_image_url TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);

-- ==========================
-- ASSIGNMENTS (Scheduling & Ops)
-- ==========================

CREATE TABLE IF NOT EXISTS assignments (
    id UUID PRIMARY KEY,
    asset_id UUID NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    package_id UUID REFERENCES packages(id) ON DELETE SET NULL,
    inventory_id UUID REFERENCES inventory(id) ON DELETE SET NULL,
    schedule_start TIMESTAMP,
    schedule_end TIMESTAMP,
    recurrence_rule TEXT,
    eta TIMESTAMP,
    completed_at TIMESTAMP,
    status TEXT NOT NULL DEFAULT 'scheduled',
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);

-- ==========================
-- INDEXES
-- ==========================

CREATE INDEX IF NOT EXISTS idx_assignments_asset_id ON assignments(asset_id);
CREATE INDEX IF NOT EXISTS idx_assignments_package_id ON assignments(package_id);
CREATE INDEX IF NOT EXISTS idx_assignments_inventory_id ON assignments(inventory_id);
CREATE INDEX IF NOT EXISTS idx_assignments_status ON assignments(status);

