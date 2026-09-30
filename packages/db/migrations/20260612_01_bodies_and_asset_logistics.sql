-- =====================================================================
-- 🛰️ TIDasONE — Universal Bodies + Asset Logistics
-- Multi-body addressing (Earth/Moon/Mars/Jupiter/exoplanets), BOM tree,
-- structured part/serial/lot, flexible grouping (tags/kits),
-- AstroNet hop graph, named surface features.
-- =====================================================================

-- ---------------------------------------------------------------------
-- 1. Fix the silent no-op from 20251025_01_assets.sql
--    The original create_schema migration won the IF NOT EXISTS race,
--    so meta JSONB never landed. Backfill the missing columns here.
-- ---------------------------------------------------------------------
ALTER TABLE assets
  ADD COLUMN IF NOT EXISTS meta          JSONB DEFAULT '{}'::jsonb,
  ADD COLUMN IF NOT EXISTS part_number   TEXT,
  ADD COLUMN IF NOT EXISTS serial_number TEXT,
  ADD COLUMN IF NOT EXISTS lot_number    TEXT,
  ADD COLUMN IF NOT EXISTS manufacturer  TEXT,
  ADD COLUMN IF NOT EXISTS supplier      TEXT,
  ADD COLUMN IF NOT EXISTS body_id       INT,
  ADD COLUMN IF NOT EXISTS lat           DOUBLE PRECISION,
  ADD COLUMN IF NOT EXISTS lon           DOUBLE PRECISION,
  ADD COLUMN IF NOT EXISTS alt           DOUBLE PRECISION,
  ADD COLUMN IF NOT EXISTS mass_kg       DOUBLE PRECISION,
  ADD COLUMN IF NOT EXISTS volume_m3     DOUBLE PRECISION,
  ADD COLUMN IF NOT EXISTS hazmat_class  TEXT,
  ADD COLUMN IF NOT EXISTS condition     TEXT,
  ADD COLUMN IF NOT EXISTS lifecycle     TEXT,
  ADD COLUMN IF NOT EXISTS custody_node  UUID,
  ADD COLUMN IF NOT EXISTS expires_at    TIMESTAMPTZ,
  ADD COLUMN IF NOT EXISTS retired_at    TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_assets_part_number   ON assets(part_number);
CREATE INDEX IF NOT EXISTS idx_assets_serial_number ON assets(serial_number);
CREATE INDEX IF NOT EXISTS idx_assets_lot_number    ON assets(lot_number);
CREATE INDEX IF NOT EXISTS idx_assets_body_id       ON assets(body_id);
CREATE INDEX IF NOT EXISTS idx_assets_lifecycle     ON assets(lifecycle);


-- ---------------------------------------------------------------------
-- 2. bodies — the universe registry (NAIF + Exoplanet Archive)
-- ---------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS bodies (
  body_id        INT PRIMARY KEY,                -- NAIF id for solar system; synthetic for stars/exoplanets
  name           TEXT NOT NULL,
  parent_id      INT REFERENCES bodies(body_id),
  body_class     TEXT NOT NULL,                  -- star|planet|dwarf_planet|moon|asteroid|comet|spacecraft|exoplanet
  frame          TEXT,                           -- IAU_EARTH, IAU_MARS, MOON_ME, ICRF, ...
  radius_km      DOUBLE PRECISION,
  gravity_ms2    DOUBLE PRECISION,
  rotation_hr    DOUBLE PRECISION,               -- sidereal rotation period
  ephemeris_src  TEXT,                           -- naif|horizons|exoplanet_archive|manual
  distance_pc    DOUBLE PRECISION,               -- for stars/exoplanets, in parsecs
  meta           JSONB DEFAULT '{}'::jsonb,
  created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_bodies_class  ON bodies(body_class);
CREATE INDEX IF NOT EXISTS idx_bodies_parent ON bodies(parent_id);
CREATE INDEX IF NOT EXISTS idx_bodies_name   ON bodies(name);


-- ---------------------------------------------------------------------
-- 3. asset_parts — the BOM tree (self-referencing through assets)
--    Lets you ask "find every rover containing battery cells from lot X"
-- ---------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS asset_parts (
  parent_id    UUID NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
  child_id     UUID NOT NULL REFERENCES assets(id) ON DELETE RESTRICT,
  qty          NUMERIC NOT NULL DEFAULT 1,
  position     TEXT NOT NULL DEFAULT '',         -- "bay A3", "left actuator", slot ('' = unspecified)
  criticality  TEXT,                             -- safety|operational|cosmetic
  installed_at TIMESTAMPTZ DEFAULT NOW(),
  removed_at   TIMESTAMPTZ,
  PRIMARY KEY (parent_id, child_id, position)
);

CREATE INDEX IF NOT EXISTS idx_asset_parts_child ON asset_parts(child_id);


-- ---------------------------------------------------------------------
-- 4. Flexible grouping: tags + kits + categories
-- ---------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS asset_tags (
  asset_id UUID NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
  tag      TEXT NOT NULL,
  PRIMARY KEY (asset_id, tag)
);
CREATE INDEX IF NOT EXISTS idx_asset_tags_tag ON asset_tags(tag);

CREATE TABLE IF NOT EXISTS asset_kits (
  id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  name        TEXT NOT NULL,
  description TEXT,
  owner_id    UUID REFERENCES users(id) ON DELETE SET NULL,
  meta        JSONB DEFAULT '{}'::jsonb,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS asset_kit_items (
  kit_id   UUID NOT NULL REFERENCES asset_kits(id) ON DELETE CASCADE,
  asset_id UUID NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
  qty      NUMERIC NOT NULL DEFAULT 1,
  PRIMARY KEY (kit_id, asset_id)
);

CREATE TABLE IF NOT EXISTS asset_categories (
  id        UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  name      TEXT NOT NULL,
  parent_id UUID REFERENCES asset_categories(id) ON DELETE SET NULL,
  meta      JSONB DEFAULT '{}'::jsonb,
  UNIQUE(name, parent_id)
);

CREATE TABLE IF NOT EXISTS asset_category_links (
  asset_id    UUID NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
  category_id UUID NOT NULL REFERENCES asset_categories(id) ON DELETE CASCADE,
  PRIMARY KEY (asset_id, category_id)
);


-- ---------------------------------------------------------------------
-- 5. Universal addressing on telemetry + outposts
--    A node or telemetry point on Mars or Europa is now first-class.
-- ---------------------------------------------------------------------
ALTER TABLE fleet_telemetry
  ADD COLUMN IF NOT EXISTS body_id INT,
  ADD COLUMN IF NOT EXISTS frame   TEXT;

CREATE INDEX IF NOT EXISTS idx_fleet_telemetry_body ON fleet_telemetry(body_id);

ALTER TABLE node_registry
  ADD COLUMN IF NOT EXISTS body_id INT,
  ADD COLUMN IF NOT EXISTS lat     DOUBLE PRECISION,
  ADD COLUMN IF NOT EXISTS lon     DOUBLE PRECISION,
  ADD COLUMN IF NOT EXISTS alt     DOUBLE PRECISION;

CREATE INDEX IF NOT EXISTS idx_node_registry_body ON node_registry(body_id);


-- ---------------------------------------------------------------------
-- 6. surface_features — named points on a body (landing sites, craters,
--    bases, poles, future outpost candidates).
-- ---------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS surface_features (
  id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  body_id      INT NOT NULL REFERENCES bodies(body_id),
  name         TEXT NOT NULL,
  feature_type TEXT,                             -- crater|mons|mare|vallis|planitia|chaos|landing_site|base|pole|outpost
  lat          DOUBLE PRECISION,
  lon          DOUBLE PRECISION,
  alt          DOUBLE PRECISION,
  source       TEXT,                             -- naif|pds|iau_gazetteer|nasa|manual
  meta         JSONB DEFAULT '{}'::jsonb,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  UNIQUE(body_id, name)
);

CREATE INDEX IF NOT EXISTS idx_surface_features_body ON surface_features(body_id);
CREATE INDEX IF NOT EXISTS idx_surface_features_type ON surface_features(feature_type);


-- ---------------------------------------------------------------------
-- 7. astronet_hops — the routing graph between bodies/orbits.
--    Symbolic for now (hardcoded delta-v + transit). Upgrade per edge
--    later by pulling real values from JPL Horizons.
-- ---------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS astronet_hops (
  id            BIGSERIAL PRIMARY KEY,
  from_body_id  INT NOT NULL REFERENCES bodies(body_id),
  to_body_id    INT NOT NULL REFERENCES bodies(body_id),
  hop_kind      TEXT NOT NULL,                   -- surface|orbit_insert|transfer|landing|cislunar|interplanetary|gravity_assist
  delta_v_kms   DOUBLE PRECISION,
  transit_days  DOUBLE PRECISION,
  cost_per_kg   NUMERIC,                         -- in TIDasToken
  window_rule   TEXT,                            -- 'always' | 'hohmann:26mo' | 'launch_window:annual'
  meta          JSONB DEFAULT '{}'::jsonb,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  UNIQUE (from_body_id, to_body_id, hop_kind)
);

CREATE INDEX IF NOT EXISTS idx_astronet_hops_from ON astronet_hops(from_body_id);
CREATE INDEX IF NOT EXISTS idx_astronet_hops_to   ON astronet_hops(to_body_id);
