-- 用户。email 唯一：注册处依赖数据库层的唯一约束，不靠先查后插（那有竞态）。
CREATE TABLE users (
  id                TEXT PRIMARY KEY,        -- uuid v4，即密钥上的 user_id
  email             TEXT NOT NULL UNIQUE,
  password_hash     TEXT NOT NULL,           -- argon2 PHC 串
  display_name      TEXT,
  email_verified_at TEXT,                    -- 一期恒为 NULL，见计划裁决 4
  disabled          INTEGER NOT NULL DEFAULT 0,
  created_at        TEXT NOT NULL
);

-- 流水。只追加，绝不 UPDATE/DELETE：余额是它的和，账要能重算。
CREATE TABLE ledger (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id         TEXT NOT NULL REFERENCES users(id),
  -- 正数入账（发放/充值），负数出账（消费）。单位 micro-USD，整数。
  -- 用整数是因为浮点做钱会累积误差，而这个产品的花费到千分之一美分。
  delta_micro_usd INTEGER NOT NULL,
  source          TEXT NOT NULL,             -- admin_grant | consumption | payment
  note            TEXT,
  created_at      TEXT NOT NULL
);
CREATE INDEX ledger_user ON ledger(user_id, id);

-- 消费对账的水位线：记「已经计到哪个时刻」，不是「已经计了多少」。
--
-- 这里曾经想记累计额、每轮扣差值 —— 那是错的。花费指标是 counter，而且
-- **每个网关副本各自暴露一份**；任一副本重启都会让 sum 下陷，看起来就像
-- counter 重置。按累计额做差就会把水位线重新对齐到低点，那一刻起未入账的
-- 消费永久丢失，且毫无信号：用户白得推理，账面看不出异常。
--
-- 改成记时刻后，每轮查 increase(...[自上次至今])。increase() 是逐时间序列
-- 处理重置再求和的，跨副本天然安全，也没有缺口或重叠。
CREATE TABLE consumption_mark (
  user_id         TEXT PRIMARY KEY REFERENCES users(id),
  counted_through TEXT NOT NULL,             -- RFC3339，已计入流水的截止时刻
  updated_at      TEXT NOT NULL
);
