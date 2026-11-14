-- 原生对象只记录归属和恢复所需的元数据；账号凭据和原生输入输出不落此处。
CREATE TABLE native_operations (
  environment_id uuid NOT NULL,
  operation_id uuid NOT NULL,
  dp_id uuid NOT NULL,
  tenant_id uuid NOT NULL REFERENCES tenants(id),
  api_key_id uuid NOT NULL REFERENCES tenant_keys(id),
  member_id uuid REFERENCES members(id),
  submission jsonb NOT NULL CHECK (octet_length(submission::text) <= 1048576),
  state text NOT NULL DEFAULT 'pending_submission'
    CHECK (state IN ('pending_submission', 'bound', 'uncertain')),
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (environment_id, operation_id)
);

CREATE TABLE native_objects (
  id uuid PRIMARY KEY,
  environment_id uuid NOT NULL,
  operation_id uuid NOT NULL,
  tenant_id uuid NOT NULL,
  api_key_id uuid NOT NULL,
  member_id uuid,
  protocol text NOT NULL CHECK (protocol IN ('openai', 'anthropic')),
  kind text NOT NULL CHECK (kind IN ('response', 'file', 'message_batch')),
  model_id uuid NOT NULL,
  provider_key_id uuid NOT NULL,
  boundary text NOT NULL CHECK (length(boundary) = 67),
  raw_id text NOT NULL CHECK (octet_length(raw_id) BETWEEN 1 AND 512),
  parent_id uuid REFERENCES native_objects(id),
  version bigint NOT NULL DEFAULT 1,
  created_at timestamptz NOT NULL DEFAULT now(),
  provider_created_at timestamptz,
  provider_expires_at timestamptz,
  deleted_at timestamptz,
  pending boolean NOT NULL,
  poll_after timestamptz NOT NULL DEFAULT now(),
  lease_dp_id uuid,
  lease_expires_at timestamptz,
  lease_generation bigint NOT NULL DEFAULT 0,
  terminal_status text,
  completed_at timestamptz,
  FOREIGN KEY (environment_id, operation_id)
    REFERENCES native_operations(environment_id, operation_id),
  UNIQUE (environment_id, operation_id),
  UNIQUE (environment_id, protocol, kind, boundary, raw_id),
  CHECK (parent_id IS DISTINCT FROM id)
);
CREATE INDEX native_objects_owned ON native_objects
  (environment_id, tenant_id, protocol, kind, raw_id);
CREATE INDEX native_objects_pending ON native_objects (poll_after, id) WHERE pending;

-- 终态与这张表一起提交；普通用量保留期不删除这张表的去重凭据。
CREATE TABLE native_terminal_outbox (
  event_id uuid PRIMARY KEY,
  object_id uuid NOT NULL REFERENCES native_objects(id),
  entry_id text NOT NULL CHECK (octet_length(entry_id) <= 256),
  dp_id uuid NOT NULL,
  event jsonb NOT NULL CHECK (octet_length(event::text) <= 16384),
  created_at timestamptz NOT NULL DEFAULT now(),
  delivered_at timestamptz,
  UNIQUE (object_id, entry_id)
);
CREATE INDEX native_terminal_outbox_pending ON native_terminal_outbox
  (created_at, event_id) WHERE delivered_at IS NULL;

-- 防止重试或后续改动把既有归属、原始提交或已记账的事件改写。
CREATE FUNCTION native_authority_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF TG_TABLE_NAME = 'native_operations' THEN
    IF (to_jsonb(NEW) - 'state') IS DISTINCT FROM (to_jsonb(OLD) - 'state') THEN
      RAISE EXCEPTION 'native submission is immutable' USING ERRCODE = 'check_violation';
    END IF;
  ELSIF TG_TABLE_NAME = 'native_objects' THEN
    IF (NEW.id, NEW.environment_id, NEW.operation_id, NEW.tenant_id, NEW.api_key_id,
        NEW.member_id, NEW.protocol, NEW.kind, NEW.model_id, NEW.provider_key_id,
        NEW.boundary, NEW.raw_id, NEW.parent_id)
       IS DISTINCT FROM
       (OLD.id, OLD.environment_id, OLD.operation_id, OLD.tenant_id, OLD.api_key_id,
        OLD.member_id, OLD.protocol, OLD.kind, OLD.model_id, OLD.provider_key_id,
        OLD.boundary, OLD.raw_id, OLD.parent_id) THEN
      RAISE EXCEPTION 'native ownership is immutable' USING ERRCODE = 'check_violation';
    END IF;
    IF OLD.completed_at IS NOT NULL AND
       (NEW.completed_at, NEW.terminal_status, NEW.pending) IS DISTINCT FROM
       (OLD.completed_at, OLD.terminal_status, OLD.pending) THEN
      RAISE EXCEPTION 'native completion is immutable' USING ERRCODE = 'check_violation';
    END IF;
  ELSE
    IF (to_jsonb(NEW) - 'delivered_at') IS DISTINCT FROM (to_jsonb(OLD) - 'delivered_at')
       OR (OLD.delivered_at IS NOT NULL AND NEW.delivered_at IS DISTINCT FROM OLD.delivered_at) THEN
      RAISE EXCEPTION 'native terminal event is immutable' USING ERRCODE = 'check_violation';
    END IF;
  END IF;
  RETURN NEW;
END
$$;
CREATE TRIGGER native_operations_immutable BEFORE UPDATE ON native_operations
  FOR EACH ROW EXECUTE FUNCTION native_authority_immutable();
CREATE TRIGGER native_objects_immutable BEFORE UPDATE ON native_objects
  FOR EACH ROW EXECUTE FUNCTION native_authority_immutable();
CREATE TRIGGER native_terminal_outbox_immutable BEFORE UPDATE ON native_terminal_outbox
  FOR EACH ROW EXECUTE FUNCTION native_authority_immutable();
