-- 原生持久聊天和分片上传沿用既有对象归属机制，不改写已有记录。
ALTER TABLE native_objects DROP CONSTRAINT native_objects_kind_check;
ALTER TABLE native_objects ADD CONSTRAINT native_objects_kind_check
  CHECK (kind IN ('response', 'file', 'message_batch', 'chat_completion', 'upload'));
