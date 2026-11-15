, adjusted AS (
    SELECT pa.event_id, sum(pa.delta_micro_cny)::bigint AS adjusted
    FROM pricing_adjustments_cny pa JOIN scoped s ON s.event_id = pa.event_id
    GROUP BY pa.event_id
), latest AS (
    SELECT DISTINCT ON (pa.event_id) pa.event_id, pa.created_at, pa.lines
    FROM pricing_adjustments_cny pa JOIN scoped s ON s.event_id = pa.event_id
    ORDER BY pa.event_id, pa.created_at DESC, pa.run_id DESC
), visible AS (
    SELECT date_trunc('{unit}', pc.occurred_at, 'Asia/Shanghai') AS period_start,
           pc.requested_model_id AS model_id,
           CASE WHEN pc.status = 'charged' THEN pc.charge_micro_cny END AS charged,
           adj.adjusted, pc.estimated, {effective_lines} AS effective_lines
    FROM scoped pc
    LEFT JOIN adjusted adj ON adj.event_id = pc.event_id
    LEFT JOIN latest eff ON eff.event_id = pc.event_id
    WHERE (CASE WHEN pc.status = 'charged' OR adj.event_id IS NOT NULL THEN TRUE
          ELSE (
              SELECT CASE WHEN mi.projection_version = 1 THEN mi.json_is_final_attempt
                     ELSE (SELECT ue.event->'is_final_attempt' FROM usage_events ue
                           WHERE ue.event_id = pc.event_id) END
              FROM (VALUES (1)) anchor(n)
              LEFT JOIN usage_metric_inputs mi ON mi.event_id = pc.event_id
          ) = 'true'::jsonb END)
    OFFSET 0
), totals AS (
    SELECT period_start, model_id, count(*)::bigint AS requests,
           COALESCE(sum(charged), 0)::bigint + sum(COALESCE(adjusted, 0))::bigint AS amount,
           (count(*) FILTER (WHERE estimated))::bigint AS estimated
    FROM visible GROUP BY 1, 2
), dimensions AS (
    SELECT period_start, model_id, l->>'dimension' AS dimension,
           COALESCE(sum((l->>'quantity')::bigint), 0)::bigint AS quantity
    FROM visible CROSS JOIN LATERAL jsonb_array_elements(effective_lines) l
    GROUP BY 1, 2, 3
)
SELECT 0 AS kind, period_start, model_id, requests, amount, estimated,
       ''::text AS dimension, 0::bigint AS quantity
FROM totals
UNION ALL
SELECT 1, period_start, model_id, 0::bigint, 0::bigint, 0::bigint, dimension, quantity
FROM dimensions
ORDER BY kind, period_start, model_id, dimension
