-- Tenant members and their invitations. A live member's email is unique
-- across the platform (one person belongs to one tenant at a time); a
-- removed member keeps its row, so its email can be invited again. An
-- invitation is a one-time token, stored as its SHA-256.
CREATE TABLE members (id uuid PRIMARY KEY, tenant_id uuid NOT NULL REFERENCES tenants(id),
  email text NOT NULL, password_hash text NOT NULL, display_name text,
  role text NOT NULL CHECK (role IN ('owner','admin','member','billing')),
  disabled boolean NOT NULL DEFAULT false, removed_at timestamptz, created_at timestamptz NOT NULL DEFAULT now());
CREATE UNIQUE INDEX members_email_live ON members (email) WHERE removed_at IS NULL;
CREATE INDEX members_tenant ON members (tenant_id);
CREATE TABLE member_invites (id uuid PRIMARY KEY, tenant_id uuid NOT NULL REFERENCES tenants(id),
  email text NOT NULL, role text NOT NULL, token_sha256 bytea NOT NULL UNIQUE, expires_at timestamptz NOT NULL,
  created_by_kind text NOT NULL, created_by uuid, accepted_at timestamptz, revoked_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now());
