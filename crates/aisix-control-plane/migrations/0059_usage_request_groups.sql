-- 仅非空错误需要等值索引；成功行不生成索引条目，避免额外摄入开销。
-- 表达式与调用筛选的CASE一致；hash不会因旧遥测错误字符串过长拒绝写入。
CREATE INDEX usage_events_call_error_class ON usage_events USING hash
  ((CASE WHEN jsonb_typeof(event->'error_class') = 'string'
    THEN event->>'error_class' ELSE '' END))
  WHERE (CASE WHEN jsonb_typeof(event->'error_class') = 'string'
    THEN event->>'error_class' ELSE '' END) <> '';
