WITH pending_fresh AS MATERIALIZED (
  SELECT * FROM billing_profit_current($5::text[])
  WHERE cardinality($5::text[]) > 0
), parts AS (
  SELECT charged_micro_cny, adjusted_micro_cny, known_cost_micro_cny, charged_with_cost_micro_cny, rows_with_cost, rows_without_cost, unpriced_rows, rows FROM billing_profit_daily
  WHERE $1::timestamptz IS NULL OR (day >= ($3::timestamptz AT TIME ZONE 'UTC')::date
                                  AND day < ($4::timestamptz AT TIME ZONE 'UTC')::date)
  UNION ALL
  SELECT f.charged_micro_cny - COALESCE(old.charged_micro_cny, 0), f.adjusted_micro_cny - COALESCE(old.adjusted_micro_cny, 0), f.known_cost_micro_cny - COALESCE(old.known_cost_micro_cny, 0), f.charged_with_cost_micro_cny - COALESCE(old.charged_with_cost_micro_cny, 0), f.rows_with_cost - COALESCE(old.rows_with_cost, 0), f.rows_without_cost - COALESCE(old.rows_without_cost, 0), f.unpriced_rows - COALESCE(old.unpriced_rows, 0), f.rows - COALESCE(old.rows, 0)
  FROM pending_fresh f LEFT JOIN billing_profit_rows old USING(event_id)
  WHERE $1::timestamptz IS NULL OR (f.occurred_at >= $3 AND f.occurred_at < $4)
  UNION ALL
  SELECT edge.charged_micro_cny, edge.adjusted_micro_cny, edge.known_cost_micro_cny, edge.charged_with_cost_micro_cny, edge.rows_with_cost, edge.rows_without_cost, edge.unpriced_rows, edge.rows
  FROM billing_profit_current(ARRAY(
    SELECT event_id FROM pricing_charges_cny
    WHERE $1::timestamptz IS NOT NULL AND occurred_at >= $1 AND occurred_at < LEAST($2, $3)
    UNION ALL
    SELECT event_id FROM pricing_charges_cny
    WHERE $1::timestamptz IS NOT NULL AND occurred_at >= GREATEST($1, $3, $4) AND occurred_at < $2
  )) edge WHERE $1::timestamptz IS NOT NULL
)
SELECT COALESCE(sum(charged_micro_cny)::numeric, 0)::text AS charged_micro_cny,
       COALESCE(sum(adjusted_micro_cny)::numeric, 0)::text AS adjusted_micro_cny,
       COALESCE(sum(known_cost_micro_cny)::numeric, 0)::text AS known_cost_micro_cny,
       COALESCE(sum(charged_with_cost_micro_cny)::numeric, 0)::text AS charged_with_cost_micro_cny,
       COALESCE(sum(rows_with_cost)::numeric, 0)::text AS rows_with_cost,
       COALESCE(sum(rows_without_cost)::numeric, 0)::text AS rows_without_cost,
       COALESCE(sum(unpriced_rows)::numeric, 0)::text AS unpriced_rows,
       COALESCE(sum(rows)::numeric, 0)::text AS rows FROM parts
