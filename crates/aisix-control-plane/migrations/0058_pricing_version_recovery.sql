-- 精确解析 bigint；先限制十进制长度，再比较 numeric，非法 JSON 不触发强制转换错误。
CREATE FUNCTION pricing_version_number(version jsonb) RETURNS bigint
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT CASE WHEN jsonb_typeof(version) = 'number' AND version::text ~ '^-?[0-9]{1,19}$'
    THEN CASE WHEN version::text::numeric BETWEEN -9223372036854775808 AND 9223372036854775807
      THEN version::text::bigint END END
$$;

-- 旧读者最多接受十八位，新的命名空间严格大于任何旧缓存。
-- 高水位不随事务回滚，版本行丢失或损坏后也不能复用已提交过的版本。
CREATE SEQUENCE pricing_version_high_water AS bigint START WITH 1000000000000000000 CACHE 1 NO CYCLE;
SELECT setval('pricing_version_high_water',
  GREATEST(1000000000000000000, COALESCE(
    (SELECT pricing_version_number(value) FROM cp_settings WHERE key = 'pricing_version'), 0)));
INSERT INTO cp_settings (key, value) VALUES
  ('pricing_version', to_jsonb((SELECT last_value FROM pricing_version_high_water)))
ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = clock_timestamp();

-- 旧进程仍用 value + 1，合法人工写也必须推进高水位。
-- 正常递增保持一步；从坏值恢复、重新插入或倒退的合法值进入新版本。
CREATE FUNCTION pricing_version_keep_high_water() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
  version bigint := pricing_version_number(NEW.value);
  prior bigint;
BEGIN
  IF version IS NOT NULL THEN
    IF TG_OP = 'UPDATE' THEN prior := pricing_version_number(OLD.value); END IF;
    IF TG_OP = 'INSERT' OR prior IS NULL OR version <= prior THEN
      version := GREATEST(version, nextval('pricing_version_high_water'));
      NEW.value := to_jsonb(version);
    END IF;
    IF version > (SELECT last_value FROM pricing_version_high_water) THEN
      PERFORM setval('pricing_version_high_water', version);
    END IF;
  END IF;
  RETURN NEW;
END
$$;
CREATE TRIGGER pricing_version_keep_high_water
BEFORE INSERT OR UPDATE OF value ON cp_settings FOR EACH ROW
WHEN (NEW.key = 'pricing_version') EXECUTE FUNCTION pricing_version_keep_high_water();
