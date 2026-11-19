-- 调用者已占用普通投影的写锁，账号游标独立记录回放进度。
WITH cursor AS MATERIALIZED (
  SELECT after_seq, high_seq FROM billing_profit_backfill WHERE version = 2 AND NOT complete
  FOR UPDATE SKIP LOCKED
), candidates AS MATERIALIZED (
  SELECT page.seq, page.event_id FROM cursor c
  CROSS JOIN LATERAL (
    SELECT seq, event_id FROM pricing_charges_cny WHERE seq > c.after_seq AND seq <= c.high_seq
    ORDER BY seq LIMIT 256
  ) page
), locked AS MATERIALIZED (
  SELECT snapshot.event_id FROM billing_profit_rows snapshot JOIN candidates USING(event_id)
  WHERE snapshot.provider_key_id IS NULL ORDER BY snapshot.event_id FOR UPDATE OF snapshot
), replayed AS (
  UPDATE billing_profit_rows snapshot SET provider_key_id = NULL FROM locked
  WHERE snapshot.event_id = locked.event_id
), progress AS (
  SELECT COALESCE(max(seq), 0) AS last_seq, count(*) < 256 AS complete FROM candidates
), advanced AS (
  UPDATE billing_profit_backfill state SET
    after_seq = CASE WHEN p.complete THEN c.high_seq ELSE p.last_seq END,
    complete = p.complete
  FROM cursor c CROSS JOIN progress p WHERE state.version = 2
  RETURNING state.complete
)
SELECT COALESCE((SELECT complete FROM advanced), (SELECT complete FROM billing_profit_backfill WHERE version = 2))
