-- 组织通知只由控制面的后台任务评估。账务摄入路径不写通知表。
CREATE TABLE billing_alert_settings (
  tenant_id uuid PRIMARY KEY REFERENCES tenants(id),
  version bigint NOT NULL DEFAULT 1,
  low_balance_micro_cny bigint CHECK (
    low_balance_micro_cny BETWEEN 0 AND 99999999990000 AND low_balance_micro_cny % 10000 = 0),
  budget_percent integer CHECK (budget_percent BETWEEN 1 AND 100),
  spend_spike_percent integer CHECK (spend_spike_percent BETWEEN 101 AND 1000),
  email text,
  sealed_webhook text,
  spend_checked_at timestamptz
);

CREATE INDEX billing_alert_settings_enabled ON billing_alert_settings (tenant_id)
  WHERE (low_balance_micro_cny IS NOT NULL OR budget_percent IS NOT NULL OR spend_spike_percent IS NOT NULL)
    AND (email IS NOT NULL OR sealed_webhook IS NOT NULL);

-- 每个组织、规则、渠道只保存最近一次尝试。先认领再发送，避免多副本重复通知。
CREATE TABLE billing_alert_states (
  tenant_id uuid NOT NULL REFERENCES billing_alert_settings(tenant_id),
  kind text NOT NULL CHECK (kind IN ('low_balance', 'budget_threshold', 'spend_spike')),
  channel text NOT NULL CHECK (channel IN ('email', 'webhook')),
  breached boolean NOT NULL,
  window_start timestamptz,
  last_claimed_at timestamptz NOT NULL,
  delivery_id uuid NOT NULL,
  version bigint NOT NULL,
  attempts integer NOT NULL CHECK (attempts BETWEEN 1 AND 3),
  next_attempt_at timestamptz NOT NULL,
  observed_micro_cny bigint NOT NULL,
  threshold_micro_cny bigint NOT NULL,
  outcome text NOT NULL CHECK (outcome IN ('claimed', 'sent', 'failed', 'skipped')),
  PRIMARY KEY (tenant_id, kind, channel)
);
