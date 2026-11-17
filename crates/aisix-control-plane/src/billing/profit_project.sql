-- 同一锁序：脏队列 -> 逐事件快照 -> 按日期递增的日汇总。
WITH locked AS MATERIALIZED (
  SELECT unnest($1::text[]) AS event_id
), fresh AS MATERIALIZED (
  SELECT * FROM billing_profit_current($1::text[])
), delta AS MATERIALIZED (
  SELECT (f.occurred_at AT TIME ZONE 'UTC')::date AS day,
         f.charged_micro_cny - COALESCE(old.charged_micro_cny, 0) AS charged_micro_cny,
         f.adjusted_micro_cny - COALESCE(old.adjusted_micro_cny, 0) AS adjusted_micro_cny,
         f.known_cost_micro_cny - COALESCE(old.known_cost_micro_cny, 0) AS known_cost_micro_cny,
         f.charged_with_cost_micro_cny - COALESCE(old.charged_with_cost_micro_cny, 0) AS charged_with_cost_micro_cny,
         f.rows_with_cost - COALESCE(old.rows_with_cost, 0) AS rows_with_cost,
         f.rows_without_cost - COALESCE(old.rows_without_cost, 0) AS rows_without_cost,
         f.unpriced_rows - COALESCE(old.unpriced_rows, 0) AS unpriced_rows,
         f.rows - COALESCE(old.rows, 0) AS rows
  FROM fresh f LEFT JOIN billing_profit_rows old USING(event_id)
), stored AS (
  INSERT INTO billing_profit_rows(event_id, occurred_at, charged_micro_cny, adjusted_micro_cny, known_cost_micro_cny, charged_with_cost_micro_cny, rows_with_cost, rows_without_cost, unpriced_rows, rows)
  SELECT event_id, occurred_at, charged_micro_cny, adjusted_micro_cny, known_cost_micro_cny, charged_with_cost_micro_cny, rows_with_cost, rows_without_cost, unpriced_rows, rows FROM fresh ORDER BY event_id
  ON CONFLICT(event_id) DO UPDATE SET
    occurred_at = EXCLUDED.occurred_at,
    charged_micro_cny = EXCLUDED.charged_micro_cny,
    adjusted_micro_cny = EXCLUDED.adjusted_micro_cny,
    known_cost_micro_cny = EXCLUDED.known_cost_micro_cny,
    charged_with_cost_micro_cny = EXCLUDED.charged_with_cost_micro_cny,
    rows_with_cost = EXCLUDED.rows_with_cost,
    rows_without_cost = EXCLUDED.rows_without_cost,
    unpriced_rows = EXCLUDED.unpriced_rows,
    rows = EXCLUDED.rows
  RETURNING event_id
), daily AS (
  INSERT INTO billing_profit_daily(day, charged_micro_cny, adjusted_micro_cny, known_cost_micro_cny, charged_with_cost_micro_cny, rows_with_cost, rows_without_cost, unpriced_rows, rows)
  SELECT day, sum(charged_micro_cny)::numeric, sum(adjusted_micro_cny)::numeric, sum(known_cost_micro_cny)::numeric, sum(charged_with_cost_micro_cny)::numeric, sum(rows_with_cost)::numeric, sum(rows_without_cost)::numeric, sum(unpriced_rows)::numeric, sum(rows)::numeric FROM delta
  GROUP BY day ORDER BY day
  ON CONFLICT(day) DO UPDATE SET
    charged_micro_cny = billing_profit_daily.charged_micro_cny + EXCLUDED.charged_micro_cny,
    adjusted_micro_cny = billing_profit_daily.adjusted_micro_cny + EXCLUDED.adjusted_micro_cny,
    known_cost_micro_cny = billing_profit_daily.known_cost_micro_cny + EXCLUDED.known_cost_micro_cny,
    charged_with_cost_micro_cny = billing_profit_daily.charged_with_cost_micro_cny + EXCLUDED.charged_with_cost_micro_cny,
    rows_with_cost = billing_profit_daily.rows_with_cost + EXCLUDED.rows_with_cost,
    rows_without_cost = billing_profit_daily.rows_without_cost + EXCLUDED.rows_without_cost,
    unpriced_rows = billing_profit_daily.unpriced_rows + EXCLUDED.unpriced_rows,
    rows = billing_profit_daily.rows + EXCLUDED.rows
  RETURNING day
), removed AS (
  DELETE FROM billing_profit_pending pending USING locked
  WHERE pending.event_id = locked.event_id
  RETURNING pending.event_id
)
SELECT count(*)::bigint FROM removed
