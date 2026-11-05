-- 门户自己停掉的密钥。
--
-- 配置文件里 `disabled` 只有一个布尔值，看不出是谁停的。之前对账环见到「该开却关
-- 着」就打开 —— 运维在原文页停掉一把泄漏的密钥，界面说已保存，下一轮就被重新启用。
-- 现在只有这张表里的密钥才归对账环启用。
--
-- key_name 是 display_name，与 key_quotas 一致。
--
-- `''` 是「还没登记过」的哨兵。升级之前门户停掉的密钥没有记录，不处理的话它们补了额
-- 也永远回不来。第一轮对账把当时已停用、且有主人的密钥全部登记成门户停的（此前能
-- 给这些密钥写 disabled 的基本只有门户），然后删掉哨兵。display_name 不能为空，
-- 撞不上真密钥。
CREATE TABLE portal_disabled_keys (
  key_name   TEXT PRIMARY KEY,
  updated_at TEXT NOT NULL
);
INSERT INTO portal_disabled_keys (key_name, updated_at) VALUES ('', '');
