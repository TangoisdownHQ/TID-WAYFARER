-- Cross-organisation federation: federated roots with explicit pairwise trust.
--
-- See Documentation/OrgBoundaries.md for why this shape and not the other two.
-- In short: each organisation holds its own root keypair, two orgs that want
-- to trade exchange roots once out of band and each signs a trust grant naming
-- the other and what it is trusted for. Trust is bilateral, revocable by
-- either side alone, and — the deciding property — **verifiable offline**. An
-- outpost holding a counterparty's root can check that counterparty's node
-- certificate with no network, during a blackout, with no authority to
-- consult. A central CA cannot do that, which is why it was rejected.

CREATE TABLE IF NOT EXISTS organisations (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name            TEXT NOT NULL,
    slug            TEXT UNIQUE,

    -- The org's identity. The secret half lives wherever the org chooses and
    -- deliberately never enters this schema: an outpost needs to *verify*
    -- certificates, never to issue them, and a root key sitting on every
    -- outpost would make every outpost able to mint its own authority.
    root_public_key TEXT NOT NULL,

    -- Where settlement to this org is paid. Orgs trade, not individuals.
    wallet_address  TEXT,

    status          TEXT NOT NULL DEFAULT 'active',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT organisations_status_check CHECK (status IN ('active','suspended'))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_org_root_key ON organisations (root_public_key);

-- === Membership ===
CREATE TABLE IF NOT EXISTS org_members (
    org_id     UUID NOT NULL REFERENCES organisations(id) ON DELETE CASCADE,
    user_id    UUID NOT NULL REFERENCES users(id)         ON DELETE CASCADE,
    role       TEXT NOT NULL DEFAULT 'operator',
    added_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (org_id, user_id),
    CONSTRAINT org_members_role_check CHECK (role IN ('owner','admin','operator','viewer'))
);

CREATE INDEX IF NOT EXISTS idx_org_members_user ON org_members (user_id);

-- === Which outposts belong to an org, and the certificate proving it ===
--
-- The certificate binds (node_id, node_public_key, org_id, not_after) and is
-- signed by the org root. This is what lets a peer be verified without having
-- been met before: the guard stops asking "is this node in my registry" and
-- starts asking "does it present a certificate signed by a root I trust".
CREATE TABLE IF NOT EXISTS org_outposts (
    org_id          UUID NOT NULL REFERENCES organisations(id) ON DELETE CASCADE,
    node_id         UUID NOT NULL,
    node_public_key TEXT NOT NULL,
    certificate     TEXT NOT NULL,          -- base64 Ed25519 over the canonical form
    not_after       TIMESTAMPTZ,
    issued_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    revoked_at      TIMESTAMPTZ,
    PRIMARY KEY (org_id, node_id)
);

-- One node belongs to one org. A node certified by two orgs could act with
-- either's authority, which is precisely the confusion federation exists to
-- remove.
CREATE UNIQUE INDEX IF NOT EXISTS idx_org_outposts_node ON org_outposts (node_id);

-- === Trust grants ===
--
-- What this org has granted another, and until when. Scopes are explicit and
-- additive: absent means denied. `commands`, `telemetry` and `rules` are
-- deliberately NOT valid scopes — one org actuating another's outpost is a
-- safety boundary, not a business decision, so it is unrepresentable here
-- rather than merely switched off by default.
CREATE TABLE IF NOT EXISTS org_trust (
    id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id               UUID NOT NULL REFERENCES organisations(id) ON DELETE CASCADE,
    counterparty_name    TEXT NOT NULL,
    counterparty_root_key TEXT NOT NULL,
    scopes               TEXT[] NOT NULL DEFAULT '{}',
    granted_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at           TIMESTAMPTZ,
    revoked_at           TIMESTAMPTZ,
    -- Signed by the granting org's root, so the grant itself can travel over
    -- DTN and be verified by a third outpost that was not present when it was
    -- made.
    grant_signature      TEXT,
    note                 TEXT,

    UNIQUE (org_id, counterparty_root_key)
);

CREATE INDEX IF NOT EXISTS idx_org_trust_counterparty
    ON org_trust (counterparty_root_key) WHERE revoked_at IS NULL;

-- === Ownership on business data ===
-- Nullable during migration: existing rows belong to the default org, and a
-- NULL is treated as "this outpost's own org" so nothing breaks mid-rollout.
ALTER TABLE inventory  ADD COLUMN IF NOT EXISTS org_id UUID REFERENCES organisations(id);
ALTER TABLE orders     ADD COLUMN IF NOT EXISTS org_id UUID REFERENCES organisations(id);
ALTER TABLE capsules   ADD COLUMN IF NOT EXISTS org_id UUID REFERENCES organisations(id);
ALTER TABLE rate_cards ADD COLUMN IF NOT EXISTS org_id UUID REFERENCES organisations(id);

CREATE INDEX IF NOT EXISTS idx_inventory_org  ON inventory  (org_id);
CREATE INDEX IF NOT EXISTS idx_orders_org     ON orders     (org_id);
CREATE INDEX IF NOT EXISTS idx_capsules_org   ON capsules   (org_id);
CREATE INDEX IF NOT EXISTS idx_rate_cards_org ON rate_cards (org_id);

COMMENT ON TABLE organisations IS 'A party in the fabric. Holds a root keypair; only the public half is stored here.';
COMMENT ON TABLE org_trust IS 'Bilateral, revocable, offline-verifiable trust between two orgs.';
