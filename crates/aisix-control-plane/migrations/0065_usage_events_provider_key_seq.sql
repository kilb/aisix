-- no-transaction
-- 按账户和存储序号有界翻页；建索引期间仍可摄入用量。下一迁移复核索引是否就绪。
-- 控制面启动为整轮迁移预留 600 秒；超大表可在升级前单独执行下列同名同定义的并发建索引语句。
-- 控制面使用标准 UUID 字符串定位账户，不索引自由格式的其他遥测值。
CREATE INDEX CONCURRENTLY IF NOT EXISTS usage_events_provider_key_seq
  ON usage_events ((event->>'provider_key_id'), seq)
  WHERE length(event->>'provider_key_id') = 36;
