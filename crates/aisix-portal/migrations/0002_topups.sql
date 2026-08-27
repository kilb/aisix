-- 线下充值单。
--
-- 用户发起一笔申请，管理员在后台确认后才入账。真接支付时把「确认」换成支付
-- 回调即可，账本这一侧不用改（设计文档 §8 未决问题 1 说的可插拔来源）。
--
-- `status` 只在 pending → approved / rejected 之间走一次。批准时的入账与状态
-- 变更必须在同一个事务里，且状态变更要带上 `WHERE status = 'pending'` ——
-- 靠影响行数判断有没有人抢先处理过，而不是先查后写（那有竞态，后果是
-- 同一笔充值入账两次）。
CREATE TABLE topups (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id         TEXT NOT NULL REFERENCES users(id),
  micro_usd       INTEGER NOT NULL,
  note            TEXT,
  status          TEXT NOT NULL DEFAULT 'pending',
  created_at      TEXT NOT NULL,
  decided_at      TEXT,
  decided_note    TEXT
);
CREATE INDEX topups_user ON topups(user_id, id);
CREATE INDEX topups_pending ON topups(status, id);
