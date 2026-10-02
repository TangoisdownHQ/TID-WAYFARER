-- People you can administer, and a way to talk to them.
--
-- Until now an account could only come into existence by signing itself up,
-- which is the wrong shape for every organisation that would actually run
-- this: a depot lead does not ask the warehouse to self-register, they add
-- three people and say which of them may approve a shipment. And once there
-- are named people on both sides of a trade, the conversation about a trade
-- has to live beside it rather than in someone's email.

-- === Accounts an administrator controls ===

-- Deactivation rather than deletion. A user who signed a custody receipt or
-- released a compliance hold is referenced by those records, and the records
-- are the point — a deleted row would leave a shipment attested by nobody.
ALTER TABLE users ADD COLUMN IF NOT EXISTS active BOOLEAN NOT NULL DEFAULT TRUE;

-- Set when an administrator creates the account or resets its password. The
-- temporary password is shown to the administrator once and never stored in
-- readable form, so this flag is the only thing that remembers it is
-- temporary.
ALTER TABLE users ADD COLUMN IF NOT EXISTS must_change_password BOOLEAN NOT NULL DEFAULT FALSE;

-- Who added whom. The first account on an outpost has no creator, so it is
-- nullable rather than defaulted to something untrue.
ALTER TABLE users ADD COLUMN IF NOT EXISTS created_by UUID REFERENCES users(id) ON DELETE SET NULL;

-- Answers "is this account in use?", which is the question actually asked
-- before deactivating someone.
ALTER TABLE users ADD COLUMN IF NOT EXISTS last_login_at TIMESTAMPTZ;
ALTER TABLE users ADD COLUMN IF NOT EXISTS full_name TEXT;

CREATE INDEX IF NOT EXISTS idx_users_active ON users (active) WHERE active;

-- === Conversations ===
--
-- Three kinds, because the authorisation differs and nothing else does:
--
--   org    everyone in one organisation. Internal coordination.
--   direct named people, all within one organisation.
--   deal   two organisations, and the only kind that crosses the boundary.
--
-- A deal thread is the whole reason this table exists. Buyers and sellers
-- have to be able to talk — about a substitution, a delivery window, a
-- damaged pallet — and the existing org-boundary model says marketplace is
-- the one scope an organisation may grant another. So a cross-org
-- conversation is anchored to the transaction that justifies it: an order, or
-- a bid on an order. Not to a global user directory, which would hand every
-- participant a list of every other organisation's staff.
CREATE TABLE IF NOT EXISTS chat_threads (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    kind       TEXT NOT NULL,
    subject    TEXT,

    -- What authorises the thread, for a deal. Exactly one of these is set.
    order_id   UUID REFERENCES orders(id) ON DELETE CASCADE,
    bid_id     UUID REFERENCES bids(id)   ON DELETE CASCADE,

    -- Set for org and direct threads: the single organisation they live in.
    org_id     UUID REFERENCES organisations(id) ON DELETE CASCADE,

    created_by UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Denormalised so the thread list can sort by activity without touching
    -- the message table. The list is the most-loaded read in a chat.
    last_message_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT chat_threads_kind CHECK (kind IN ('org', 'direct', 'deal')),

    -- A deal thread without an anchor would be a cross-org channel with
    -- nothing authorising it; an org thread with one would be ambiguous about
    -- which rule applies. The constraint is here rather than in the handler
    -- because the handler is not the only thing that will ever write a row.
    CONSTRAINT chat_threads_anchored CHECK (
        (kind = 'deal' AND (order_id IS NOT NULL OR bid_id IS NOT NULL))
        OR (kind <> 'deal' AND order_id IS NULL AND bid_id IS NULL AND org_id IS NOT NULL)
    )
);

-- One deal thread per anchor. Two threads on the same bid would split the
-- negotiation in half and let each side quote from a different one.
CREATE UNIQUE INDEX IF NOT EXISTS idx_chat_threads_one_per_order
    ON chat_threads (order_id) WHERE order_id IS NOT NULL AND bid_id IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_chat_threads_one_per_bid
    ON chat_threads (bid_id) WHERE bid_id IS NOT NULL;

-- === Who is in the room ===
--
-- Membership is the whole access rule: you may read a thread and post to it
-- if and only if there is a row here for you. Derived rules ("anyone in the
-- buying org") were tempting and wrong — an org's membership changes, and
-- someone who joins next month should not inherit a conversation about a
-- shipment that closed last month.
CREATE TABLE IF NOT EXISTS chat_participants (
    thread_id    UUID NOT NULL REFERENCES chat_threads(id) ON DELETE CASCADE,
    user_id      UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,

    -- Which side this person speaks for. Shown beside their name, because in
    -- a deal thread "who said this" is incomplete without "for whom".
    org_id       UUID REFERENCES organisations(id) ON DELETE SET NULL,

    -- Unread counts are derived from this, so it is per-participant rather
    -- than a read receipt per message: the question is "what have I not
    -- seen", not "who saw this".
    last_read_at TIMESTAMPTZ,
    added_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    PRIMARY KEY (thread_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_chat_participants_user ON chat_participants (user_id);

-- === Messages ===
CREATE TABLE IF NOT EXISTS chat_messages (
    id         BIGSERIAL PRIMARY KEY,
    thread_id  UUID NOT NULL REFERENCES chat_threads(id) ON DELETE CASCADE,
    sender_id  UUID REFERENCES users(id) ON DELETE SET NULL,
    body       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- A message that arrived over DTN from another outpost, kept so a
    -- conversation that crossed a disconnected link can be told apart from
    -- one that did not. Null for anything written locally.
    via_node_id UUID,

    CONSTRAINT chat_messages_body_not_blank CHECK (length(trim(body)) > 0)
);

-- The one hot read: a page of messages in a thread, newest last.
CREATE INDEX IF NOT EXISTS idx_chat_messages_thread ON chat_messages (thread_id, id);

-- Keeps the thread list's sort column honest without the write path having to
-- remember. A trigger rather than an UPDATE in the handler because the DTN
-- receive path will also insert messages, and the two must not drift.
CREATE OR REPLACE FUNCTION chat_touch_thread() RETURNS TRIGGER AS $$
BEGIN
  UPDATE chat_threads SET last_message_at = NEW.created_at WHERE id = NEW.thread_id;
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_chat_touch_thread ON chat_messages;
CREATE TRIGGER trg_chat_touch_thread
  AFTER INSERT ON chat_messages
  FOR EACH ROW EXECUTE FUNCTION chat_touch_thread();

COMMENT ON TABLE  chat_threads IS 'A deal thread is authorised by its order or bid, not by a user directory.';
COMMENT ON TABLE  chat_participants IS 'Membership is the access rule: no row here, no read and no post.';
COMMENT ON COLUMN users.active IS 'Deactivate rather than delete; signed records must keep naming a real signer.';
