-- 先锁源表再锁投影表；让旧触发器写完，冻结上界后到达的行由新触发器投影。
LOCK TABLE usage_events IN SHARE MODE;

-- NULL 表示旧行尚未补齐；缺字段、JSON null 以及非布尔值由提取规则归为 false。
ALTER TABLE usage_metric_inputs ADD COLUMN usage_estimated boolean;

-- 保留第一版基础投影及其写入围栏，滚动升级中的旧读者可继续使用同一行。
CREATE OR REPLACE FUNCTION usage_metric_inputs_refresh_v1()
RETURNS trigger
LANGUAGE plpgsql VOLATILE PARALLEL UNSAFE
SET search_path = pg_catalog
AS $refresh$
BEGIN
  EXECUTE format($write$
    WITH projected AS MATERIALIZED (
      SELECT n.event_id, 1 AS projection_version, p.*,
             COALESCE(n.event->'usage_estimated' = 'true'::jsonb, false) AS usage_estimated
      FROM metric_new_events n
      CROSS JOIN LATERAL %I.usage_metric_input_v1(n.event) p
    )
    INSERT INTO %I.usage_metric_inputs AS current (
        event_id, projection_version, u32_prompt_tokens,
        u32_completion_tokens, u32_reasoning_tokens, u32_total_tokens,
        u32_cached_prompt_tokens, u32_cache_read_tokens, u32_cache_creation_tokens,
        u32_cache_write_tokens, u32_status_code, u32_first_token_latency_ms,
        u32_downstream_latency_ms, u32_upstream_ttft_ms, u32_upstream_latency_ms,
        tpot, text_error_class, text_cache_status,
        json_is_final_attempt, json_cache_write_tokens, has_reasoning_tokens,
        has_cached_prompt_tokens, has_cache_read_tokens, has_cache_creation_tokens,
        has_cache_write_tokens, text_model_id, text_provider_key_id,
        text_inference_stage, text_native_terminal_status, text_upstream_dispatched,
        text_cache_usage_reported, json_upstream_dispatched, json_cache_usage_reported,
        has_cache_usage_reported, text_service_tier, usage_estimated
    )
    SELECT p.* FROM projected p
    LEFT JOIN %I.usage_metric_inputs old USING (event_id)
    WHERE old.event_id IS NULL OR (old.projection_version <= 1 AND ROW(old.*) IS DISTINCT FROM ROW(p.*))
    ON CONFLICT (event_id) DO UPDATE SET
        (projection_version, u32_prompt_tokens, u32_completion_tokens,
        u32_reasoning_tokens, u32_total_tokens, u32_cached_prompt_tokens,
        u32_cache_read_tokens, u32_cache_creation_tokens, u32_cache_write_tokens,
        u32_status_code, u32_first_token_latency_ms, u32_downstream_latency_ms,
        u32_upstream_ttft_ms, u32_upstream_latency_ms, tpot,
        text_error_class, text_cache_status, json_is_final_attempt,
        json_cache_write_tokens, has_reasoning_tokens, has_cached_prompt_tokens,
        has_cache_read_tokens, has_cache_creation_tokens, has_cache_write_tokens,
        text_model_id, text_provider_key_id, text_inference_stage,
        text_native_terminal_status, text_upstream_dispatched, text_cache_usage_reported,
        json_upstream_dispatched, json_cache_usage_reported, has_cache_usage_reported,
        text_service_tier, usage_estimated) =
        (EXCLUDED.projection_version, EXCLUDED.u32_prompt_tokens, EXCLUDED.u32_completion_tokens,
        EXCLUDED.u32_reasoning_tokens, EXCLUDED.u32_total_tokens, EXCLUDED.u32_cached_prompt_tokens,
        EXCLUDED.u32_cache_read_tokens, EXCLUDED.u32_cache_creation_tokens, EXCLUDED.u32_cache_write_tokens,
        EXCLUDED.u32_status_code, EXCLUDED.u32_first_token_latency_ms, EXCLUDED.u32_downstream_latency_ms,
        EXCLUDED.u32_upstream_ttft_ms, EXCLUDED.u32_upstream_latency_ms, EXCLUDED.tpot,
        EXCLUDED.text_error_class, EXCLUDED.text_cache_status, EXCLUDED.json_is_final_attempt,
        EXCLUDED.json_cache_write_tokens, EXCLUDED.has_reasoning_tokens, EXCLUDED.has_cached_prompt_tokens,
        EXCLUDED.has_cache_read_tokens, EXCLUDED.has_cache_creation_tokens, EXCLUDED.has_cache_write_tokens,
        EXCLUDED.text_model_id, EXCLUDED.text_provider_key_id, EXCLUDED.text_inference_stage,
        EXCLUDED.text_native_terminal_status, EXCLUDED.text_upstream_dispatched, EXCLUDED.text_cache_usage_reported,
        EXCLUDED.json_upstream_dispatched, EXCLUDED.json_cache_usage_reported, EXCLUDED.has_cache_usage_reported,
        EXCLUDED.text_service_tier, EXCLUDED.usage_estimated)
    WHERE current.projection_version <= 1 AND current IS DISTINCT FROM EXCLUDED
  $write$, TG_TABLE_SCHEMA, TG_TABLE_SCHEMA, TG_TABLE_SCHEMA);
  RETURN NULL;
END
$refresh$;

-- 独立游标防止滚动升级中的旧版回填任务越过尚未补齐标记的历史行。
INSERT INTO usage_metric_input_backfill (version, high_seq, complete)
SELECT 2, COALESCE(max(seq), 0), max(seq) IS NULL FROM usage_events;
