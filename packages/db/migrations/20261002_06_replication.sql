-- Replication: so a dark site's last-known stock still counts.
--
-- The rollup answers "what do we have, everywhere" by fanning out live reads
-- to every peer. That is honest but it only works for sites currently in
-- contact: an outpost that is dark contributes *nothing*, and a farm office on
-- a weekly uplink is therefore invisible six days out of seven. The total went
-- from "what we have" to "what we have at the places answering the phone", and
-- the only thing distinguishing those two is a `complete: false` flag that is
-- easy to read past.
--
-- A snapshot fixes the common case. The stock at Jezero-Farm three days ago is
-- not the truth, but it is far closer to the truth than zero, and it is the
-- number an operator would use if you asked them. So peers' summaries are kept
-- locally and used when the peer cannot be reached — with the age attached, and
-- counted separately, so nobody mistakes one for the other.
--
-- Three rules this schema exists to enforce:
--
--   1. A replica is a cache of someone else's authoritative data. Nothing but
--      the replication path writes it, and no part of the application treats
--      it as a source of truth for a decision that needs one (a reservation, a
--      custody transfer, a settlement).
--   2. The age is the *data's* age, taken from the peer's own clock, not the
--      age of the row. A snapshot re-fetched unchanged is not fresher for it.
--   3. A snapshot is scoped by organisation, because the summary it came from
--      was. Forgetting that would make this table the one place where one
--      org's holdings could be served to another.

-- === What a peer told us, and when ===
--
-- One row per (peer, organisation). Replaced wholesale on each successful
-- fetch rather than appended to: this is a current-state cache, not a history,
-- and replace-on-write is what keeps it bounded without a retention job.
CREATE TABLE IF NOT EXISTS replica_sources (
    node_id      UUID NOT NULL,

    -- Null means "unaffiliated holdings" — a real bucket, since `caller_org`
    -- fails closed and a caller with no organisation sees exactly the rows
    -- whose org_id is null.
    org_id       UUID,

    -- Null is not allowed in a primary key, so the key is built on this
    -- instead. A generated column rather than a COALESCE in every query,
    -- because the one query that forgot would silently mix two orgs' caches.
    org_key      UUID NOT NULL GENERATED ALWAYS AS
                 (COALESCE(org_id, '00000000-0000-0000-0000-000000000000'::uuid)) STORED,

    outpost_name TEXT NOT NULL,
    body_id      BIGINT,
    region       TEXT,

    -- The peer's own clock, from its summary. This is what "three days old"
    -- means; `fetched_at` only says when we last managed to ask.
    generated_at TIMESTAMPTZ NOT NULL,
    fetched_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    item_count   INTEGER NOT NULL DEFAULT 0,

    PRIMARY KEY (node_id, org_key)
);

-- === The snapshot itself ===
--
-- Deliberately mirrors the shape of `rollup::SummaryItem` rather than the
-- shape of `inventory`. It is a copy of what a peer *reported*, already
-- collapsed by that peer — re-deriving it from a replica of raw inventory rows
-- would mean reimplementing the peer's own grouping and getting a subtly
-- different answer.
CREATE TABLE IF NOT EXISTS replica_inventory (
    id              BIGSERIAL PRIMARY KEY,
    node_id         UUID NOT NULL,
    org_id          UUID,
    org_key         UUID NOT NULL GENERATED ALWAYS AS
                    (COALESCE(org_id, '00000000-0000-0000-0000-000000000000'::uuid)) STORED,

    name            TEXT NOT NULL,
    unit            TEXT NOT NULL,
    category        TEXT NOT NULL,
    location        TEXT,
    quantity        BIGINT NOT NULL,
    threshold       BIGINT NOT NULL,
    below_threshold BOOLEAN NOT NULL,

    -- Denormalised from replica_sources so a read of one peer's snapshot does
    -- not need the join to know how old it is. The rollup reads this on every
    -- request for every dark peer.
    generated_at    TIMESTAMPTZ NOT NULL
);

-- The one hot read: everything one peer reported for one organisation.
CREATE INDEX IF NOT EXISTS idx_replica_inventory_source
    ON replica_inventory (node_id, org_key);

-- Answering "what do we hold of this, everywhere" without scanning the table.
CREATE INDEX IF NOT EXISTS idx_replica_inventory_item
    ON replica_inventory (org_key, lower(name), lower(unit));

COMMENT ON TABLE replica_sources IS
  'Per-(peer, org) snapshot metadata. generated_at is the peer''s clock; fetched_at is ours.';
COMMENT ON TABLE replica_inventory IS
  'Cached copy of a peer''s own summary. Never authoritative — see the migration header.';
COMMENT ON COLUMN replica_sources.org_key IS
  'COALESCE(org_id, nil uuid), so the key works with an unaffiliated bucket. Never mix orgs.';
