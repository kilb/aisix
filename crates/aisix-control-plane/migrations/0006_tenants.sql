-- Tenants: the platform's customers, which the gateways see as teams
-- (`teams/<tenant id>`). A tenant reaches the global model catalog only
-- through `allowed_model_ids`; `limits` is its default protection, projected
-- as the rate-limit policy `limits_policy_id`. Exactly one tenant is the
-- platform's own (`is_internal`).
CREATE TABLE tenants (id uuid PRIMARY KEY, name text NOT NULL,
  status text NOT NULL CHECK (status IN ('pending','active','suspended')),
  is_internal boolean NOT NULL DEFAULT false,
  billing_mode text NOT NULL DEFAULT 'prepaid' CHECK (billing_mode IN ('prepaid','postpaid')),
  allowed_model_ids uuid[] NOT NULL DEFAULT '{}', allowed_provider_groups text[] NOT NULL DEFAULT '{}',
  limits jsonb NOT NULL DEFAULT '{}', limits_policy_id uuid, version bigint NOT NULL DEFAULT 1,
  created_at timestamptz NOT NULL DEFAULT now(), approved_at timestamptz, approved_by uuid, suspended_at timestamptz);
CREATE UNIQUE INDEX tenants_one_internal ON tenants (is_internal) WHERE is_internal;
