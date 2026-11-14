-- IO 终态是已结算用量的元数据补充；未知父行可稍后到达，不能提前造出用量或账单。
CREATE TABLE usage_io_audit_pending (
  dp_id uuid NOT NULL,
  event_id text NOT NULL,
  io_audit jsonb,
  received_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (dp_id, event_id)
);
CREATE INDEX usage_io_audit_pending_received ON usage_io_audit_pending (received_at);
