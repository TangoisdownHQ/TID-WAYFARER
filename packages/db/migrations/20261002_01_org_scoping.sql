-- Put every existing row and user inside an organisation.
--
-- The previous migration added org_id columns and the trust machinery, but
-- nothing read them: two organisations could exist, certify outposts and grant
-- each other scoped trust while still seeing all of each other's data. The
-- federation was decorative until this lands.
--
-- Everything that predates organisations belongs to one default org. Doing it
-- here rather than leaving NULLs matters: a nullable owner forces every query
-- to decide what a NULL means, and the safe reading ("belongs to nobody, so
-- hide it") and the convenient one ("belongs to everybody") are both wrong in
-- ways that only surface later.

DO $$
DECLARE
    default_org UUID;
BEGIN
    SELECT id INTO default_org FROM organisations WHERE slug = 'default';

    IF default_org IS NULL THEN
        INSERT INTO organisations (name, slug, root_public_key)
        VALUES (
            COALESCE(NULLIF(current_setting('wayfarer.default_org_name', true), ''), 'Default Organisation'),
            'default',
            -- A placeholder, deliberately recognisable. This org cannot issue
            -- usable node certificates until a real root key replaces it, and
            -- the string says so rather than looking like a key.
            'REPLACE-WITH-REAL-ORG-ROOT-PUBLIC-KEY-000000'
        )
        RETURNING id INTO default_org;
    END IF;

    -- Existing business data.
    UPDATE inventory  SET org_id = default_org WHERE org_id IS NULL;
    UPDATE orders     SET org_id = default_org WHERE org_id IS NULL;
    UPDATE capsules   SET org_id = default_org WHERE org_id IS NULL;
    UPDATE rate_cards SET org_id = default_org WHERE org_id IS NULL;

    -- Existing people. An admin becomes an owner so the org is administrable
    -- from the first login rather than needing a hand-written INSERT.
    INSERT INTO org_members (org_id, user_id, role)
    SELECT default_org, u.id,
           CASE WHEN u.role = 'admin' THEN 'owner' ELSE 'operator' END
    FROM users u
    WHERE NOT EXISTS (SELECT 1 FROM org_members m WHERE m.user_id = u.id)
    ON CONFLICT DO NOTHING;
END $$;

-- Reads filter on these constantly now.
CREATE INDEX IF NOT EXISTS idx_inventory_org_name ON inventory (org_id, name);
CREATE INDEX IF NOT EXISTS idx_orders_org_status  ON orders (org_id, status);

COMMENT ON COLUMN inventory.org_id IS
    'Owning organisation. Every read filters on this; a NULL would be invisible to every caller.';
