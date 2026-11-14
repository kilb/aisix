-- 接受后的对象绑定使用准入时的可信可见范围；旧提交缺少证明时默认收窄。
-- 原有 native_operations_immutable 触发器同时禁止改写该列；该值不来自客户端 DTO。
ALTER TABLE native_operations
  ADD COLUMN visibility_full boolean NOT NULL DEFAULT false;
