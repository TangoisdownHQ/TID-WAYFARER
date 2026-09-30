CREATE TABLE IF NOT EXISTS blockchain_alerts (
  id            UUID PRIMARY KEY,
  chain         TEXT NOT NULL,
  alert_type    TEXT NOT NULL,
  address       TEXT,
  tx_hash       TEXT,
  severity      TEXT NOT NULL DEFAULT 'medium',
  context       JSONB NOT NULL,
  reported_by   UUID,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_blockchain_alerts_chain 
ON blockchain_alerts(chain, created_at DESC);

