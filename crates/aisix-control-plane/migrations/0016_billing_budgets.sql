-- Quotas: spend caps at three levels, each its own `budgets` document. An
-- operator sets an organization's (`team`, the tenant), an owner or admin a
-- member's (`member`, `org_admin`), and an owner, admin or the key's member
-- a key's (`api_key`, `org_admin` or `member`). A quota is periodic (a UTC
-- day, week or month) or cumulative, counting every charge priced from its
-- creation (`counted_from`) on. Usage intake adds each charge to the spend
-- row of every quota it falls under, in the period its `price_instant`
-- starts (a cumulative quota has one row, at `counted_from`). A quota row
-- references its tenant's account, not the tenant, and a spend row its
-- quota: intake inserts spend rows while it holds the account, and must
-- never wait on a tenant row. A deleted quota keeps its row (`deleted_at`).
CREATE TABLE billing_budgets (
  id uuid PRIMARY KEY, tenant_id uuid NOT NULL REFERENCES billing_accounts(tenant_id),
  scope text NOT NULL CHECK (scope IN ('team','member','api_key')), scope_ref uuid NOT NULL,
  set_by text NOT NULL CHECK (set_by IN ('operator','org_admin','member')),
  kind text NOT NULL DEFAULT 'periodic' CHECK (kind IN ('periodic','cumulative')),
  period text CHECK (period IN ('day','week','month')),
  counted_from timestamptz,                   -- cumulative: spend counts from here
  limit_micro_usd bigint NOT NULL CHECK (limit_micro_usd > 0),
  marked_period_start timestamptz,            -- periodic: the period the projected document marks exhausted
  marked_exhausted boolean,                   -- cumulative: whether the projected document is exhausted
  created_by_kind text NOT NULL, created_by uuid, created_at timestamptz NOT NULL DEFAULT now(),
  deleted_at timestamptz,
  CHECK ((kind = 'periodic') = (period IS NOT NULL)),
  CHECK ((kind = 'cumulative') = (counted_from IS NOT NULL)),
  CHECK ((scope = 'team') = (set_by = 'operator')),
  CHECK (scope <> 'member' OR set_by = 'org_admin'));
CREATE UNIQUE INDEX billing_budgets_one_per_setter ON billing_budgets
  (scope, scope_ref, kind, coalesce(period, '-'), set_by) WHERE deleted_at IS NULL;
CREATE INDEX billing_budgets_tenant ON billing_budgets (tenant_id) WHERE deleted_at IS NULL;
CREATE TABLE billing_budget_spend (
  budget_id uuid NOT NULL REFERENCES billing_budgets(id), period_start timestamptz NOT NULL,
  spent_micro_usd bigint NOT NULL DEFAULT 0, PRIMARY KEY (budget_id, period_start));
