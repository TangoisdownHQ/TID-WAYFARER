-- Closing the command loop.
--
-- Until now a pushed command was recorded as an ops_event and nothing else:
-- LOCKDOWN did not lock anything down, and core marked the command 'sent'
-- with no way to learn whether it worked. Two things were missing — somewhere
-- for an actuator to put durable state, and a place to record the outcome.

-- === Local outpost state an actuator can actually change ===
-- Key/value rather than columns: actuators are meant to be extensible, and a
-- new one should not need a migration to record its effect.
CREATE TABLE IF NOT EXISTS outpost_state (
  key         TEXT PRIMARY KEY,
  value       JSONB NOT NULL,
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  -- Which causal chain set this, so a lockdown traces back to the telemetry
  -- row that triggered it.
  trace_id    UUID
);

-- === Delivery outcome on the queue ===
-- command_queue already had acked_at; these record what the far side said.
ALTER TABLE command_queue ADD COLUMN IF NOT EXISTS ack_status TEXT;
ALTER TABLE command_queue ADD COLUMN IF NOT EXISTS ack_detail TEXT;

-- Operators ask "what is still unacknowledged?" far more than anything else.
CREATE INDEX IF NOT EXISTS idx_command_queue_unacked
  ON command_queue (status, sent_at)
  WHERE acked_at IS NULL;
