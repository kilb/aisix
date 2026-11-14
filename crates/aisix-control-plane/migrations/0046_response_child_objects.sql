-- Responses 原始项目及容器 ID 归属持久化，身份在返回客户端前与父 Response 一并绑定。
ALTER TABLE native_objects DROP CONSTRAINT native_objects_kind_check;
ALTER TABLE native_objects ADD CONSTRAINT native_objects_kind_check
  CHECK (kind IN ('response', 'response_item', 'response_container', 'file', 'message_batch', 'chat_completion', 'upload'));
