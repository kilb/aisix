-- 请求级只读查询先按请求、网关和追踪身份找尝试，再核对CP自己的租户/成员/密钥。
-- 没有完整身份的历史事件独立展示，不按调用方可复用的request_id猜测归并。
CREATE INDEX usage_events_request_group
  ON usage_events (request_id, dp_id, (event->>'trace_id')) INCLUDE (event_id)
  WHERE request_id IS NOT NULL AND request_id <> ''
    AND jsonb_typeof(event->'trace_id') = 'string'
    AND event->>'trace_id' ~ '^[0-9a-f]{32}$'
    AND event->>'trace_id' <> '00000000000000000000000000000000';

-- 与调用筛选的CASE完全一致；hash等值索引不会因旧遥测错误字符串过长拒绝写入。
CREATE INDEX usage_events_call_error_class ON usage_events USING hash
  ((CASE WHEN jsonb_typeof(event->'error_class') = 'string'
    THEN event->>'error_class' ELSE '' END));
