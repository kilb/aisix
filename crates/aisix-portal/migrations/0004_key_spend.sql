-- 每把密钥的累计花费。
--
-- 网关那边的额度闸是**进程内**计数器：重启即归零，于是每把密钥的子额度会
-- 「续杯」。用户级额度没有这个问题，因为它有持久兜底 —— 流水余额归零就把密钥
-- 写成停用。这张表给密钥级同样的兜底：按 api_key_id 从指标里累计花费，越过它
-- 自己的额度就停这一把。
--
-- 按 display_name 记，与 key_quotas 一致：那是网关认识的身份（entry id 由它
-- 派生）。窗口沿用用户的水位线（consumption_mark）—— 同一个用户名下所有密钥
-- 与用户本身用同一个窗口，两边的账才对得上。
CREATE TABLE key_spend (
  user_id     TEXT NOT NULL REFERENCES users(id),
  key_name    TEXT NOT NULL,
  spent_micro_usd INTEGER NOT NULL DEFAULT 0,
  updated_at  TEXT NOT NULL,
  PRIMARY KEY (user_id, key_name)
);

-- 流水的两个聚合（余额、总额度）都要按 user_id 扫这个人的全部行，而对账环
-- 每轮给每个有消费的用户追加一条 —— 表只增不减。把 source 与 delta 一起放进
-- 索引，SUM 就能直接在索引上算完，不必回表。
CREATE INDEX ledger_user_source ON ledger(user_id, source, delta_micro_usd);
