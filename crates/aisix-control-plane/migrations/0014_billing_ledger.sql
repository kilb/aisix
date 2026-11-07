-- Each tenant's billing account, a signed balance in micro-USD, and its
-- append-only ledger. Every entry moves the balance by its amount and records
-- the balance after it: `usage` debits one charged usage row (`event_id`),
-- `topup` credits one top-up, and `adjustment` is one re-pricing run's
-- correction for the tenant. A trigger refuses every UPDATE and DELETE of a
-- ledger row. Ingest locks accounts, never tenant rows: a ledger entry
-- references its account, not its tenant.
CREATE TABLE billing_accounts (
  tenant_id uuid PRIMARY KEY REFERENCES tenants(id),
  balance_micro_usd bigint NOT NULL DEFAULT 0,
  credit_limit_micro_usd bigint NOT NULL DEFAULT 0 CHECK (credit_limit_micro_usd >= 0),
  balance_budget_id uuid NOT NULL UNIQUE DEFAULT gen_random_uuid(), -- the balance mark's budgets document id
  marked_exhausted boolean,                  -- what the projected balance document says; NULL: none projected
  marks_dirty boolean NOT NULL DEFAULT false,
  updated_at timestamptz NOT NULL DEFAULT now());
-- Existing customer tenants that are past pending get their balance mark on the settler's first pass.
INSERT INTO billing_accounts (tenant_id, marks_dirty)
  SELECT id, NOT is_internal AND status <> 'pending' FROM tenants;
CREATE INDEX billing_accounts_dirty ON billing_accounts (tenant_id) WHERE marks_dirty;
CREATE TABLE billing_ledger (
  seq bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,   -- paging only
  tenant_id uuid NOT NULL REFERENCES billing_accounts(tenant_id),
  kind text NOT NULL CHECK (kind IN ('usage','topup','adjustment')),
  amount_micro_usd bigint NOT NULL CHECK (amount_micro_usd <> 0),
  balance_after_micro_usd bigint NOT NULL,
  event_id text UNIQUE REFERENCES pricing_charges(event_id),
  topup_id uuid UNIQUE,                       -- FK added by 0015
  repricing_run_id uuid,                      -- FK added by 0018
  posted_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT billing_ledger_source CHECK (
    (kind = 'usage' AND event_id IS NOT NULL AND topup_id IS NULL AND repricing_run_id IS NULL AND amount_micro_usd < 0)
    OR (kind = 'topup' AND topup_id IS NOT NULL AND event_id IS NULL AND repricing_run_id IS NULL AND amount_micro_usd > 0)
    OR (kind = 'adjustment' AND repricing_run_id IS NOT NULL AND event_id IS NULL AND topup_id IS NULL)));
CREATE UNIQUE INDEX billing_ledger_run_tenant ON billing_ledger (repricing_run_id, tenant_id) WHERE repricing_run_id IS NOT NULL;
CREATE INDEX billing_ledger_tenant_seq ON billing_ledger (tenant_id, seq);
CREATE INDEX billing_ledger_tenant_posted ON billing_ledger (tenant_id, posted_at);
CREATE FUNCTION billing_append_only() RETURNS trigger LANGUAGE plpgsql AS
  $$ BEGIN RAISE EXCEPTION '% is append-only', TG_TABLE_NAME; END $$;
CREATE TRIGGER billing_ledger_append_only BEFORE UPDATE OR DELETE ON billing_ledger
  FOR EACH ROW EXECUTE FUNCTION billing_append_only();
