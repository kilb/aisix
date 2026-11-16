-- 入口仅记脏事件；日汇总在后台短事务更新，不持有调用摄入的全局热行。
CREATE TABLE billing_profit_pending (event_id text PRIMARY KEY);
CREATE TABLE billing_profit_rows (
  event_id text PRIMARY KEY,
  occurred_at timestamptz NOT NULL,
  charged_micro_cny numeric NOT NULL,
  adjusted_micro_cny numeric NOT NULL,
  known_cost_micro_cny numeric NOT NULL,
  charged_with_cost_micro_cny numeric NOT NULL,
  rows_with_cost numeric NOT NULL,
  rows_without_cost numeric NOT NULL,
  unpriced_rows numeric NOT NULL,
  rows numeric NOT NULL
);
CREATE TABLE billing_profit_daily (
  day date PRIMARY KEY,
  charged_micro_cny numeric NOT NULL,
  adjusted_micro_cny numeric NOT NULL,
  known_cost_micro_cny numeric NOT NULL,
  charged_with_cost_micro_cny numeric NOT NULL,
  rows_with_cost numeric NOT NULL,
  rows_without_cost numeric NOT NULL,
  unpriced_rows numeric NOT NULL,
  rows numeric NOT NULL
);
CREATE TABLE billing_profit_backfill (
  version integer PRIMARY KEY,
  after_seq bigint NOT NULL DEFAULT 0,
  high_seq bigint,
  complete boolean NOT NULL DEFAULT false
);
INSERT INTO billing_profit_backfill(version) VALUES (1);

-- 金额均保留 numeric；调整归属于原调用，成本永远读原收费快照。
CREATE FUNCTION billing_profit_current(ids text[]) RETURNS TABLE (
  event_id text, occurred_at timestamptz,
  charged_micro_cny numeric,
  adjusted_micro_cny numeric,
  known_cost_micro_cny numeric,
  charged_with_cost_micro_cny numeric,
  rows_with_cost numeric,
  rows_without_cost numeric,
  unpriced_rows numeric,
  rows numeric
) LANGUAGE sql STABLE AS $$
  SELECT c.event_id, c.occurred_at,
         (CASE WHEN c.status = 'charged' THEN COALESCE(c.charge_micro_cny, 0) ELSE 0 END)::numeric
           + COALESCE(a.adjusted, 0),
         COALESCE(a.adjusted, 0),
         CASE WHEN (c.status = 'charged' OR a.adjusted IS NOT NULL) AND c.cost_micro_cny IS NOT NULL
              THEN c.cost_micro_cny ELSE 0 END::numeric,
         CASE WHEN c.cost_micro_cny IS NOT NULL AND (c.status = 'charged' OR a.adjusted IS NOT NULL)
              THEN COALESCE(c.charge_micro_cny, 0)::numeric + COALESCE(a.adjusted, 0) ELSE 0 END,
         CASE WHEN (c.status = 'charged' OR a.adjusted IS NOT NULL) AND c.cost_micro_cny IS NOT NULL THEN 1 ELSE 0 END::numeric,
         CASE WHEN (c.status = 'charged' OR a.adjusted IS NOT NULL) AND c.cost_micro_cny IS NULL THEN 1 ELSE 0 END::numeric,
         CASE WHEN c.status = 'unpriced' AND a.adjusted IS NULL THEN 1 ELSE 0 END::numeric,
         1::numeric
  FROM pricing_charges_cny c
  LEFT JOIN LATERAL (
    SELECT sum(pa.delta_micro_cny)::numeric AS adjusted FROM pricing_adjustments_cny pa
    WHERE pa.event_id = c.event_id
  ) a ON true
  WHERE c.event_id = ANY(ids)
$$;

CREATE FUNCTION billing_profit_enqueue() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  -- UPDATE 参与后台的行锁；DO NOTHING 会丢掉后台旧快照看不到的并发调整。
  EXECUTE format('INSERT INTO %I.billing_profit_pending(event_id)
                  SELECT DISTINCT event_id FROM changed_profit_rows ORDER BY event_id
                  ON CONFLICT(event_id) DO UPDATE SET event_id = EXCLUDED.event_id', TG_TABLE_SCHEMA);
  RETURN NULL;
END;
$$;
CREATE TRIGGER billing_profit_charge_insert AFTER INSERT ON pricing_charges_cny
  REFERENCING NEW TABLE AS changed_profit_rows FOR EACH STATEMENT EXECUTE FUNCTION billing_profit_enqueue();
CREATE TRIGGER billing_profit_adjustment_insert AFTER INSERT ON pricing_adjustments_cny
  REFERENCING NEW TABLE AS changed_profit_rows FOR EACH STATEMENT EXECUTE FUNCTION billing_profit_enqueue();
