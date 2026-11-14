-- 临时凭据只保存随机网关令牌摘要；默认模型是审计来源，不是永久授权。
CREATE TABLE native_realtime_credentials (
  id uuid PRIMARY KEY,
  environment_id uuid NOT NULL,
  operation_id uuid NOT NULL,
  token_hash text NOT NULL CHECK (token_hash ~ '^[0-9a-f]{64}$'),
  tenant_id uuid NOT NULL REFERENCES tenants(id),
  api_key_id uuid NOT NULL REFERENCES tenant_keys(id),
  member_id uuid REFERENCES members(id),
  creator_key_hash text NOT NULL,
  creator_key_version bigint NOT NULL,
  creator_member_role text,
  visibility_full boolean NOT NULL,
  purpose text NOT NULL CHECK (purpose IN ('realtime','transcription','translation')),
  expires_at timestamptz NOT NULL,
  defaults jsonb NOT NULL CHECK (octet_length(defaults::text) <= 262144),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  revoked_at timestamptz,
  FOREIGN KEY (environment_id,operation_id) REFERENCES native_operations(environment_id,operation_id),
  UNIQUE (environment_id,token_hash),
  UNIQUE (environment_id,operation_id),
  CHECK (expires_at > created_at)
);

CREATE TABLE native_live_sessions (
  environment_id uuid NOT NULL,
  operation_id uuid NOT NULL,
  tenant_id uuid NOT NULL REFERENCES tenants(id),
  api_key_id uuid NOT NULL REFERENCES tenant_keys(id),
  member_id uuid REFERENCES members(id),
  credential_id uuid REFERENCES native_realtime_credentials(id),
  purpose text NOT NULL CHECK (purpose IN ('realtime','transcription','translation')),
  transport text NOT NULL CHECK (transport IN ('web_socket','web_rtc','sip')),
  provider_session_id text CHECK (octet_length(provider_session_id) BETWEEN 1 AND 512),
  state text NOT NULL CHECK (state IN ('live','requires_stop','closed')),
  coverage text NOT NULL CHECK (coverage IN ('partial','complete','unknown')),
  accounting_state text NOT NULL CHECK (accounting_state IN ('pending','observed','awaiting_reconciliation','settled')),
  lease_dp_id uuid NOT NULL,
  lease_generation bigint NOT NULL CHECK (lease_generation > 0),
  lease_expires_at timestamptz NOT NULL,
  deadline_at timestamptz NOT NULL,
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  closed_at timestamptz,
  updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (environment_id,operation_id),
  FOREIGN KEY (environment_id,operation_id) REFERENCES native_operations(environment_id,operation_id),
  CHECK ((state='closed')=(closed_at IS NOT NULL))
);
CREATE INDEX native_live_sessions_recovery ON native_live_sessions
  (environment_id,lease_expires_at,operation_id) WHERE state <> 'closed';

-- 主模型之外的 ASR 等独立计费模型，先授权再转发配置；之后撤权不丢既有用量。
CREATE TABLE native_live_entries (
  environment_id uuid NOT NULL,
  operation_id uuid NOT NULL,
  entry_id text NOT NULL CHECK (octet_length(entry_id) <= 256),
  entry jsonb NOT NULL CHECK (octet_length(entry::text) <= 16384),
  accepted_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (environment_id,operation_id,entry_id),
  FOREIGN KEY (environment_id,operation_id) REFERENCES native_live_sessions(environment_id,operation_id)
);

-- 事件和响应两种真实身份都永久去重；普通 usage 保留期不会清除此账本。
CREATE TABLE native_live_journal (
  id uuid PRIMARY KEY,
  environment_id uuid NOT NULL,
  operation_id uuid NOT NULL,
  event_id text NOT NULL CHECK (octet_length(event_id) BETWEEN 1 AND 512),
  response_id text CHECK (octet_length(response_id) BETWEEN 1 AND 512),
  observer_dp_id uuid NOT NULL,
  usage jsonb NOT NULL CHECK (jsonb_typeof(usage)='array' AND octet_length(usage::text) <= 1048576),
  raw_usage jsonb CHECK (octet_length(raw_usage::text) <= 262144),
  coverage text NOT NULL CHECK (coverage IN ('partial','complete','unknown')),
  terminal boolean NOT NULL,
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  FOREIGN KEY (environment_id,operation_id) REFERENCES native_live_sessions(environment_id,operation_id),
  UNIQUE (environment_id,operation_id,event_id),
  UNIQUE (environment_id,operation_id,response_id)
);

-- 每个已收到的用量独立进入现有接收和定价事务，不等待 session 结束。
CREATE TABLE native_live_outbox (
  event_id uuid PRIMARY KEY,
  journal_id uuid NOT NULL REFERENCES native_live_journal(id),
  entry_id text NOT NULL CHECK (octet_length(entry_id) <= 256),
  dp_id uuid NOT NULL,
  event jsonb NOT NULL CHECK (octet_length(event::text) <= 32768),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  delivered_at timestamptz,
  pricing_state text CHECK (pricing_state IN ('charged','exempt','nothing_to_bill','unpriced')),
  CHECK ((delivered_at IS NULL)=(pricing_state IS NULL)),
  UNIQUE (journal_id,entry_id)
);
CREATE INDEX native_live_outbox_pending ON native_live_outbox(created_at,event_id)
  WHERE delivered_at IS NULL;

-- 同一供应商事件不能换签名路由认领给另一个租户；SIP headers 不参与归属判断。
CREATE TABLE native_sip_intakes (
  environment_id uuid NOT NULL,
  boundary text NOT NULL,
  provider_event_id text NOT NULL CHECK (octet_length(provider_event_id) BETWEEN 1 AND 512),
  webhook_id text NOT NULL CHECK (octet_length(webhook_id) BETWEEN 1 AND 512),
  call_id text NOT NULL CHECK (octet_length(call_id) BETWEEN 1 AND 512),
  route_id text NOT NULL,
  provider_key_id uuid NOT NULL,
  operation_id uuid NOT NULL,
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (environment_id,boundary,provider_event_id),
  UNIQUE (environment_id,boundary,call_id),
  UNIQUE (environment_id,boundary,webhook_id),
  FOREIGN KEY (environment_id,operation_id) REFERENCES native_live_sessions(environment_id,operation_id)
);

CREATE FUNCTION native_realtime_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP='DELETE' THEN
    RAISE EXCEPTION 'native realtime evidence is immutable' USING ERRCODE='check_violation';
  ELSIF TG_TABLE_NAME='native_realtime_credentials' THEN
    IF (to_jsonb(NEW)-'revoked_at') IS DISTINCT FROM (to_jsonb(OLD)-'revoked_at')
       OR (OLD.revoked_at IS NOT NULL AND NEW.revoked_at IS DISTINCT FROM OLD.revoked_at) THEN
      RAISE EXCEPTION 'native realtime credential is immutable' USING ERRCODE='check_violation';
    END IF;
  ELSIF TG_TABLE_NAME='native_live_sessions' THEN
    IF (NEW.environment_id,NEW.operation_id,NEW.tenant_id,NEW.api_key_id,NEW.member_id,
        NEW.credential_id,NEW.purpose,NEW.transport,NEW.provider_session_id,NEW.deadline_at,NEW.created_at)
      IS DISTINCT FROM
       (OLD.environment_id,OLD.operation_id,OLD.tenant_id,OLD.api_key_id,OLD.member_id,
        OLD.credential_id,OLD.purpose,OLD.transport,OLD.provider_session_id,OLD.deadline_at,OLD.created_at)
      OR NEW.lease_generation < OLD.lease_generation
      OR (OLD.state='closed' AND (NEW.state,NEW.closed_at) IS DISTINCT FROM (OLD.state,OLD.closed_at))
      OR (OLD.coverage='unknown' AND NEW.coverage <> 'unknown') THEN
      RAISE EXCEPTION 'native realtime identity or coverage is immutable' USING ERRCODE='check_violation';
    END IF;
  ELSIF TG_TABLE_NAME='native_live_outbox' THEN
    IF (to_jsonb(NEW)-'delivered_at'-'pricing_state') IS DISTINCT FROM (to_jsonb(OLD)-'delivered_at'-'pricing_state')
      OR (OLD.delivered_at IS NOT NULL AND (NEW.delivered_at,NEW.pricing_state) IS DISTINCT FROM (OLD.delivered_at,OLD.pricing_state)) THEN
      RAISE EXCEPTION 'native realtime usage is immutable' USING ERRCODE='check_violation';
    END IF;
  ELSE
    RAISE EXCEPTION 'native realtime journal is immutable' USING ERRCODE='check_violation';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER native_realtime_credentials_immutable BEFORE UPDATE OR DELETE ON native_realtime_credentials
  FOR EACH ROW EXECUTE FUNCTION native_realtime_immutable();
CREATE TRIGGER native_live_sessions_immutable BEFORE UPDATE OR DELETE ON native_live_sessions
  FOR EACH ROW EXECUTE FUNCTION native_realtime_immutable();
CREATE TRIGGER native_live_journal_immutable BEFORE UPDATE OR DELETE ON native_live_journal
  FOR EACH ROW EXECUTE FUNCTION native_realtime_immutable();
CREATE TRIGGER native_live_entries_immutable BEFORE UPDATE OR DELETE ON native_live_entries
  FOR EACH ROW EXECUTE FUNCTION native_realtime_immutable();
CREATE TRIGGER native_sip_intakes_immutable BEFORE UPDATE OR DELETE ON native_sip_intakes
  FOR EACH ROW EXECUTE FUNCTION native_realtime_immutable();
CREATE TRIGGER native_live_outbox_immutable BEFORE UPDATE OR DELETE ON native_live_outbox
  FOR EACH ROW EXECUTE FUNCTION native_realtime_immutable();
