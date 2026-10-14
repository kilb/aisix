-- 花费计数器的游标：每条时间序列上次读到的累计值。
--
-- 之前按时间窗查 `increase(...[自上次至今])`，在首尾相接的窗口上它算不对：
-- 一个窗口里少于两个采样点就返回空（被当成 0），而且只取窗口内首末两个采样
-- 之差，窗口之间的那段增量永远算不到。抓取间隔一旦不比对账周期短，几乎什么
-- 都扣不到 —— 用真 Prometheus（抓取 15s、对账 5s）实测，$5 的消费扣了 $0。
--
-- 现在每轮读每条序列的原始累计值，与这里记下的上次读数做差。游标按**单条
-- 序列**（完整标签，含 instance）记，不按总和记：某个副本重启只影响它自己那条
-- 序列，不会像对总和做差那样把整体水位线拉回低点。每轮读数同时带着这条序列上次
-- 以来重置过几次（Prometheus 的 `resets()`）：重置过就按当前值算，没重置过而变小
-- 只是一次旧读数，不扣。读数是原始值而不是窗口内的差，所以对账周期与抓取间隔
-- 怎么搭配都不丢增量。
--
-- `series` 的格式落了库：改格式就等于让所有游标对不上、整段重扣，要改先写迁移。
CREATE TABLE spend_cursor (
  user_id         TEXT NOT NULL REFERENCES users(id),
  series          TEXT NOT NULL,             -- 排序后的完整标签
  value_micro_usd INTEGER NOT NULL,          -- 上次读到的累计值
  -- 上次读数时这条序列所在网关进程的启动时刻（毫秒，`process_start_time_seconds`）。
  -- 变了就是重启过 —— 计数器的值认不出「重启后在第一次抓取前就涨过了上次读数」
  -- 那种重启。旧网关不暴露它时为空，退回只按 `resets()` 判断。
  instance_started_ms INTEGER,
  updated_at      TEXT NOT NULL,
  PRIMARY KEY (user_id, series)
);

-- 这个用户的游标是否已经建好。已有的行（按时间窗计费时代的老用户）都是 0：升级
-- 后那一轮只记下当前读数、不扣款 —— 否则每条序列都会被当成新序列，整段历史消费
-- 被一次扣掉。之后新出现的序列（新副本、新密钥、新用户）从 0 算起，读到多少就是
-- 花了多少；新用户没有这一行，第一轮对账就照常扣。
ALTER TABLE consumption_mark ADD COLUMN cursors_seeded INTEGER NOT NULL DEFAULT 0;
