-- 每把密钥的额度。
--
-- 用户把自己的总额度分配到名下各把密钥上，分配总和不得超过总额度 —— 那条
-- 不变量在写入时校验（见 keys::set_quota）。
--
-- 按 display_name 记，因为那是网关认识的身份：文件模式下密钥的 entry id 是
-- `uuid5(命名空间, "api_keys/<display_name>")`，而下推的策略要按 entry id 定位。
CREATE TABLE key_quotas (
  user_id     TEXT NOT NULL REFERENCES users(id),
  key_name    TEXT NOT NULL,
  micro_usd   INTEGER NOT NULL,
  updated_at  TEXT NOT NULL,
  PRIMARY KEY (user_id, key_name)
);
