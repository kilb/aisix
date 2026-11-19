WITH config AS MATERIALIZED (
  SELECT s.tenant_id, s.version, s.low_balance_micro_cny, s.budget_percent, s.spend_spike_percent,
         s.email, s.sealed_webhook, s.spend_checked_at, statement_timestamp() AS measured_at,
         date_trunc('day', statement_timestamp(), 'UTC') AS today
  FROM billing_alert_settings s JOIN tenants t ON t.id = s.tenant_id
  WHERE s.tenant_id = $1 AND t.status = 'active'
  FOR NO KEY UPDATE OF s SKIP LOCKED
), channels AS MATERIALIZED (
  SELECT 'webhook' AS channel FROM config c WHERE c.sealed_webhook IS NOT NULL
  UNION ALL
  SELECT 'email' FROM config c WHERE $2 AND EXISTS (
    SELECT 1 FROM members m WHERE m.tenant_id = c.tenant_id AND m.email = c.email
      AND m.role IN ('owner', 'admin') AND NOT m.disabled AND m.removed_at IS NULL
  )
), eligible AS MATERIALIZED (
  SELECT c.* FROM config c WHERE EXISTS (SELECT 1 FROM channels)
), spend_due AS MATERIALIZED (
  SELECT c.* FROM eligible c WHERE c.spend_spike_percent IS NOT NULL
    AND (c.spend_checked_at IS NULL OR c.spend_checked_at <= c.measured_at - interval '5 minutes')
), spend_checked AS (
  UPDATE billing_alert_settings s SET spend_checked_at = c.measured_at
  FROM spend_due c WHERE s.tenant_id = c.tenant_id
), signals AS MATERIALIZED (
  SELECT 'low_balance' AS kind, account.available AS observed_micro_cny,
         c.low_balance_micro_cny AS threshold_micro_cny, NULL::timestamptz AS window_start
  FROM eligible c
  JOIN billing_accounts a ON a.tenant_id = c.tenant_id
  CROSS JOIN LATERAL (
    SELECT LEAST(9223372036854775807, a.balance_micro_cny::numeric + a.credit_limit_micro_cny)::bigint AS available
  ) account
  WHERE account.available <= c.low_balance_micro_cny
  UNION ALL
  SELECT 'budget_threshold', q.spent_micro_cny, q.threshold_micro_cny, q.window_start
  FROM eligible c
  CROSS JOIN LATERAL (
    SELECT s.spent_micro_cny, s.period_start AS window_start,
           ceil(b.limit_micro_cny::numeric * c.budget_percent / 100)::bigint AS threshold_micro_cny
    FROM billing_budgets_cny b
    JOIN billing_budget_spend_cny s ON s.budget_id = b.id
      AND s.period_start = COALESCE(b.counted_from, date_trunc(b.period, c.measured_at, 'UTC'))
    WHERE c.budget_percent IS NOT NULL AND b.tenant_id = c.tenant_id
      AND b.scope = 'team' AND b.deleted_at IS NULL
      AND s.spent_micro_cny::numeric * 100 >= b.limit_micro_cny::numeric * c.budget_percent
    ORDER BY s.spent_micro_cny::numeric / b.limit_micro_cny DESC, b.id
    LIMIT 1
  ) q
  UNION ALL
  SELECT 'spend_spike', LEAST(recent.today_spend, 9223372036854775807)::bigint,
         LEAST(ceil(recent.prior_spend * c.spend_spike_percent / 700), 9223372036854775807)::bigint,
         c.today
  FROM spend_due c
  CROSS JOIN LATERAL (
    SELECT COALESCE(sum(-l.amount_micro_cny::numeric) FILTER (WHERE l.posted_at >= c.today), 0)::numeric AS today_spend,
           COALESCE(sum(-l.amount_micro_cny::numeric) FILTER (WHERE l.posted_at < c.today), 0)::numeric AS prior_spend
    FROM billing_ledger_cny l
    WHERE $3 AND l.tenant_id = c.tenant_id AND l.kind = 'usage'
      AND l.posted_at >= c.today - interval '168 hours' AND l.posted_at <= c.measured_at
  ) recent
  WHERE $3 AND recent.prior_spend > 0 AND recent.today_spend * 700 >= recent.prior_spend * c.spend_spike_percent
), reset AS (
  UPDATE billing_alert_states s SET breached = false
  FROM eligible c
  WHERE s.tenant_id = c.tenant_id AND s.breached
    AND (s.kind <> 'spend_spike' OR ($3 AND EXISTS (SELECT 1 FROM spend_due)))
    AND NOT EXISTS (SELECT 1 FROM signals x WHERE x.kind = s.kind)
), eligible_attempts AS (
  SELECT c.tenant_id, c.version, s.kind, h.channel, s.window_start, c.measured_at,
         s.observed_micro_cny, s.threshold_micro_cny,
         previous.delivery_id, previous.attempts, previous.last_claimed_at,
         previous.observed_micro_cny AS previous_observed, previous.threshold_micro_cny AS previous_threshold,
         COALESCE(previous.version = c.version AND previous.breached
           AND previous.window_start IS NOT DISTINCT FROM s.window_start
           AND previous.outcome IN ('claimed', 'failed') AND previous.attempts < 3
           AND previous.next_attempt_at <= c.measured_at
           AND previous.last_claimed_at > c.measured_at - interval '30 minutes', false) AS retry,
         (previous.delivery_id IS NULL
          OR (previous.outcome = 'skipped' AND previous.attempts = 1)
          OR (previous.last_claimed_at <= c.measured_at - interval '24 hours'
           AND (NOT previous.breached OR previous.window_start IS DISTINCT FROM s.window_start
                OR (previous.version <> c.version AND previous.outcome <> 'sent')))) AS fresh
  FROM eligible c CROSS JOIN signals s CROSS JOIN channels h
  LEFT JOIN billing_alert_states previous
    ON previous.tenant_id = c.tenant_id AND previous.kind = s.kind AND previous.channel = h.channel
), reserved AS (
  INSERT INTO billing_alert_states
    (tenant_id, kind, channel, breached, window_start, last_claimed_at, delivery_id, version,
     attempts, next_attempt_at, observed_micro_cny, threshold_micro_cny, outcome)
  SELECT tenant_id, kind, channel, true, window_start,
         CASE WHEN retry THEN last_claimed_at ELSE measured_at END,
         CASE WHEN retry THEN delivery_id ELSE gen_random_uuid() END, version,
         CASE WHEN retry THEN attempts + 1 ELSE 1 END, measured_at + interval '2 minutes',
         CASE WHEN retry THEN previous_observed ELSE observed_micro_cny END,
         CASE WHEN retry THEN previous_threshold ELSE threshold_micro_cny END,
         'claimed'
  FROM eligible_attempts WHERE retry OR fresh
  ON CONFLICT (tenant_id, kind, channel) DO UPDATE SET
    breached = EXCLUDED.breached,
    window_start = EXCLUDED.window_start,
    last_claimed_at = EXCLUDED.last_claimed_at,
    delivery_id = EXCLUDED.delivery_id,
    version = EXCLUDED.version,
    attempts = EXCLUDED.attempts,
    next_attempt_at = EXCLUDED.next_attempt_at,
    observed_micro_cny = EXCLUDED.observed_micro_cny,
    threshold_micro_cny = EXCLUDED.threshold_micro_cny,
    outcome = EXCLUDED.outcome
  WHERE (billing_alert_states.delivery_id <> EXCLUDED.delivery_id
         AND billing_alert_states.outcome <> 'claimed')
    OR (billing_alert_states.delivery_id <> EXCLUDED.delivery_id
        AND billing_alert_states.next_attempt_at <= statement_timestamp())
    OR (billing_alert_states.delivery_id = EXCLUDED.delivery_id
        AND billing_alert_states.outcome IN ('claimed', 'failed')
        AND billing_alert_states.attempts = EXCLUDED.attempts - 1
        AND billing_alert_states.next_attempt_at <= statement_timestamp())
  RETURNING kind, channel, delivery_id, attempts,
            last_claimed_at AS measured_at, observed_micro_cny, threshold_micro_cny, window_start
)
SELECT kind, channel, delivery_id, attempts, measured_at,
       observed_micro_cny, threshold_micro_cny, window_start
FROM reserved
