-- DTN replay protection.
--
-- The envelope signature covered the sender id and the body, and nothing else.
-- That is enough to prove who wrote a message and that it was not altered. It
-- is not enough to say *which* message it is, *when* it was written, how long
-- it stays valid, or who it was addressed to — so the same captured envelope
-- verified every time it was posted, at any node, forever.
--
-- Two consequences, one of them routine rather than hostile:
--
--   The forwarder retries on any non-2xx *or* a dropped connection. A delivery
--   that succeeded but whose response was lost got sent again, and the
--   receiver had no way to recognise it — so duplicates were produced by
--   ordinary operation, not just by an attacker.
--
--   Anything that consumed the inbox saw those duplicates as distinct events.
--   For a message that means two of a conversation. For a command or a
--   movement it means doing the thing twice.
--
-- The fix is a message identity that the signature covers, remembered for as
-- long as the message can legitimately arrive. Reception becomes idempotent:
-- a retransmit is absorbed and acknowledged, a replay is absorbed and
-- counted.

-- === Message identity ===
-- Carried on both sides so a delivery can be traced end to end, and so the
-- outbox knows when to stop trying.
ALTER TABLE dtn_outbox ADD COLUMN IF NOT EXISTS msg_id     UUID;
ALTER TABLE dtn_outbox ADD COLUMN IF NOT EXISTS expires_at TIMESTAMPTZ;

ALTER TABLE dtn_inbox  ADD COLUMN IF NOT EXISTS msg_id     UUID;
ALTER TABLE dtn_inbox  ADD COLUMN IF NOT EXISTS sent_at    TIMESTAMPTZ;
ALTER TABLE dtn_inbox  ADD COLUMN IF NOT EXISTS expires_at TIMESTAMPTZ;

-- A bundle past its lifetime is not worth a delivery attempt. Partial so the
-- forwarder's existing next_try_at scan is unaffected.
CREATE INDEX IF NOT EXISTS idx_dtn_outbox_expiry
  ON dtn_outbox (expires_at) WHERE expires_at IS NOT NULL;

-- === The replay window ===
-- One row per envelope this node has accepted. The primary key is the whole
-- defence: a second arrival of the same (sender, message) cannot be inserted,
-- which is how a retransmit and a replay become indistinguishable from each
-- other and harmless.
--
-- Scoped by sender because msg_id is chosen by the sender. A global unique
-- constraint would let one peer deny another peer a message id by claiming it
-- first.
CREATE TABLE IF NOT EXISTS dtn_seen (
    src_node_id UUID        NOT NULL,
    msg_id      UUID        NOT NULL,

    -- Rows are kept until the bundle could no longer legitimately arrive.
    -- Pruning earlier would reopen the window; keeping them forever would
    -- grow without bound, which is why the sender's chosen lifetime is capped
    -- on arrival rather than trusted.
    expires_at  TIMESTAMPTZ NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    PRIMARY KEY (src_node_id, msg_id)
);

CREATE INDEX IF NOT EXISTS idx_dtn_seen_expiry ON dtn_seen (expires_at);

-- === Refusals ===
-- An envelope that fails a check is no longer stored in the inbox, because
-- storing it there is what let an unauthenticated writer put rows in front of
-- consumers that were only filtering on `verified` by convention. It is
-- counted here instead, where it reads as what it is: something was turned
-- away, and this is why.
--
-- Absence of the payload is deliberate. A refused envelope is unauthenticated
-- input; keeping its body invites something downstream to read it.
CREATE TABLE IF NOT EXISTS dtn_rejected (
    id          BIGSERIAL PRIMARY KEY,
    src_node_id UUID,
    msg_id      UUID,
    reason      TEXT NOT NULL,
    detail      TEXT,
    at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_dtn_rejected_at ON dtn_rejected (at DESC);

COMMENT ON TABLE  dtn_seen     IS 'Accepted (sender, message) pairs. The primary key is the replay defence.';
COMMENT ON TABLE  dtn_rejected IS 'Envelopes turned away at the door, with the reason. No payload is kept.';
COMMENT ON COLUMN dtn_seen.expires_at IS 'Retained until the bundle could no longer legitimately arrive.';
