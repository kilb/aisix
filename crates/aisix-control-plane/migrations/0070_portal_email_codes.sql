-- 每次发信占一个配额，即使服务商拒绝请求也保留。只记录收件地址和验证码的带密钥摘要。
CREATE TABLE portal_email_codes (
    id uuid PRIMARY KEY,
    recipient_mac bytea NOT NULL,
    code_mac bytea NOT NULL,
    issued_at timestamptz NOT NULL DEFAULT statement_timestamp(),
    delivered boolean NOT NULL DEFAULT false,
    closed boolean NOT NULL DEFAULT false,
    attempts smallint NOT NULL DEFAULT 0
);

CREATE INDEX portal_email_codes_issued ON portal_email_codes (issued_at);
CREATE INDEX portal_email_codes_recipient ON portal_email_codes (recipient_mac, issued_at DESC);
CREATE UNIQUE INDEX portal_email_codes_current ON portal_email_codes (recipient_mac)
    WHERE delivered AND NOT closed;
