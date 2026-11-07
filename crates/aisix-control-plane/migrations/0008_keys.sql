-- Tenant API keys. Each live key projects one gateway document,
-- `api_keys/<id>`, built from the key, its tenant and its member. Only the
-- SHA-256 of the plaintext is kept (`key_hash`); `key_prefix` shows the key
-- by its first characters. `disabled` is the tenant's own switch and
-- `operator_disabled` the platform's, which the tenant cannot undo.
-- `doc_version` is the shape the key's document was last written in
-- (`keys::KEY_DOC_VERSION`); a key behind the running release's is
-- rewritten at boot and on every housekeeping tick. A revoked key keeps
-- its row (`revoked_at`) and leaves every list.
CREATE TABLE tenant_keys (id uuid PRIMARY KEY, tenant_id uuid NOT NULL REFERENCES tenants(id), member_id uuid REFERENCES members(id),
  label text NOT NULL, key_hash text NOT NULL UNIQUE, key_prefix text NOT NULL,
  model_ids uuid[], allowed_models text[], allowed_provider_groups text[], rate_limit jsonb NOT NULL DEFAULT '{}',
  disabled boolean NOT NULL DEFAULT false, operator_disabled boolean NOT NULL DEFAULT false,
  doc_version int NOT NULL DEFAULT 0, expires_at timestamptz, version bigint NOT NULL DEFAULT 1,
  created_by_kind text NOT NULL, created_by uuid, created_at timestamptz NOT NULL DEFAULT now(), revoked_at timestamptz);
CREATE INDEX tenant_keys_tenant ON tenant_keys (tenant_id) WHERE revoked_at IS NULL;
CREATE INDEX tenant_keys_member ON tenant_keys (member_id) WHERE revoked_at IS NULL;
-- Every key of a tenant, revoked ones included (a tenant's rollout covers a
-- revoke still on its way).
CREATE INDEX tenant_keys_tenant_any ON tenant_keys (tenant_id);
-- The live keys whose documents are behind (`doc_version < $1`): a range
-- scan that reads nothing while none are, which is every tick but the
-- first after an upgrade.
CREATE INDEX tenant_keys_doc_version ON tenant_keys (doc_version) WHERE revoked_at IS NULL;
-- The documents a tenant, a member or a key derives, found by their owner.
-- `gw_resources` is 0003's; the index is here as the next migration.
CREATE INDEX gw_resources_managed_by ON gw_resources (managed_by) WHERE managed_by IS NOT NULL;
