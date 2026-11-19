-- 保留每个已接纳的请求，避免是否存在成员影响发送限流。邮件送达后再填验证码摘要。
CREATE TABLE portal_password_recovery (
    id uuid PRIMARY KEY,
    recipient_mac bytea NOT NULL,
    member_id uuid,
    code_mac bytea,
    issued_at timestamptz NOT NULL DEFAULT statement_timestamp(),
    attempts smallint NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 5),
    closed boolean NOT NULL DEFAULT false,
    CHECK ((member_id IS NULL) = (code_mac IS NULL))
);

CREATE INDEX portal_password_recovery_issued ON portal_password_recovery (issued_at);
CREATE INDEX portal_password_recovery_recipient_issued ON portal_password_recovery (recipient_mac, issued_at);
CREATE UNIQUE INDEX portal_password_recovery_open ON portal_password_recovery (recipient_mac)
    WHERE code_mac IS NOT NULL AND NOT closed;
