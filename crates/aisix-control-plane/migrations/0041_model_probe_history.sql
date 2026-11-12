-- 手工测试的有界脱敏样本；不写正式 usage 或计费数据，不回填旧验收结果。
CREATE TABLE model_probe_history (
  id bigserial PRIMARY KEY,
  model_id uuid NOT NULL,
  provider_key_id uuid,
  model_version bigint NOT NULL,
  provider_key_version bigint,
  model_name text NOT NULL,
  account_name text,
  actor_id uuid NOT NULL REFERENCES operators(id),
  mode text NOT NULL CHECK (mode IN ('baseline', 'capability')),
  started_at timestamptz NOT NULL DEFAULT now(),
  finished_at timestamptz,
  result jsonb,
  CHECK (model_version > 0),
  CHECK (provider_key_version IS NULL OR provider_key_version > 0),
  CHECK (result IS NULL OR octet_length(result::text) <= 16384)
);
CREATE INDEX model_probe_history_target ON model_probe_history (model_id, id DESC);
CREATE INDEX model_probe_history_mode ON model_probe_history (mode, id DESC);
CREATE INDEX model_probe_history_status ON model_probe_history ((result->>'status'), id DESC);
CREATE INDEX model_probe_history_time ON model_probe_history (started_at DESC, id DESC);
