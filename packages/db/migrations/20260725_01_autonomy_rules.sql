-- Autonomous ops policy rules. The rules engine evaluates each incoming
-- telemetry row against every enabled rule; when a rule fires it queues the
-- command into command_queue and records an ops_events entry.
CREATE TABLE IF NOT EXISTS autonomy_rules (
  id            BIGSERIAL PRIMARY KEY,
  name          TEXT NOT NULL UNIQUE,
  description   TEXT,
  enabled       BOOLEAN NOT NULL DEFAULT true,
  -- Telemetry metric to test: temperature|anomaly_score|speed|battery|signal_db|heading
  metric        TEXT NOT NULL,
  op            TEXT NOT NULL CHECK (op IN ('gt','gte','lt','lte','eq')),
  threshold     DOUBLE PRECISION NOT NULL,
  -- Command queued when the rule fires, e.g. {"type":"DIAGNOSTIC_SNAPSHOT","args":{"mode":"thermal"}}
  command       JSONB NOT NULL,
  event_kind    TEXT NOT NULL DEFAULT 'rule_fired',
  severity      TEXT NOT NULL DEFAULT 'warn',
  -- Minimum seconds between firings per asset (prevents command storms)
  cooldown_secs INTEGER NOT NULL DEFAULT 300 CHECK (cooldown_secs >= 0),
  created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Last firing per (rule, asset), used as an atomic cooldown gate.
-- Telemetry without an asset_id is tracked under the nil UUID.
CREATE TABLE IF NOT EXISTS autonomy_rule_firings (
  rule_id   BIGINT NOT NULL REFERENCES autonomy_rules(id) ON DELETE CASCADE,
  asset_id  UUID NOT NULL,
  fired_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  PRIMARY KEY (rule_id, asset_id)
);

-- Default policies (previously hardcoded in autonomy.rs / telemetry_processor.rs)
INSERT INTO autonomy_rules (name, description, metric, op, threshold, command, event_kind, severity, cooldown_secs)
VALUES
  ('overheat-thermal-snapshot',
   'Asset running hot: capture a thermal diagnostic snapshot',
   'temperature', 'gt', 80.0,
   '{"type":"DIAGNOSTIC_SNAPSHOT","args":{"mode":"thermal"}}',
   'overheat', 'warn', 300),
  ('anomaly-diagnostic-snapshot',
   'Anomaly score above threshold: capture a diagnostic snapshot',
   'anomaly_score', 'gte', 0.80,
   '{"type":"DIAGNOSTIC_SNAPSHOT"}',
   'anomaly', 'warn', 120),
  ('low-battery-power-save',
   'Battery below 15%: switch asset to power-save profile',
   'battery', 'lt', 15.0,
   '{"type":"SET_POWER_PROFILE","args":{"profile":"save"}}',
   'low_battery', 'warn', 1800)
ON CONFLICT (name) DO NOTHING;
