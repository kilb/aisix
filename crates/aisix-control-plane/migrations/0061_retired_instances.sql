-- 明确退役只记录管理员确认，不删除实例的版本、用量或历史时间。
ALTER TABLE dp_instances ADD COLUMN retired_at timestamptz;

-- 旧版控制面的 upsert 也更新 last_seen_at；按事件清标记，不比较可能倒置的事务时间。
CREATE FUNCTION clear_instance_retirement_on_heartbeat() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  NEW.retired_at := NULL;
  RETURN NEW;
END;
$$;
CREATE TRIGGER instance_heartbeat_clears_retirement
  BEFORE UPDATE OF last_seen_at ON dp_instances
  FOR EACH ROW EXECUTE FUNCTION clear_instance_retirement_on_heartbeat();
