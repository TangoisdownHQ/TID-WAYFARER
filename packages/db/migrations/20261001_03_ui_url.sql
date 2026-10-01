-- A browser-reachable address for each outpost, separate from its fabric one.
--
-- `api_endpoint` is how peers reach a node: a container hostname, a mesh
-- address, something behind a VPN. It is frequently not resolvable from an
-- operator's browser, so deriving a console link from it produced a URL that
-- looked right and went nowhere. The two are different facts and need
-- different columns.
ALTER TABLE node_registry ADD COLUMN IF NOT EXISTS ui_url TEXT;
