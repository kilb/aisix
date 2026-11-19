CREATE TABLE portal_passkeys (
    id uuid PRIMARY KEY,
    member_id uuid NOT NULL REFERENCES members(id) ON DELETE CASCADE,
    rp_id text NOT NULL,
    credential_id bytea NOT NULL,
    public_credential jsonb NOT NULL,
    revision bigint NOT NULL DEFAULT 0,
    name text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz,
    UNIQUE (rp_id, credential_id)
);

CREATE INDEX portal_passkeys_member ON portal_passkeys (member_id, rp_id);

-- WebAuthn 状态只存在服务端，按用途和浏览器或门户会话绑定。
CREATE TABLE portal_passkey_challenges (
    id uuid PRIMARY KEY,
    member_id uuid NOT NULL REFERENCES members(id) ON DELETE CASCADE,
    purpose text NOT NULL CHECK (purpose IN ('register', 'login')),
    origin text NOT NULL,
    binding_sha256 bytea NOT NULL,
    verification_state jsonb NOT NULL,
    expires_at timestamptz NOT NULL,
    UNIQUE (member_id, purpose, binding_sha256)
);

CREATE INDEX portal_passkey_challenges_expiry ON portal_passkey_challenges (expires_at);
