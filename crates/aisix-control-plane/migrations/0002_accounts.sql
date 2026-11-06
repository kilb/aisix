-- Operator accounts, browser sessions (operators and tenant members) and the
-- audit log. Sessions are stored as the SHA-256 of their token.
CREATE TABLE operators (id uuid PRIMARY KEY, email text NOT NULL UNIQUE, password_hash text NOT NULL,
  display_name text, role text NOT NULL CHECK (role IN ('admin','operator','viewer')),
  disabled boolean NOT NULL DEFAULT false, created_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE sessions (token_sha256 bytea PRIMARY KEY,
  principal_kind text NOT NULL CHECK (principal_kind IN ('operator','member')),
  principal_id uuid NOT NULL, expires_at timestamptz NOT NULL, created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX sessions_principal ON sessions (principal_kind, principal_id);
CREATE TABLE audit_log (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, at timestamptz NOT NULL DEFAULT now(),
  actor_kind text NOT NULL, actor_id uuid, actor_email text, action text NOT NULL,
  target_kind text NOT NULL, target_id text, tenant_id uuid, detail jsonb NOT NULL DEFAULT '{}');
CREATE INDEX audit_target ON audit_log (target_kind, target_id, id DESC);
CREATE INDEX audit_tenant ON audit_log (tenant_id, id DESC);
