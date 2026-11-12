-- 冷却属于账号对此公共模型的直接目标；旧行缺省关闭，不改既有选路。
ALTER TABLE channel_models ADD COLUMN cooldown jsonb;

-- 老控制面会读更高版本的数据库，但其生成器不认识此列。只拦会丢配置的
-- 直接资源写入；投影状态列更新和撤销不受影响，避免阻塞后续撤权的投影。
CREATE FUNCTION channel_cooldown_kept() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
  channel channel_models%ROWTYPE;
BEGIN
  IF NEW.kind <> 'models' OR NEW.deleted THEN
    RETURN NEW;
  END IF;
  SELECT c.* INTO channel FROM channel_models c WHERE c.direct_model_id = NEW.id;
  IF NOT FOUND OR NEW.managed_by IS DISTINCT FROM 'public:' || channel.public_model_id::text THEN
    RETURN NEW;
  END IF;
  IF (NEW.doc -> 'cooldown') IS DISTINCT FROM channel.cooldown THEN
    RAISE EXCEPTION 'the direct model % does not carry its stored cooldown settings', NEW.id
      USING ERRCODE = 'check_violation', CONSTRAINT = 'channel_cooldown_kept';
  END IF;
  RETURN NEW;
END
$$;

CREATE TRIGGER channel_cooldown_kept
  BEFORE INSERT OR UPDATE OF doc, deleted, managed_by ON gw_resources
  FOR EACH ROW EXECUTE FUNCTION channel_cooldown_kept();
