-- Guarded so migrations stay re-runnable: they are applied on every container
-- start, and an unguarded ADD COLUMN succeeds once and fails thereafter.
ALTER TABLE assets
  ADD COLUMN IF NOT EXISTS updated_at TIMESTAMP NOT NULL DEFAULT NOW();
