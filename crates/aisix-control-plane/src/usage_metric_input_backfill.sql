-- 候选窗口包含已就绪行，保证每次最多检查一小批，而不是反复全表找洞。
WITH candidates AS MATERIALIZED (
  SELECT seq, event_id FROM usage_events
  WHERE seq > $1 AND seq <= $2 ORDER BY seq LIMIT $3
), locked AS MATERIALIZED (
  SELECT ue.seq, ue.event_id, ue.event
  FROM usage_events ue JOIN candidates c ON c.event_id = ue.event_id
  ORDER BY ue.seq FOR NO KEY UPDATE OF ue SKIP LOCKED
), missing AS MATERIALIZED (
  SELECT l.event_id, l.event FROM locked l
  LEFT JOIN usage_metric_inputs current ON current.event_id = l.event_id
  WHERE current.event_id IS NULL OR current.projection_version < 1
), written AS (
  INSERT INTO usage_metric_inputs (event_id, projection_version, u32_prompt_tokens, u32_completion_tokens, u32_reasoning_tokens, u32_total_tokens, u32_cached_prompt_tokens, u32_cache_read_tokens, u32_cache_creation_tokens, u32_cache_write_tokens, u32_status_code, u32_first_token_latency_ms, u32_downstream_latency_ms, u32_upstream_ttft_ms, u32_upstream_latency_ms, tpot, text_error_class, text_cache_status, json_is_final_attempt, json_cache_write_tokens, has_reasoning_tokens, has_cached_prompt_tokens, has_cache_read_tokens, has_cache_creation_tokens, has_cache_write_tokens, text_model_id, text_provider_key_id, text_inference_stage, text_native_terminal_status, text_upstream_dispatched, text_cache_usage_reported, json_upstream_dispatched, json_cache_usage_reported, has_cache_usage_reported, text_service_tier)
  SELECT m.event_id, 1, p.u32_prompt_tokens, p.u32_completion_tokens, p.u32_reasoning_tokens, p.u32_total_tokens, p.u32_cached_prompt_tokens, p.u32_cache_read_tokens, p.u32_cache_creation_tokens, p.u32_cache_write_tokens, p.u32_status_code, p.u32_first_token_latency_ms, p.u32_downstream_latency_ms, p.u32_upstream_ttft_ms, p.u32_upstream_latency_ms, p.tpot, p.text_error_class, p.text_cache_status, p.json_is_final_attempt, p.json_cache_write_tokens, p.has_reasoning_tokens, p.has_cached_prompt_tokens, p.has_cache_read_tokens, p.has_cache_creation_tokens, p.has_cache_write_tokens, p.text_model_id, p.text_provider_key_id, p.text_inference_stage, p.text_native_terminal_status, p.text_upstream_dispatched, p.text_cache_usage_reported, p.json_upstream_dispatched, p.json_cache_usage_reported, p.has_cache_usage_reported, p.text_service_tier
  FROM missing m CROSS JOIN LATERAL usage_metric_input_v1(m.event) p
  ON CONFLICT (event_id) DO UPDATE SET
    projection_version = EXCLUDED.projection_version,
    u32_prompt_tokens = EXCLUDED.u32_prompt_tokens,
    u32_completion_tokens = EXCLUDED.u32_completion_tokens,
    u32_reasoning_tokens = EXCLUDED.u32_reasoning_tokens,
    u32_total_tokens = EXCLUDED.u32_total_tokens,
    u32_cached_prompt_tokens = EXCLUDED.u32_cached_prompt_tokens,
    u32_cache_read_tokens = EXCLUDED.u32_cache_read_tokens,
    u32_cache_creation_tokens = EXCLUDED.u32_cache_creation_tokens,
    u32_cache_write_tokens = EXCLUDED.u32_cache_write_tokens,
    u32_status_code = EXCLUDED.u32_status_code,
    u32_first_token_latency_ms = EXCLUDED.u32_first_token_latency_ms,
    u32_downstream_latency_ms = EXCLUDED.u32_downstream_latency_ms,
    u32_upstream_ttft_ms = EXCLUDED.u32_upstream_ttft_ms,
    u32_upstream_latency_ms = EXCLUDED.u32_upstream_latency_ms,
    tpot = EXCLUDED.tpot,
    text_error_class = EXCLUDED.text_error_class,
    text_cache_status = EXCLUDED.text_cache_status,
    json_is_final_attempt = EXCLUDED.json_is_final_attempt,
    json_cache_write_tokens = EXCLUDED.json_cache_write_tokens,
    has_reasoning_tokens = EXCLUDED.has_reasoning_tokens,
    has_cached_prompt_tokens = EXCLUDED.has_cached_prompt_tokens,
    has_cache_read_tokens = EXCLUDED.has_cache_read_tokens,
    has_cache_creation_tokens = EXCLUDED.has_cache_creation_tokens,
    has_cache_write_tokens = EXCLUDED.has_cache_write_tokens,
    text_model_id = EXCLUDED.text_model_id,
    text_provider_key_id = EXCLUDED.text_provider_key_id,
    text_inference_stage = EXCLUDED.text_inference_stage,
    text_native_terminal_status = EXCLUDED.text_native_terminal_status,
    text_upstream_dispatched = EXCLUDED.text_upstream_dispatched,
    text_cache_usage_reported = EXCLUDED.text_cache_usage_reported,
    json_upstream_dispatched = EXCLUDED.json_upstream_dispatched,
    json_cache_usage_reported = EXCLUDED.json_cache_usage_reported,
    has_cache_usage_reported = EXCLUDED.has_cache_usage_reported,
    text_service_tier = EXCLUDED.text_service_tier
  WHERE usage_metric_inputs.projection_version < EXCLUDED.projection_version
  RETURNING event_id
), gap AS (
  SELECT min(c.seq) AS seq FROM candidates c
  LEFT JOIN locked l ON l.event_id = c.event_id WHERE l.event_id IS NULL
)
SELECT COALESCE((SELECT max(c.seq) FROM candidates c CROSS JOIN gap g
                WHERE g.seq IS NULL OR c.seq < g.seq), $1)::bigint,
       (SELECT count(*)::bigint FROM candidates),
       (SELECT count(*)::bigint FROM locked),
       (SELECT count(*)::bigint FROM written)
