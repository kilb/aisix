-- 并发建索引中断时 PostgreSQL 可能留下同名无效索引；阻止其被 IF NOT EXISTS 当作已完成。
DO $$
DECLARE
  source_table regclass := 'usage_events'::regclass;
  index_name text;
BEGIN
  SELECT format('%I.%I', n.nspname, 'usage_events_provider_key_seq') INTO index_name
  FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
  WHERE c.oid = source_table;
  IF NOT EXISTS (SELECT FROM pg_catalog.pg_index
                 WHERE indexrelid = to_regclass(index_name) AND indrelid = source_table
                   AND indisvalid AND indisready) THEN
    RAISE EXCEPTION '账户历史索引 % 未就绪', index_name
      USING HINT = format('REINDEX INDEX CONCURRENTLY %s; 完成后重新运行迁移。', index_name);
  END IF;
END $$;

-- 运营补填单独留痕；收费、原成本、行明细和余额均不改写。
CREATE TABLE pricing_cost_backfills (
  event_id text PRIMARY KEY REFERENCES pricing_charges_cny(event_id),
  operation_id uuid NOT NULL,
  provider_key_id uuid NOT NULL,
  rate_revision_id bigint NOT NULL,
  cost_micro_cny bigint NOT NULL CHECK (cost_micro_cny >= 0),
  created_by uuid NOT NULL REFERENCES operators(id),
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TRIGGER pricing_cost_backfills_append_only BEFORE UPDATE OR DELETE ON pricing_cost_backfills
  FOR EACH ROW EXECUTE FUNCTION billing_append_only();
CREATE TRIGGER pricing_cost_backfills_no_truncate BEFORE TRUNCATE ON pricing_cost_backfills
  FOR EACH STATEMENT EXECUTE FUNCTION billing_append_only();

-- 费率修订本身禁止更新、删除和截断。插入时核验引用，也要求费率确属本账户。
-- 这样直接截断费率表时仍由既有的 append-only 触发器拒绝。
CREATE FUNCTION pricing_cost_backfills_check_rates() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF EXISTS (SELECT FROM inserted_costs b
             WHERE NOT EXISTS (SELECT FROM pricing_cost_rates r
               WHERE r.revision_id = b.rate_revision_id AND r.provider_key_id = b.provider_key_id
                 AND r.cny_per_usd_e4 IS NOT NULL)) THEN
    RAISE EXCEPTION '补填成本须引用该账户已有的费率修订' USING ERRCODE = 'foreign_key_violation';
  END IF;
  RETURN NULL;
END $$;
CREATE TRIGGER pricing_cost_backfills_check_rates AFTER INSERT ON pricing_cost_backfills
  REFERENCING NEW TABLE AS inserted_costs FOR EACH STATEMENT EXECUTE FUNCTION pricing_cost_backfills_check_rates();

-- 所有利润读路径共用此函数，原成本优先。补填事务将相应事件交给已有的日汇总队列。
CREATE OR REPLACE FUNCTION billing_profit_current(ids text[]) RETURNS TABLE (
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
         CASE WHEN (c.status = 'charged' OR a.adjusted IS NOT NULL) AND cost.amount IS NOT NULL
              THEN cost.amount ELSE 0 END::numeric,
         CASE WHEN cost.amount IS NOT NULL AND (c.status = 'charged' OR a.adjusted IS NOT NULL)
              THEN COALESCE(c.charge_micro_cny, 0)::numeric + COALESCE(a.adjusted, 0) ELSE 0 END,
         CASE WHEN (c.status = 'charged' OR a.adjusted IS NOT NULL) AND cost.amount IS NOT NULL THEN 1 ELSE 0 END::numeric,
         CASE WHEN (c.status = 'charged' OR a.adjusted IS NOT NULL) AND cost.amount IS NULL THEN 1 ELSE 0 END::numeric,
         CASE WHEN c.status = 'unpriced' AND a.adjusted IS NULL THEN 1 ELSE 0 END::numeric,
         1::numeric
  FROM pricing_charges_cny c
  LEFT JOIN pricing_cost_backfills b ON b.event_id = c.event_id
  CROSS JOIN LATERAL (SELECT COALESCE(c.cost_micro_cny, b.cost_micro_cny) AS amount) cost
  LEFT JOIN LATERAL (
    SELECT sum(pa.delta_micro_cny)::numeric AS adjusted FROM pricing_adjustments_cny pa
    WHERE pa.event_id = c.event_id
  ) a ON true
  WHERE c.event_id = ANY(ids)
$$;
CREATE TRIGGER billing_profit_cost_backfill_insert AFTER INSERT ON pricing_cost_backfills
  REFERENCING NEW TABLE AS changed_profit_rows FOR EACH STATEMENT EXECUTE FUNCTION billing_profit_enqueue();
