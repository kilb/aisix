-- 提交元数据与原生权威 HTTP 上限一致，仍保留固定的数据库字节边界。
ALTER TABLE native_operations DROP CONSTRAINT native_operations_submission_check;
ALTER TABLE native_operations ADD CONSTRAINT native_operations_submission_check
  CHECK (octet_length(submission::text) <= 8388608);
