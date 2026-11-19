WITH pending_fresh AS MATERIALIZED (
  SELECT * FROM billing_profit_current($5::text[])
  WHERE cardinality($5::text[]) > 0
), pending_diff AS (
  SELECT f.event_id, f.charged_micro_cny - COALESCE(old.charged_micro_cny, 0) AS charged_micro_cny,
         f.adjusted_micro_cny - COALESCE(old.adjusted_micro_cny, 0) AS adjusted_micro_cny,
         f.known_cost_micro_cny - COALESCE(old.known_cost_micro_cny, 0) AS known_cost_micro_cny,
         f.charged_with_cost_micro_cny - COALESCE(old.charged_with_cost_micro_cny, 0) AS charged_with_cost_micro_cny,
         f.rows_with_cost - COALESCE(old.rows_with_cost, 0) AS rows_with_cost,
         f.rows_without_cost - COALESCE(old.rows_without_cost, 0) AS rows_without_cost,
         f.unpriced_rows - COALESCE(old.unpriced_rows, 0) AS unpriced_rows,
         f.rows - COALESCE(old.rows, 0) AS rows
  FROM pending_fresh f LEFT JOIN billing_profit_rows old USING(event_id)
  WHERE $1::timestamptz IS NULL OR (f.occurred_at >= $3 AND f.occurred_at < $4)
), edges AS (
  SELECT edge.*
  FROM billing_profit_current(ARRAY(
    SELECT event_id FROM pricing_charges_cny
    WHERE $1::timestamptz IS NOT NULL AND occurred_at >= $1 AND occurred_at < LEAST($2, $3)
    UNION ALL
    SELECT event_id FROM pricing_charges_cny
    WHERE $1::timestamptz IS NOT NULL AND occurred_at >= GREATEST($1, $3, $4) AND occurred_at < $2
  )) edge
  WHERE $1::timestamptz IS NOT NULL
), raw_parts AS (
  SELECT event_id, charged_micro_cny, adjusted_micro_cny, known_cost_micro_cny, charged_with_cost_micro_cny, rows_with_cost, rows_without_cost, unpriced_rows, rows FROM pending_diff
  UNION ALL
  SELECT event_id, charged_micro_cny, adjusted_micro_cny, known_cost_micro_cny, charged_with_cost_micro_cny, rows_with_cost, rows_without_cost, unpriced_rows, rows FROM edges
), attributed AS (
  SELECT p.*, billing_profit_account_id(CASE
      WHEN $6::boolean AND m.event_id IS NULL
        THEN (SELECT u.event->>'provider_key_id' FROM usage_events u WHERE u.event_id = p.event_id)
      ELSE m.text_provider_key_id END) AS provider_key_id
  FROM raw_parts p LEFT JOIN LATERAL (
    SELECT event_id, text_provider_key_id FROM usage_metric_inputs
    WHERE $6::boolean AND event_id = p.event_id AND projection_version = 1
    LIMIT 1
  ) m ON true
), parts AS (
  SELECT NULL::uuid AS provider_key_id, charged_micro_cny, adjusted_micro_cny, known_cost_micro_cny, charged_with_cost_micro_cny, rows_with_cost, rows_without_cost, unpriced_rows, rows FROM billing_profit_daily
  WHERE $1::timestamptz IS NULL OR (day >= ($3::timestamptz AT TIME ZONE 'UTC')::date
                                  AND day < ($4::timestamptz AT TIME ZONE 'UTC')::date)
  UNION ALL
  SELECT provider_key_id, charged_micro_cny, adjusted_micro_cny, known_cost_micro_cny, charged_with_cost_micro_cny, rows_with_cost, rows_without_cost, unpriced_rows, rows FROM billing_profit_account_daily
  WHERE $6::boolean AND ($1::timestamptz IS NULL OR (day >= ($3::timestamptz AT TIME ZONE 'UTC')::date
                                  AND day < ($4::timestamptz AT TIME ZONE 'UTC')::date))
  UNION ALL
  SELECT scope.provider_key_id, p.charged_micro_cny, p.adjusted_micro_cny, p.known_cost_micro_cny, p.charged_with_cost_micro_cny, p.rows_with_cost, p.rows_without_cost, p.unpriced_rows, p.rows
  FROM attributed p CROSS JOIN LATERAL (
    SELECT NULL::uuid AS provider_key_id
    UNION ALL SELECT p.provider_key_id WHERE p.provider_key_id IS NOT NULL
  ) scope
  UNION ALL
  SELECT NULL::uuid, 0, 0, 0, 0, 0, 0, 0, 0
), sums AS (
  SELECT provider_key_id, sum(charged_micro_cny)::numeric AS charged, sum(adjusted_micro_cny)::numeric AS adjusted,
         sum(known_cost_micro_cny)::numeric AS known_cost, sum(charged_with_cost_micro_cny)::numeric AS charged_with_cost,
         sum(rows_with_cost)::numeric AS rows_with_cost, sum(rows_without_cost)::numeric AS rows_without_cost,
         sum(unpriced_rows)::numeric AS unpriced_rows, sum(rows)::numeric AS rows
  FROM parts GROUP BY provider_key_id
)
SELECT s.provider_key_id, r.doc->>'display_name' AS provider_key_display_name,
       s.charged::text AS charged_micro_cny, s.adjusted::text AS adjusted_micro_cny,
       s.known_cost::text AS known_cost_micro_cny, s.charged_with_cost::text AS charged_with_cost_micro_cny,
       s.rows_with_cost::text, s.rows_without_cost::text, s.unpriced_rows::text, s.rows::text
FROM sums s LEFT JOIN gw_resources r ON r.kind = 'provider_keys' AND r.id = s.provider_key_id AND NOT r.deleted
ORDER BY s.provider_key_id IS NOT NULL,
         CASE WHEN s.rows_without_cost = 0 AND s.unpriced_rows = 0 THEN s.charged - s.known_cost END DESC NULLS LAST,
         s.charged DESC, s.provider_key_id
