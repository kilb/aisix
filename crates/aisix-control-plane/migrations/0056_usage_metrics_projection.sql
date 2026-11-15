-- 统计只持久化事件导出的窄输入；权限、模型映射和钱账仍在读取时取权威表。
-- 安装只建空表、纯函数与事务内触发器，历史行由有界后台批次补齐。
CREATE TABLE usage_metric_inputs (
  event_id text PRIMARY KEY REFERENCES usage_events(event_id) ON DELETE CASCADE ON UPDATE CASCADE,
  projection_version integer NOT NULL,
  u32_prompt_tokens bigint,
  u32_completion_tokens bigint,
  u32_reasoning_tokens bigint,
  u32_total_tokens bigint,
  u32_cached_prompt_tokens bigint,
  u32_cache_read_tokens bigint,
  u32_cache_creation_tokens bigint,
  u32_cache_write_tokens bigint,
  u32_status_code bigint,
  u32_first_token_latency_ms bigint,
  u32_downstream_latency_ms bigint,
  u32_upstream_ttft_ms bigint,
  u32_upstream_latency_ms bigint,
  tpot double precision,
  text_error_class text,
  text_cache_status text,
  json_is_final_attempt jsonb,
  json_cache_write_tokens jsonb,
  has_reasoning_tokens boolean,
  has_cached_prompt_tokens boolean,
  has_cache_read_tokens boolean,
  has_cache_creation_tokens boolean,
  has_cache_write_tokens boolean,
  text_model_id text,
  text_provider_key_id text,
  text_inference_stage text,
  text_native_terminal_status text,
  text_upstream_dispatched text,
  text_cache_usage_reported text,
  json_upstream_dispatched jsonb,
  json_cache_usage_reported jsonb,
  has_cache_usage_reported boolean,
  text_service_tier text
);

-- STRICT 让已就绪行的 SQL NULL 回退参数不执行函数；字段原类型、显式 null 和存在位不合并。
CREATE FUNCTION usage_metric_input_v1(input_event jsonb)
RETURNS TABLE (
  u32_prompt_tokens bigint,
  u32_completion_tokens bigint,
  u32_reasoning_tokens bigint,
  u32_total_tokens bigint,
  u32_cached_prompt_tokens bigint,
  u32_cache_read_tokens bigint,
  u32_cache_creation_tokens bigint,
  u32_cache_write_tokens bigint,
  u32_status_code bigint,
  u32_first_token_latency_ms bigint,
  u32_downstream_latency_ms bigint,
  u32_upstream_ttft_ms bigint,
  u32_upstream_latency_ms bigint,
  tpot double precision,
  text_error_class text,
  text_cache_status text,
  json_is_final_attempt jsonb,
  json_cache_write_tokens jsonb,
  has_reasoning_tokens boolean,
  has_cached_prompt_tokens boolean,
  has_cache_read_tokens boolean,
  has_cache_creation_tokens boolean,
  has_cache_write_tokens boolean,
  text_model_id text,
  text_provider_key_id text,
  text_inference_stage text,
  text_native_terminal_status text,
  text_upstream_dispatched text,
  text_cache_usage_reported text,
  json_upstream_dispatched jsonb,
  json_cache_usage_reported jsonb,
  has_cache_usage_reported boolean,
  text_service_tier text
)
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE ROWS 1
SET search_path = pg_catalog
AS $projection$
  SELECT CASE WHEN jsonb_typeof(COALESCE(event_fields.event, 'null'::jsonb)->'prompt_tokens') = 'number' AND (COALESCE(event_fields.event, 'null'::jsonb)->>'prompt_tokens') ~ '^[0-9]{1,10}$' THEN CASE WHEN (COALESCE(event_fields.event, 'null'::jsonb)->>'prompt_tokens')::numeric <= 4294967295 THEN (COALESCE(event_fields.event, 'null'::jsonb)->>'prompt_tokens')::bigint END END AS u32_prompt_tokens,
    CASE WHEN jsonb_typeof(COALESCE(event_fields.event, 'null'::jsonb)->'completion_tokens') = 'number' AND (COALESCE(event_fields.event, 'null'::jsonb)->>'completion_tokens') ~ '^[0-9]{1,10}$' THEN CASE WHEN (COALESCE(event_fields.event, 'null'::jsonb)->>'completion_tokens')::numeric <= 4294967295 THEN (COALESCE(event_fields.event, 'null'::jsonb)->>'completion_tokens')::bigint END END AS u32_completion_tokens,
    CASE WHEN jsonb_typeof(COALESCE(event_fields.event, 'null'::jsonb)->'reasoning_tokens') = 'number' AND (COALESCE(event_fields.event, 'null'::jsonb)->>'reasoning_tokens') ~ '^[0-9]{1,10}$' THEN CASE WHEN (COALESCE(event_fields.event, 'null'::jsonb)->>'reasoning_tokens')::numeric <= 4294967295 THEN (COALESCE(event_fields.event, 'null'::jsonb)->>'reasoning_tokens')::bigint END END AS u32_reasoning_tokens,
    CASE WHEN jsonb_typeof(COALESCE(event_fields.event, 'null'::jsonb)->'total_tokens') = 'number' AND (COALESCE(event_fields.event, 'null'::jsonb)->>'total_tokens') ~ '^[0-9]{1,10}$' THEN CASE WHEN (COALESCE(event_fields.event, 'null'::jsonb)->>'total_tokens')::numeric <= 4294967295 THEN (COALESCE(event_fields.event, 'null'::jsonb)->>'total_tokens')::bigint END END AS u32_total_tokens,
    CASE WHEN jsonb_typeof(COALESCE(event_fields.event, 'null'::jsonb)->'cached_prompt_tokens') = 'number' AND (COALESCE(event_fields.event, 'null'::jsonb)->>'cached_prompt_tokens') ~ '^[0-9]{1,10}$' THEN CASE WHEN (COALESCE(event_fields.event, 'null'::jsonb)->>'cached_prompt_tokens')::numeric <= 4294967295 THEN (COALESCE(event_fields.event, 'null'::jsonb)->>'cached_prompt_tokens')::bigint END END AS u32_cached_prompt_tokens,
    CASE WHEN jsonb_typeof(COALESCE(event_fields.event, 'null'::jsonb)->'cache_read_tokens') = 'number' AND (COALESCE(event_fields.event, 'null'::jsonb)->>'cache_read_tokens') ~ '^[0-9]{1,10}$' THEN CASE WHEN (COALESCE(event_fields.event, 'null'::jsonb)->>'cache_read_tokens')::numeric <= 4294967295 THEN (COALESCE(event_fields.event, 'null'::jsonb)->>'cache_read_tokens')::bigint END END AS u32_cache_read_tokens,
    CASE WHEN jsonb_typeof(COALESCE(event_fields.event, 'null'::jsonb)->'cache_creation_tokens') = 'number' AND (COALESCE(event_fields.event, 'null'::jsonb)->>'cache_creation_tokens') ~ '^[0-9]{1,10}$' THEN CASE WHEN (COALESCE(event_fields.event, 'null'::jsonb)->>'cache_creation_tokens')::numeric <= 4294967295 THEN (COALESCE(event_fields.event, 'null'::jsonb)->>'cache_creation_tokens')::bigint END END AS u32_cache_creation_tokens,
    CASE WHEN jsonb_typeof(COALESCE(event_fields.event, 'null'::jsonb)->'cache_write_tokens') = 'number' AND (COALESCE(event_fields.event, 'null'::jsonb)->>'cache_write_tokens') ~ '^[0-9]{1,10}$' THEN CASE WHEN (COALESCE(event_fields.event, 'null'::jsonb)->>'cache_write_tokens')::numeric <= 4294967295 THEN (COALESCE(event_fields.event, 'null'::jsonb)->>'cache_write_tokens')::bigint END END AS u32_cache_write_tokens,
    CASE WHEN jsonb_typeof(COALESCE(event_fields.event, 'null'::jsonb)->'status_code') = 'number' AND (COALESCE(event_fields.event, 'null'::jsonb)->>'status_code') ~ '^[0-9]{1,10}$' THEN CASE WHEN (COALESCE(event_fields.event, 'null'::jsonb)->>'status_code')::numeric <= 4294967295 THEN (COALESCE(event_fields.event, 'null'::jsonb)->>'status_code')::bigint END END AS u32_status_code,
    CASE WHEN jsonb_typeof(COALESCE(event_fields.event, 'null'::jsonb)->'first_token_latency_ms') = 'number' AND (COALESCE(event_fields.event, 'null'::jsonb)->>'first_token_latency_ms') ~ '^[0-9]{1,10}$' THEN CASE WHEN (COALESCE(event_fields.event, 'null'::jsonb)->>'first_token_latency_ms')::numeric <= 4294967295 THEN (COALESCE(event_fields.event, 'null'::jsonb)->>'first_token_latency_ms')::bigint END END AS u32_first_token_latency_ms,
    CASE WHEN jsonb_typeof(COALESCE(event_fields.event, 'null'::jsonb)->'downstream_latency_ms') = 'number' AND (COALESCE(event_fields.event, 'null'::jsonb)->>'downstream_latency_ms') ~ '^[0-9]{1,10}$' THEN CASE WHEN (COALESCE(event_fields.event, 'null'::jsonb)->>'downstream_latency_ms')::numeric <= 4294967295 THEN (COALESCE(event_fields.event, 'null'::jsonb)->>'downstream_latency_ms')::bigint END END AS u32_downstream_latency_ms,
    CASE WHEN jsonb_typeof(COALESCE(event_fields.event, 'null'::jsonb)->'upstream_ttft_ms') = 'number' AND (COALESCE(event_fields.event, 'null'::jsonb)->>'upstream_ttft_ms') ~ '^[0-9]{1,10}$' THEN CASE WHEN (COALESCE(event_fields.event, 'null'::jsonb)->>'upstream_ttft_ms')::numeric <= 4294967295 THEN (COALESCE(event_fields.event, 'null'::jsonb)->>'upstream_ttft_ms')::bigint END END AS u32_upstream_ttft_ms,
    CASE WHEN jsonb_typeof(COALESCE(event_fields.event, 'null'::jsonb)->'upstream_latency_ms') = 'number' AND (COALESCE(event_fields.event, 'null'::jsonb)->>'upstream_latency_ms') ~ '^[0-9]{1,10}$' THEN CASE WHEN (COALESCE(event_fields.event, 'null'::jsonb)->>'upstream_latency_ms')::numeric <= 4294967295 THEN (COALESCE(event_fields.event, 'null'::jsonb)->>'upstream_latency_ms')::bigint END END AS u32_upstream_latency_ms,
    CASE WHEN jsonb_typeof(COALESCE(event_fields.event, 'null'::jsonb)->'upstream_tpot_ms') = 'number' THEN CASE WHEN (COALESCE(event_fields.event, 'null'::jsonb)->>'upstream_tpot_ms')::numeric BETWEEN 1e-100 AND 1e100 THEN (COALESCE(event_fields.event, 'null'::jsonb)->>'upstream_tpot_ms')::double precision END END AS tpot,
    COALESCE(event_fields.event, 'null'::jsonb)->>'error_class' AS text_error_class,
    COALESCE(event_fields.event, 'null'::jsonb)->>'cache_status' AS text_cache_status,
    COALESCE(event_fields.event, 'null'::jsonb)->'is_final_attempt' AS json_is_final_attempt,
    COALESCE(event_fields.event, 'null'::jsonb)->'cache_write_tokens' AS json_cache_write_tokens,
    COALESCE(event_fields.event, 'null'::jsonb) ? 'reasoning_tokens' AS has_reasoning_tokens,
    COALESCE(event_fields.event, 'null'::jsonb) ? 'cached_prompt_tokens' AS has_cached_prompt_tokens,
    COALESCE(event_fields.event, 'null'::jsonb) ? 'cache_read_tokens' AS has_cache_read_tokens,
    COALESCE(event_fields.event, 'null'::jsonb) ? 'cache_creation_tokens' AS has_cache_creation_tokens,
    COALESCE(event_fields.event, 'null'::jsonb) ? 'cache_write_tokens' AS has_cache_write_tokens,
    COALESCE(event_fields.event, 'null'::jsonb)->>'model_id' AS text_model_id,
    COALESCE(event_fields.event, 'null'::jsonb)->>'provider_key_id' AS text_provider_key_id,
    COALESCE(event_fields.event, 'null'::jsonb)->>'inference_stage' AS text_inference_stage,
    COALESCE(event_fields.event, 'null'::jsonb)->>'native_terminal_status' AS text_native_terminal_status,
    COALESCE(event_fields.event, 'null'::jsonb)->>'upstream_dispatched' AS text_upstream_dispatched,
    COALESCE(event_fields.event, 'null'::jsonb)->>'cache_usage_reported' AS text_cache_usage_reported,
    COALESCE(event_fields.event, 'null'::jsonb)->'upstream_dispatched' AS json_upstream_dispatched,
    COALESCE(event_fields.event, 'null'::jsonb)->'cache_usage_reported' AS json_cache_usage_reported,
    COALESCE(event_fields.event, 'null'::jsonb) ? 'cache_usage_reported' AS has_cache_usage_reported,
    COALESCE(event_fields.event, 'null'::jsonb)->>'service_tier' AS text_service_tier
  FROM jsonb_to_record(jsonb_build_object('event', input_event)) AS event_fields(event jsonb)
$projection$;

-- 一条摄入语句统一处理真实已存行；重复冲突没有新行，旧进程也走同一事务触发器。
CREATE FUNCTION usage_metric_inputs_refresh_v1()
RETURNS trigger
LANGUAGE plpgsql VOLATILE PARALLEL UNSAFE
SET search_path = pg_catalog
AS $refresh$
BEGIN
  EXECUTE format($write$
    INSERT INTO %I.usage_metric_inputs (
      event_id, projection_version, u32_prompt_tokens, u32_completion_tokens, u32_reasoning_tokens, u32_total_tokens, u32_cached_prompt_tokens, u32_cache_read_tokens, u32_cache_creation_tokens, u32_cache_write_tokens, u32_status_code, u32_first_token_latency_ms, u32_downstream_latency_ms, u32_upstream_ttft_ms, u32_upstream_latency_ms, tpot, text_error_class, text_cache_status, json_is_final_attempt, json_cache_write_tokens, has_reasoning_tokens, has_cached_prompt_tokens, has_cache_read_tokens, has_cache_creation_tokens, has_cache_write_tokens, text_model_id, text_provider_key_id, text_inference_stage, text_native_terminal_status, text_upstream_dispatched, text_cache_usage_reported, json_upstream_dispatched, json_cache_usage_reported, has_cache_usage_reported, text_service_tier
    )
    SELECT n.event_id, 1, p.u32_prompt_tokens, p.u32_completion_tokens, p.u32_reasoning_tokens, p.u32_total_tokens, p.u32_cached_prompt_tokens, p.u32_cache_read_tokens, p.u32_cache_creation_tokens, p.u32_cache_write_tokens, p.u32_status_code, p.u32_first_token_latency_ms, p.u32_downstream_latency_ms, p.u32_upstream_ttft_ms, p.u32_upstream_latency_ms, p.tpot, p.text_error_class, p.text_cache_status, p.json_is_final_attempt, p.json_cache_write_tokens, p.has_reasoning_tokens, p.has_cached_prompt_tokens, p.has_cache_read_tokens, p.has_cache_creation_tokens, p.has_cache_write_tokens, p.text_model_id, p.text_provider_key_id, p.text_inference_stage, p.text_native_terminal_status, p.text_upstream_dispatched, p.text_cache_usage_reported, p.json_upstream_dispatched, p.json_cache_usage_reported, p.has_cache_usage_reported, p.text_service_tier
    FROM metric_new_events n
    CROSS JOIN LATERAL %I.usage_metric_input_v1(n.event) p
    LEFT JOIN %I.usage_metric_inputs stored ON stored.event_id = n.event_id
    WHERE stored.event_id IS NULL OR stored.projection_version < 1
      OR (stored.projection_version = 1 AND ROW(stored.u32_prompt_tokens, stored.u32_completion_tokens, stored.u32_reasoning_tokens, stored.u32_total_tokens, stored.u32_cached_prompt_tokens, stored.u32_cache_read_tokens, stored.u32_cache_creation_tokens, stored.u32_cache_write_tokens, stored.u32_status_code, stored.u32_first_token_latency_ms, stored.u32_downstream_latency_ms, stored.u32_upstream_ttft_ms, stored.u32_upstream_latency_ms, stored.tpot, stored.text_error_class, stored.text_cache_status, stored.json_is_final_attempt, stored.json_cache_write_tokens, stored.has_reasoning_tokens, stored.has_cached_prompt_tokens, stored.has_cache_read_tokens, stored.has_cache_creation_tokens, stored.has_cache_write_tokens, stored.text_model_id, stored.text_provider_key_id, stored.text_inference_stage, stored.text_native_terminal_status, stored.text_upstream_dispatched, stored.text_cache_usage_reported, stored.json_upstream_dispatched, stored.json_cache_usage_reported, stored.has_cache_usage_reported, stored.text_service_tier)
          IS DISTINCT FROM ROW(p.u32_prompt_tokens, p.u32_completion_tokens, p.u32_reasoning_tokens, p.u32_total_tokens, p.u32_cached_prompt_tokens, p.u32_cache_read_tokens, p.u32_cache_creation_tokens, p.u32_cache_write_tokens, p.u32_status_code, p.u32_first_token_latency_ms, p.u32_downstream_latency_ms, p.u32_upstream_ttft_ms, p.u32_upstream_latency_ms, p.tpot, p.text_error_class, p.text_cache_status, p.json_is_final_attempt, p.json_cache_write_tokens, p.has_reasoning_tokens, p.has_cached_prompt_tokens, p.has_cache_read_tokens, p.has_cache_creation_tokens, p.has_cache_write_tokens, p.text_model_id, p.text_provider_key_id, p.text_inference_stage, p.text_native_terminal_status, p.text_upstream_dispatched, p.text_cache_usage_reported, p.json_upstream_dispatched, p.json_cache_usage_reported, p.has_cache_usage_reported, p.text_service_tier))
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
    WHERE usage_metric_inputs.projection_version <= EXCLUDED.projection_version
      AND ROW(usage_metric_inputs.projection_version, usage_metric_inputs.u32_prompt_tokens, usage_metric_inputs.u32_completion_tokens, usage_metric_inputs.u32_reasoning_tokens, usage_metric_inputs.u32_total_tokens, usage_metric_inputs.u32_cached_prompt_tokens, usage_metric_inputs.u32_cache_read_tokens, usage_metric_inputs.u32_cache_creation_tokens, usage_metric_inputs.u32_cache_write_tokens, usage_metric_inputs.u32_status_code, usage_metric_inputs.u32_first_token_latency_ms, usage_metric_inputs.u32_downstream_latency_ms, usage_metric_inputs.u32_upstream_ttft_ms, usage_metric_inputs.u32_upstream_latency_ms, usage_metric_inputs.tpot, usage_metric_inputs.text_error_class, usage_metric_inputs.text_cache_status, usage_metric_inputs.json_is_final_attempt, usage_metric_inputs.json_cache_write_tokens, usage_metric_inputs.has_reasoning_tokens, usage_metric_inputs.has_cached_prompt_tokens, usage_metric_inputs.has_cache_read_tokens, usage_metric_inputs.has_cache_creation_tokens, usage_metric_inputs.has_cache_write_tokens, usage_metric_inputs.text_model_id, usage_metric_inputs.text_provider_key_id, usage_metric_inputs.text_inference_stage, usage_metric_inputs.text_native_terminal_status, usage_metric_inputs.text_upstream_dispatched, usage_metric_inputs.text_cache_usage_reported, usage_metric_inputs.json_upstream_dispatched, usage_metric_inputs.json_cache_usage_reported, usage_metric_inputs.has_cache_usage_reported, usage_metric_inputs.text_service_tier)
          IS DISTINCT FROM ROW(EXCLUDED.projection_version, EXCLUDED.u32_prompt_tokens, EXCLUDED.u32_completion_tokens, EXCLUDED.u32_reasoning_tokens, EXCLUDED.u32_total_tokens, EXCLUDED.u32_cached_prompt_tokens, EXCLUDED.u32_cache_read_tokens, EXCLUDED.u32_cache_creation_tokens, EXCLUDED.u32_cache_write_tokens, EXCLUDED.u32_status_code, EXCLUDED.u32_first_token_latency_ms, EXCLUDED.u32_downstream_latency_ms, EXCLUDED.u32_upstream_ttft_ms, EXCLUDED.u32_upstream_latency_ms, EXCLUDED.tpot, EXCLUDED.text_error_class, EXCLUDED.text_cache_status, EXCLUDED.json_is_final_attempt, EXCLUDED.json_cache_write_tokens, EXCLUDED.has_reasoning_tokens, EXCLUDED.has_cached_prompt_tokens, EXCLUDED.has_cache_read_tokens, EXCLUDED.has_cache_creation_tokens, EXCLUDED.has_cache_write_tokens, EXCLUDED.text_model_id, EXCLUDED.text_provider_key_id, EXCLUDED.text_inference_stage, EXCLUDED.text_native_terminal_status, EXCLUDED.text_upstream_dispatched, EXCLUDED.text_cache_usage_reported, EXCLUDED.json_upstream_dispatched, EXCLUDED.json_cache_usage_reported, EXCLUDED.has_cache_usage_reported, EXCLUDED.text_service_tier)
  $write$, TG_TABLE_SCHEMA, TG_TABLE_SCHEMA, TG_TABLE_SCHEMA);
  RETURN NULL;
END
$refresh$;

CREATE TRIGGER usage_metric_inputs_insert
AFTER INSERT ON usage_events
REFERENCING NEW TABLE AS metric_new_events
FOR EACH STATEMENT EXECUTE FUNCTION usage_metric_inputs_refresh_v1();

-- 过渡表需要语句级 UPDATE；仅 IO 元数据变化且投影相同的行不会更新窄表。
CREATE TRIGGER usage_metric_inputs_update
AFTER UPDATE ON usage_events
REFERENCING NEW TABLE AS metric_new_events
FOR EACH STATEMENT EXECUTE FUNCTION usage_metric_inputs_refresh_v1();

-- 游标只推进到已拿锁候选的连续前缀，不越过 SKIP LOCKED 的洞。
CREATE TABLE usage_metric_input_backfill (
  version integer PRIMARY KEY,
  after_seq bigint NOT NULL DEFAULT 0,
  high_seq bigint,
  complete boolean NOT NULL DEFAULT false
);
INSERT INTO usage_metric_input_backfill(version) VALUES (1);
