ALTER TABLE billing_profit_rows ADD COLUMN provider_key_id uuid;
CREATE TABLE billing_profit_account_daily (
  LIKE billing_profit_daily,
  provider_key_id uuid NOT NULL,
  PRIMARY KEY (day, provider_key_id)
);

-- 只回放安装触发器之前的历史，期间全站总计仍用原游标。
INSERT INTO billing_profit_backfill(version, high_seq, complete)
SELECT 2, high, high = 0 FROM (SELECT COALESCE(max(seq), 0) AS high FROM pricing_charges_cny) cutoff;

CREATE FUNCTION billing_profit_account_id(raw_id text) RETURNS uuid LANGUAGE sql IMMUTABLE STRICT AS $$
  SELECT CASE WHEN raw_id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
              THEN raw_id::uuid END
$$;

CREATE FUNCTION billing_profit_attribute_account() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE raw_id text;
BEGIN
  IF TG_OP = 'UPDATE' AND OLD.provider_key_id IS NOT NULL THEN
    NEW.provider_key_id := OLD.provider_key_id;
  ELSIF NEW.provider_key_id IS NULL THEN
    -- 常规摄入已有窄投影；只有升级时缺少投影的旧事件才读取原始 JSON。
    SELECT text_provider_key_id INTO raw_id FROM usage_metric_inputs
      WHERE event_id = NEW.event_id AND projection_version = 1;
    IF NOT FOUND THEN
      SELECT event->>'provider_key_id' INTO raw_id FROM usage_events WHERE event_id = NEW.event_id;
    END IF;
    NEW.provider_key_id := billing_profit_account_id(raw_id);
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER billing_profit_attribute_account BEFORE INSERT OR UPDATE ON billing_profit_rows
  FOR EACH ROW EXECUTE FUNCTION billing_profit_attribute_account();

CREATE FUNCTION billing_profit_rollup_accounts() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE removed text := '';
BEGIN
  IF NOT EXISTS (SELECT 1 FROM new_profit_rows WHERE provider_key_id IS NOT NULL) THEN
    RETURN NULL;
  END IF;
  IF TG_OP = 'UPDATE' THEN
    removed := 'UNION ALL SELECT -1, old_profit_rows.* FROM old_profit_rows WHERE provider_key_id IS NOT NULL';
  END IF;
  -- 原快照若未归属账号，重放时只加新值；普通更新只计同一账号的净差额。
  EXECUTE format($delta$
    WITH delta AS (
      SELECT 1 AS sign, new_profit_rows.* FROM new_profit_rows WHERE provider_key_id IS NOT NULL
      %2$s
    )
    INSERT INTO %1$I.billing_profit_account_daily AS daily
      (day, provider_key_id, charged_micro_cny, adjusted_micro_cny, known_cost_micro_cny,
       charged_with_cost_micro_cny, rows_with_cost, rows_without_cost, unpriced_rows, rows)
    SELECT (occurred_at AT TIME ZONE 'UTC')::date, provider_key_id,
           sum(sign * charged_micro_cny)::numeric, sum(sign * adjusted_micro_cny)::numeric, sum(sign * known_cost_micro_cny)::numeric,
           sum(sign * charged_with_cost_micro_cny)::numeric, sum(sign * rows_with_cost)::numeric, sum(sign * rows_without_cost)::numeric,
           sum(sign * unpriced_rows)::numeric, sum(sign * rows)::numeric
    FROM delta GROUP BY 1, 2 ORDER BY 1, 2
    ON CONFLICT(day, provider_key_id) DO UPDATE SET
      charged_micro_cny = daily.charged_micro_cny + EXCLUDED.charged_micro_cny,
      adjusted_micro_cny = daily.adjusted_micro_cny + EXCLUDED.adjusted_micro_cny,
      known_cost_micro_cny = daily.known_cost_micro_cny + EXCLUDED.known_cost_micro_cny,
      charged_with_cost_micro_cny = daily.charged_with_cost_micro_cny + EXCLUDED.charged_with_cost_micro_cny,
      rows_with_cost = daily.rows_with_cost + EXCLUDED.rows_with_cost,
      rows_without_cost = daily.rows_without_cost + EXCLUDED.rows_without_cost,
      unpriced_rows = daily.unpriced_rows + EXCLUDED.unpriced_rows,
      rows = daily.rows + EXCLUDED.rows
  $delta$, TG_TABLE_SCHEMA, removed);
  RETURN NULL;
END $$;
CREATE TRIGGER billing_profit_accounts_insert AFTER INSERT ON billing_profit_rows
  REFERENCING NEW TABLE AS new_profit_rows FOR EACH STATEMENT EXECUTE FUNCTION billing_profit_rollup_accounts();
CREATE TRIGGER billing_profit_accounts_update AFTER UPDATE ON billing_profit_rows
  REFERENCING OLD TABLE AS old_profit_rows NEW TABLE AS new_profit_rows
  FOR EACH STATEMENT EXECUTE FUNCTION billing_profit_rollup_accounts();
