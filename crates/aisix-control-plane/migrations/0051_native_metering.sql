-- 量的真实观察与定价结算分开；没有合同的量不能投影成零 token 账单。
CREATE TABLE native_metering_records (
  environment_id uuid NOT NULL,
  operation_id uuid NOT NULL,
  object_id uuid REFERENCES native_objects(id),
  target_kind text NOT NULL CHECK (target_kind IN ('object','operation')),
  dp_id uuid NOT NULL,
  tenant_id uuid NOT NULL REFERENCES tenants(id),
  api_key_id uuid NOT NULL REFERENCES tenant_keys(id),
  state text NOT NULL CHECK (state IN ('unknown','outbox_pending','awaiting_reconciliation','settled')),
  terminal_status text CHECK (octet_length(terminal_status) BETWEEN 1 AND 64),
  reason text CHECK (reason IN ('missing_usage','unknown_usage_shape','unpriced_meter',
    'incomplete_observation','uncertain_submission')),
  observed_usage jsonb NOT NULL CHECK (jsonb_typeof(observed_usage)='array'),
  artifacts jsonb NOT NULL CHECK (jsonb_typeof(artifacts)='array'),
  raw_usage jsonb CHECK (octet_length(raw_usage::text) <= 1048576),
  receipt_version bigint NOT NULL CHECK (receipt_version > 0),
  delivery_unpriced boolean NOT NULL DEFAULT false,
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (environment_id,operation_id),
  FOREIGN KEY (environment_id,operation_id) REFERENCES native_operations(environment_id,operation_id),
  CHECK ((target_kind='object')=(object_id IS NOT NULL)),
  CHECK (octet_length(observed_usage::text) <= 2097152),
  CHECK (octet_length(artifacts::text) <= 2097152),
  CHECK (state <> 'settled' OR (terminal_status IS NOT NULL AND reason IS NULL))
);
CREATE INDEX native_metering_reconciliation ON native_metering_records(environment_id,state,updated_at)
  WHERE state <> 'settled';
CREATE INDEX native_metering_tenant ON native_metering_records(environment_id,tenant_id,created_at);

-- 每个版本保留收到的原始 typed receipt；补量只追加，历史永不改写。
CREATE TABLE native_metering_receipts (
  environment_id uuid NOT NULL,
  operation_id uuid NOT NULL,
  version bigint NOT NULL CHECK (version > 0),
  observer_dp_id uuid NOT NULL,
  state text NOT NULL CHECK (state IN ('unknown','outbox_pending','awaiting_reconciliation','settled')),
  receipt jsonb NOT NULL CHECK (octet_length(receipt::text) <= 4194304),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (environment_id,operation_id,version),
  FOREIGN KEY (environment_id,operation_id) REFERENCES native_metering_records(environment_id,operation_id)
);

CREATE FUNCTION native_metering_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_TABLE_NAME='native_metering_receipts' OR TG_OP IN ('DELETE','TRUNCATE') THEN
    RAISE EXCEPTION 'native metering receipt is immutable' USING ERRCODE='check_violation';
  END IF;
  IF (NEW.environment_id,NEW.operation_id,NEW.dp_id,
      NEW.tenant_id,NEW.api_key_id,NEW.created_at)
    IS DISTINCT FROM
     (OLD.environment_id,OLD.operation_id,OLD.dp_id,
      OLD.tenant_id,OLD.api_key_id,OLD.created_at)
    OR ((NEW.object_id,NEW.target_kind) IS DISTINCT FROM (OLD.object_id,OLD.target_kind)
      AND NOT (OLD.object_id IS NULL AND OLD.target_kind='operation'
        AND OLD.reason='uncertain_submission' AND OLD.state<>'settled'
        AND NEW.object_id IS NOT NULL AND NEW.target_kind='object'
        AND EXISTS (SELECT 1 FROM native_objects object
          JOIN native_operations operation USING(environment_id,operation_id)
          WHERE object.id=NEW.object_id AND object.environment_id=NEW.environment_id
            AND object.operation_id=NEW.operation_id AND operation.submission->>'asynchronous'='true')))
    OR NEW.receipt_version <> OLD.receipt_version+1
    OR (OLD.state='settled')
    OR (OLD.state IN ('outbox_pending','awaiting_reconciliation') AND NEW.state='unknown')
    OR (OLD.delivery_unpriced AND NOT NEW.delivery_unpriced)
    OR (OLD.terminal_status IS NOT NULL AND NEW.terminal_status IS DISTINCT FROM OLD.terminal_status)
  THEN
    RAISE EXCEPTION 'native metering identity or settled receipt is immutable' USING ERRCODE='check_violation';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER native_metering_records_immutable BEFORE UPDATE OR DELETE ON native_metering_records
  FOR EACH ROW EXECUTE FUNCTION native_metering_immutable();
CREATE TRIGGER native_metering_receipts_immutable BEFORE UPDATE OR DELETE ON native_metering_receipts
  FOR EACH ROW EXECUTE FUNCTION native_metering_immutable();

-- 当前投影不能先提交而漏掉对应不可变 receipt；应用必须在同一事务追加两者。
CREATE FUNCTION native_metering_current_receipt() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM native_metering_receipts receipt
    WHERE receipt.environment_id=NEW.environment_id AND receipt.operation_id=NEW.operation_id
      AND receipt.version=NEW.receipt_version AND receipt.state=NEW.state) THEN
    RAISE EXCEPTION 'native metering receipt must commit with its projection' USING ERRCODE='check_violation';
  END IF;
  RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER native_metering_current_receipt AFTER INSERT OR UPDATE ON native_metering_records
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION native_metering_current_receipt();

CREATE TRIGGER native_metering_records_no_truncate BEFORE TRUNCATE ON native_metering_records
  FOR EACH STATEMENT EXECUTE FUNCTION native_metering_immutable();
CREATE TRIGGER native_metering_receipts_no_truncate BEFORE TRUNCATE ON native_metering_receipts
  FOR EACH STATEMENT EXECUTE FUNCTION native_metering_immutable();
