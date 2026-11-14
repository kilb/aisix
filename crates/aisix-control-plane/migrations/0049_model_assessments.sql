-- 能力对照计划及不可重放派发；全部数据独立于正式用量、健康和账单。
CREATE TABLE model_assessment_runs (
  id uuid PRIMARY KEY,
  actor_id uuid NOT NULL REFERENCES operators(id),
  client_request_id uuid,
  request_actor_id uuid REFERENCES operators(id),
  start_payload_hash text,
  reference_model_id uuid NOT NULL,
  candidate_model_id uuid NOT NULL,
  state text NOT NULL CHECK (state IN ('planned','queued','running','completed','cancelled','interrupted')),
  created_at timestamptz NOT NULL DEFAULT now(),
  expires_at timestamptz NOT NULL,
  started_at timestamptz,
  finished_at timestamptz,
  deadline_at timestamptz,
  cancel_requested_at timestamptz,
  completion_reason text CHECK (completion_reason IN ('all_scheduled','cancelled','deadline','budget','configuration_changed','owner_lost')),
  owner_generation uuid,
  heartbeat_at timestamptz,
  reserved_requests integer NOT NULL DEFAULT 0 CHECK (reserved_requests >= 0),
  max_requests integer NOT NULL CHECK (max_requests IN (20,160)),
  document jsonb NOT NULL CHECK (octet_length(document::text) <= 65536),
  plan jsonb NOT NULL CHECK (octet_length(plan::text) <= 16777216),
  UNIQUE(actor_id,client_request_id),
  UNIQUE(request_actor_id,client_request_id),
  CHECK (reference_model_id <> candidate_model_id),
  CHECK (reserved_requests <= max_requests),
  CHECK ((client_request_id IS NULL) = (start_payload_hash IS NULL)),
  CHECK ((client_request_id IS NULL) = (request_actor_id IS NULL)),
  CHECK (request_actor_id IS NULL OR request_actor_id = actor_id)
);
CREATE INDEX model_assessment_runs_history ON model_assessment_runs(created_at DESC,id DESC);
CREATE INDEX model_assessment_runs_worker ON model_assessment_runs(state,heartbeat_at);
CREATE TABLE model_assessment_samples (
  id uuid PRIMARY KEY,
  run_id uuid NOT NULL REFERENCES model_assessment_runs(id) ON DELETE CASCADE,
  ordinal integer NOT NULL CHECK (ordinal >= 0 AND ordinal < 160),
  pair_id uuid NOT NULL,
  role text NOT NULL CHECK (role IN ('reference','candidate')),
  account_id uuid NOT NULL,
  dispatch_state text NOT NULL DEFAULT 'not_sent' CHECK (dispatch_state IN ('not_sent','reserved','sent','finished','cancelled_before_send','aborted_after_send','interrupted_unknown')),
  owner_generation uuid,
  dispatch_deadline_at timestamptz,
  sent_at timestamptz,
  finished_at timestamptz,
  document jsonb NOT NULL CHECK (octet_length(document::text) <= 16384),
  normalized_answer jsonb CHECK (normalized_answer IS NULL OR octet_length(normalized_answer::text) <= 131072),
  UNIQUE(run_id,ordinal),
  UNIQUE(run_id,pair_id,role),
  CHECK (dispatch_state IN ('not_sent','cancelled_before_send') OR (owner_generation IS NOT NULL AND dispatch_deadline_at IS NOT NULL))
);
-- 两个固定槽跨副本串行准入；账号唯一约束也在数据库中成立。
CREATE TABLE model_assessment_admission (
  slot smallint PRIMARY KEY CHECK (slot IN (1,2)),
  run_id uuid REFERENCES model_assessment_runs(id),
  sample_id uuid UNIQUE REFERENCES model_assessment_samples(id),
  account_id uuid UNIQUE,
  owner_generation uuid,
  dispatch_deadline_at timestamptz,
  isolated_until timestamptz,
  CHECK ((sample_id IS NULL) = (account_id IS NULL)),
  CHECK ((sample_id IS NULL) = (owner_generation IS NULL)),
  CHECK (sample_id IS NOT NULL OR (run_id IS NULL AND dispatch_deadline_at IS NULL AND isolated_until IS NULL)),
  CHECK (sample_id IS NULL OR (run_id IS NOT NULL AND dispatch_deadline_at IS NOT NULL AND isolated_until IS NOT NULL AND isolated_until >= dispatch_deadline_at + interval '60 seconds'))
);
INSERT INTO model_assessment_admission(slot) VALUES (1),(2);
