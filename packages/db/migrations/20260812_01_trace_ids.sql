-- Correlation IDs across the autonomy pipeline.
--
-- One telemetry row can fan out into several rule firings, each queueing a
-- command that is delivered to a remote outpost and acked back. Until now
-- those records shared no key, so "why did rover-7 lock down?" meant guessing
-- from timestamps. A trace_id minted when a telemetry row is picked up is
-- carried onto every ops_event and queued command it causes, and travels with
-- the pushed command so the receiving outpost logs the same id.

ALTER TABLE ops_events    ADD COLUMN IF NOT EXISTS trace_id UUID;
ALTER TABLE command_queue ADD COLUMN IF NOT EXISTS trace_id UUID;

-- The lookup this exists to serve: pull the whole causal chain by trace id.
CREATE INDEX IF NOT EXISTS idx_ops_events_trace    ON ops_events    (trace_id) WHERE trace_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_command_queue_trace ON command_queue (trace_id) WHERE trace_id IS NOT NULL;
