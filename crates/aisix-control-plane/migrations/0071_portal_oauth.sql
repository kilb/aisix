-- 第三方账号只保存稳定的服务商主体，登录不使用邮箱匹配。
CREATE TABLE member_oauth_identities (
  member_id uuid NOT NULL REFERENCES members(id) ON DELETE CASCADE,
  provider text NOT NULL CHECK (provider IN ('github', 'google')),
  subject text NOT NULL,
  PRIMARY KEY (member_id, provider),
  UNIQUE (provider, subject)
);

-- state 与 Cookie 各自独立，数据库只存摘要；PKCE verifier 用主密钥加密。
-- 会话摘要绑定发起浏览器，关联账号时须与复核密码时的身份和会话相同。
CREATE TABLE portal_oauth_flows (
  state_sha256 bytea PRIMARY KEY,
  browser_sha256 bytea NOT NULL,
  provider text NOT NULL CHECK (provider IN ('github', 'google')),
  verifier_sealed text NOT NULL,
  client_id text NOT NULL,
  redirect_uri text NOT NULL,
  expires_at timestamptz NOT NULL,
  session_sha256 bytea,
  link_member_id uuid REFERENCES members(id) ON DELETE CASCADE,
  reauth_sha256 bytea,
  turnstile_policy_sha256 bytea,
  CHECK ((link_member_id IS NULL) = (turnstile_policy_sha256 IS NOT NULL)),
  CHECK (turnstile_policy_sha256 IS NULL OR octet_length(turnstile_policy_sha256) = 32),
  CHECK ((link_member_id IS NULL) = (reauth_sha256 IS NULL)),
  CHECK (link_member_id IS NULL OR session_sha256 IS NOT NULL)
);

CREATE INDEX portal_oauth_flows_expiry ON portal_oauth_flows (expires_at);
