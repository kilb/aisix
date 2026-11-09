-- Quotas move to CNY. `billing_budgets_cny` and `billing_budget_spend_cny`
-- mirror `0016`'s tables with `*_micro_cny` columns, and the two USD tables
-- are fenced against new rows, as every money table was in `0027`.
--
-- Nothing is converted. `0027`'s guard already refuses a database holding a
-- live customer quota, so a customer has none by the time this runs, and the
-- platform tenant's USD quotas are history: the release reads no row from
-- `billing_budgets` again.
CREATE TABLE billing_budgets_cny (
  id uuid PRIMARY KEY, tenant_id uuid NOT NULL REFERENCES billing_accounts(tenant_id),
  scope text NOT NULL CHECK (scope IN ('team','member','api_key')), scope_ref uuid NOT NULL,
  set_by text NOT NULL CHECK (set_by IN ('operator','org_admin','member')),
  kind text NOT NULL DEFAULT 'periodic' CHECK (kind IN ('periodic','cumulative')),
  period text CHECK (period IN ('day','week','month')),
  counted_from timestamptz,                   -- cumulative: spend counts from here
  limit_micro_cny bigint NOT NULL CHECK (limit_micro_cny > 0),
  marked_period_start timestamptz,            -- periodic: the period the projected document marks exhausted
  marked_exhausted boolean,                   -- cumulative: whether the projected document is exhausted
  created_by_kind text NOT NULL, created_by uuid, created_at timestamptz NOT NULL DEFAULT now(),
  deleted_at timestamptz,
  CHECK ((kind = 'periodic') = (period IS NOT NULL)),
  CHECK ((kind = 'cumulative') = (counted_from IS NOT NULL)),
  CHECK ((scope = 'team') = (set_by = 'operator')),
  CHECK (scope <> 'member' OR set_by = 'org_admin'));
CREATE UNIQUE INDEX billing_budgets_cny_one_per_setter ON billing_budgets_cny
  (scope, scope_ref, kind, coalesce(period, '-'), set_by) WHERE deleted_at IS NULL;
CREATE INDEX billing_budgets_cny_tenant ON billing_budgets_cny (tenant_id) WHERE deleted_at IS NULL;
CREATE TABLE billing_budget_spend_cny (
  budget_id uuid NOT NULL REFERENCES billing_budgets_cny(id), period_start timestamptz NOT NULL,
  spent_micro_cny bigint NOT NULL DEFAULT 0, PRIMARY KEY (budget_id, period_start));

-- The fences. A `BEFORE INSERT` trigger fires on `INSERT … ON CONFLICT` as
-- well, so intake's spend upsert is fenced too: an older replica's count
-- fails its whole transaction and the gateway sends the batch again.
CREATE TRIGGER billing_budgets_usd_fenced BEFORE INSERT ON billing_budgets
  FOR EACH ROW EXECUTE FUNCTION billing_usd_fenced();
CREATE TRIGGER billing_budget_spend_usd_fenced BEFORE INSERT ON billing_budget_spend
  FOR EACH ROW EXECUTE FUNCTION billing_usd_fenced();
