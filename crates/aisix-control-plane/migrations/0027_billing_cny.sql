-- The money path moves to CNY. Every table that held USD money gets a `_cny`
-- twin with the same shape, and the USD table is fenced against new rows. The
-- rows already there stay where they are: there is no exchange rate anywhere
-- in this platform, so customer USD money is refused at the door instead of
-- converted (design D14, D15).
--
-- What this migration does NOT do: it does not convert, move or delete a
-- single USD amount. A database that holds customer money in USD cannot take
-- this release at all, and the guard below is what says so, by name and by
-- count, before anything else runs.

-- 1. The guard: no customer tenant holds USD money except its usage history.
--
-- A customer is every tenant that is not the platform's own. An approved or
-- rejected top-up already moved money or explicitly did not, and a rejected
-- one never had an entry, so only `pending` top-ups are counted (R3). Quotas
-- and credit limits are money a tenant has been promised rather than money it
-- holds, and both are re-expressed in CNY by this release, so a live one is a
-- migration this release cannot do for the tenant.
--
-- The counts are named individually, because the operator's remedy differs:
-- a ledger balance has to be settled by hand, a pending top-up decided, a
-- quota re-set, and a credit limit is a number to re-enter.
DO $$
DECLARE l bigint; t bigint; q bigint; c bigint;
BEGIN
  SELECT count(DISTINCT e.tenant_id) INTO l
    FROM billing_ledger e JOIN tenants x ON x.id = e.tenant_id WHERE NOT x.is_internal;
  SELECT count(*) INTO t
    FROM billing_topups p JOIN tenants x ON x.id = p.tenant_id
    WHERE NOT x.is_internal AND p.status = 'pending';
  SELECT count(*) INTO q
    FROM billing_budgets b JOIN tenants x ON x.id = b.tenant_id
    WHERE NOT x.is_internal AND b.deleted_at IS NULL;
  SELECT count(*) INTO c
    FROM billing_accounts a JOIN tenants x ON x.id = a.tenant_id
    WHERE NOT x.is_internal AND a.credit_limit_micro_usd > 0;
  IF l + t + q + c > 0 THEN RAISE EXCEPTION
    'customer USD money exists (tenants with ledger entries: %, pending top-ups: %, live quotas: %, credit limits: %); money is CNY from this release and is never converted: settle it by hand before upgrading', l, t, q, c;
  END IF;
END $$;

-- 2. The accounts. The balance is a cache of the ledger and the credit limit a
-- number an operator typed, so both are new columns rather than a table: the
-- account row also carries `balance_budget_id` and `marks_dirty`, which the
-- gateway's documents depend on and which the currency does not change.
--
-- A customer account past pending is flagged, so the settler writes its
-- balance mark once from the CNY columns: until it does, the mark was decided
-- from a balance that is no longer the one this release reads.
ALTER TABLE billing_accounts
  ADD COLUMN balance_micro_cny bigint NOT NULL DEFAULT 0,
  ADD COLUMN credit_limit_micro_cny bigint NOT NULL DEFAULT 0
    CHECK (credit_limit_micro_cny >= 0);
UPDATE billing_accounts a SET marks_dirty = true
  FROM tenants t
  WHERE t.id = a.tenant_id AND NOT t.is_internal AND t.status <> 'pending';

-- 3. The internal tenant is funded postpaid (R2): it is never marked, so its
-- balance runs negative as the platform's own consumption, and an operator
-- credit settles it for bookkeeping. A fresh database creates it postpaid in
-- `tenants::ensure_internal_tenant`; this sets the row an upgrade already has.
UPDATE tenants SET billing_mode = 'postpaid' WHERE is_internal;

-- 4. `pricing_charges_cny`: one charge per usage row, written once and never
-- updated, as `pricing_charges` is. Every column, constraint and index that
-- table has after 0010, 0011, 0013, 0017, 0018, 0021 and 0022, with:
--   - `charge_micro_cny` in place of `charge_micro_usd`; `cost_micro_usd`
--     stays, because an upstream's cost is in USD;
--   - `sell_revision_id` and `tenant_override` gone: a price is a baseline
--     times a group rate, and no tenant has a card of its own;
--   - the group, the baseline and the rate that priced the row, so margin and
--     re-pricing read what was charged instead of reconstructing it from key
--     state that may since have changed;
--   - the discount's references, which stay: the discount is kept.
CREATE TABLE pricing_charges_cny (
  seq bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
  event_id text PRIMARY KEY REFERENCES usage_events(event_id),
  tenant_id text NOT NULL, api_key_id text NOT NULL, user_id text NOT NULL,
  requested_model_id text NOT NULL, occurred_at timestamptz NOT NULL, price_instant timestamptz,
  status text NOT NULL CHECK (status IN ('charged','exempt','nothing_to_bill','unpriced')),
  charge_micro_cny bigint CHECK (charge_micro_cny >= 0),
  cost_micro_usd bigint CHECK (cost_micro_usd >= 0), cost_source jsonb,
  group_id uuid REFERENCES provider_groups(id),
  baseline_revision_id bigint REFERENCES pricing_baselines(revision_id),
  rate_revision_id bigint REFERENCES pricing_group_rates(revision_id),
  cny_per_usd_e4 bigint,
  rate_is_model_override boolean NOT NULL DEFAULT false,
  discount_revision_id bigint REFERENCES pricing_discount_revisions(revision_id),
  discount_bps bigint NOT NULL DEFAULT 0,
  service_tier text, requested_service_tier text, long_context_above bigint,
  estimated boolean NOT NULL DEFAULT false, exemption text, error_code text,
  lines jsonb NOT NULL DEFAULT '[]', warnings jsonb NOT NULL DEFAULT '[]',
  engine_version bigint NOT NULL, priced_at timestamptz NOT NULL DEFAULT now(),
  request_id text,
  CHECK ((status = 'unpriced') = (charge_micro_cny IS NULL)));
CREATE INDEX pricing_charges_cny_tenant ON pricing_charges_cny (tenant_id, occurred_at);
CREATE INDEX pricing_charges_cny_key ON pricing_charges_cny (api_key_id, occurred_at);
CREATE INDEX pricing_charges_cny_unpriced ON pricing_charges_cny (seq) WHERE status = 'unpriced';
CREATE INDEX pricing_charges_cny_tenant_seq ON pricing_charges_cny (tenant_id, seq);
CREATE INDEX pricing_charges_cny_key_seq ON pricing_charges_cny (api_key_id, seq);
CREATE INDEX pricing_charges_cny_unpriced_recent ON pricing_charges_cny (priced_at, error_code)
  WHERE status = 'unpriced';
CREATE INDEX pricing_charges_cny_status_seq ON pricing_charges_cny (status, seq);
CREATE INDEX pricing_charges_cny_user_seq ON pricing_charges_cny (user_id, seq);
CREATE INDEX pricing_charges_cny_user_at ON pricing_charges_cny (user_id, occurred_at);
-- A re-pricing run selects charges by `occurred_at` alone when it names no
-- tenant, and an apply selects them again while it holds its tenants'
-- accounts, which holds their usage intake: without this index both would
-- scan every charge.
CREATE INDEX pricing_charges_cny_occurred ON pricing_charges_cny (occurred_at);
CREATE INDEX pricing_charges_cny_tenant_model_seq
  ON pricing_charges_cny (tenant_id, requested_model_id, seq);
CREATE INDEX pricing_charges_cny_tenant_request
  ON pricing_charges_cny (tenant_id, request_id, occurred_at, seq);
-- The margin view: what one group charged for one model over a period.
CREATE INDEX pricing_charges_cny_margin
  ON pricing_charges_cny (group_id, requested_model_id, occurred_at);
-- 0022's trigger, unchanged but for the table it fills: a writer that predates
-- the column (an older instance in a rolling upgrade) leaves the request id
-- NULL, and this fills it from the charge's own usage row.
CREATE OR REPLACE FUNCTION pricing_charges_cny_request_id() RETURNS trigger LANGUAGE plpgsql AS
  $$ BEGIN
       SELECT ue.request_id INTO NEW.request_id
         FROM usage_events ue WHERE ue.event_id = NEW.event_id;
       RETURN NEW;
     END $$;
CREATE TRIGGER pricing_charges_cny_request_id BEFORE INSERT ON pricing_charges_cny
  FOR EACH ROW WHEN (NEW.request_id IS NULL) EXECUTE FUNCTION pricing_charges_cny_request_id();

-- 5. The other money tables, each mirroring its own after 0014, 0015, 0018 and
-- 0020, with `*_micro_cny` in place of `*_micro_usd` and every foreign key
-- pointing at its `_cny` twin.

-- The ledger stays append-only: a correction is a new entry, never an edit.
CREATE TABLE billing_ledger_cny (
  seq bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,   -- paging only
  tenant_id uuid NOT NULL REFERENCES billing_accounts(tenant_id),
  kind text NOT NULL CHECK (kind IN ('usage','topup','adjustment')),
  amount_micro_cny bigint NOT NULL CHECK (amount_micro_cny <> 0),
  balance_after_micro_cny bigint NOT NULL,
  event_id text UNIQUE REFERENCES pricing_charges_cny(event_id),
  topup_id uuid UNIQUE,
  repricing_run_id uuid,
  posted_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT billing_ledger_cny_source CHECK (
    (kind = 'usage' AND event_id IS NOT NULL AND topup_id IS NULL AND repricing_run_id IS NULL AND amount_micro_cny < 0)
    OR (kind = 'topup' AND topup_id IS NOT NULL AND event_id IS NULL AND repricing_run_id IS NULL AND amount_micro_cny > 0)
    OR (kind = 'adjustment' AND repricing_run_id IS NOT NULL AND event_id IS NULL AND topup_id IS NULL)));
CREATE UNIQUE INDEX billing_ledger_cny_run_tenant
  ON billing_ledger_cny (repricing_run_id, tenant_id) WHERE repricing_run_id IS NOT NULL;
CREATE INDEX billing_ledger_cny_tenant_seq ON billing_ledger_cny (tenant_id, seq);
CREATE INDEX billing_ledger_cny_tenant_posted ON billing_ledger_cny (tenant_id, posted_at);
CREATE INDEX billing_ledger_cny_tenant_kind_seq ON billing_ledger_cny (tenant_id, kind, seq);
CREATE TRIGGER billing_ledger_cny_append_only BEFORE UPDATE OR DELETE ON billing_ledger_cny
  FOR EACH ROW EXECUTE FUNCTION billing_append_only();
-- A row trigger never fires on TRUNCATE (nor on TRUNCATE billing_accounts CASCADE).
CREATE TRIGGER billing_ledger_cny_no_truncate BEFORE TRUNCATE ON billing_ledger_cny
  FOR EACH STATEMENT EXECUTE FUNCTION billing_append_only();

-- Top-ups: money coming into a tenant's balance. A row leaves `pending` once
-- and never returns to it, and each top-up's ledger entry is unique on
-- `topup_id`.
CREATE TABLE billing_topups_cny (
  id uuid PRIMARY KEY, seq bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
  tenant_id uuid NOT NULL REFERENCES billing_accounts(tenant_id),
  amount_micro_cny bigint NOT NULL CHECK (amount_micro_cny > 0),
  origin text NOT NULL DEFAULT 'tenant' CHECK (origin IN ('tenant','operator')),
  note text NOT NULL DEFAULT '', reference text NOT NULL DEFAULT '',
  status text NOT NULL CHECK (status IN ('pending','approved','rejected')),
  requested_by uuid REFERENCES members(id), requested_at timestamptz NOT NULL DEFAULT now(),
  decided_by uuid REFERENCES operators(id), decided_at timestamptz, reject_reason text, credit_reason text,
  CHECK ((status = 'pending') = (decided_at IS NULL)),
  CHECK ((status = 'rejected') = (reject_reason IS NOT NULL)),
  CHECK ((origin = 'tenant') = (requested_by IS NOT NULL)),
  CHECK (origin = 'tenant' OR (status = 'approved' AND credit_reason IS NOT NULL AND decided_by IS NOT NULL)));
CREATE INDEX billing_topups_cny_tenant_seq ON billing_topups_cny (tenant_id, seq);
CREATE INDEX billing_topups_cny_pending_seq ON billing_topups_cny (seq) WHERE status = 'pending';

-- Re-pricing: a preview stores a run and posts nothing; an apply records one
-- adjustment per changed charge and one ledger entry per tenant.
CREATE TABLE billing_repricing_runs_cny (
  id uuid PRIMARY KEY, seq bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
  status text NOT NULL CHECK (status IN ('previewed','applied')),
  filter jsonb NOT NULL, reason text NOT NULL, pricing_version bigint NOT NULL, engine_version bigint NOT NULL,
  events bigint NOT NULL, changed bigint NOT NULL, still_unpriced bigint NOT NULL, skipped_unattributed bigint NOT NULL,
  delta_micro_cny bigint NOT NULL, by_tenant jsonb NOT NULL,
  created_by uuid NOT NULL REFERENCES operators(id), created_at timestamptz NOT NULL DEFAULT now(),
  applied_by uuid REFERENCES operators(id), applied_at timestamptz,
  CHECK ((status = 'applied') = (applied_at IS NOT NULL)));
CREATE TABLE pricing_adjustments_cny (
  run_id uuid NOT NULL REFERENCES billing_repricing_runs_cny(id),
  event_id text NOT NULL REFERENCES pricing_charges_cny(event_id),
  delta_micro_cny bigint NOT NULL CHECK (delta_micro_cny <> 0),
  baseline_revision_id bigint REFERENCES pricing_baselines(revision_id),
  rate_revision_id bigint REFERENCES pricing_group_rates(revision_id),
  engine_version bigint NOT NULL, reason text NOT NULL,
  created_by uuid NOT NULL REFERENCES operators(id), created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (run_id, event_id));
CREATE INDEX pricing_adjustments_cny_event ON pricing_adjustments_cny (event_id);
CREATE TRIGGER pricing_adjustments_cny_append_only BEFORE UPDATE OR DELETE ON pricing_adjustments_cny
  FOR EACH ROW EXECUTE FUNCTION billing_append_only();
CREATE TRIGGER pricing_adjustments_cny_no_truncate BEFORE TRUNCATE ON pricing_adjustments_cny
  FOR EACH STATEMENT EXECUTE FUNCTION billing_append_only();
ALTER TABLE billing_ledger_cny ADD CONSTRAINT billing_ledger_cny_topup_fk
  FOREIGN KEY (topup_id) REFERENCES billing_topups_cny(id);
ALTER TABLE billing_ledger_cny ADD CONSTRAINT billing_ledger_cny_run_fk
  FOREIGN KEY (repricing_run_id) REFERENCES billing_repricing_runs_cny(id);

-- 6. The fences. Every table that held USD money refuses a new row, so an
-- older control-plane replica's intake fails its whole transaction and the
-- gateway sends the batch again (behaviour change 12), instead of writing a
-- USD amount this release never reads. A rollback below this release is not
-- supported once this migration has run.
--
-- `pricing_discount_revisions` is not fenced: the tenant discount is kept.
CREATE FUNCTION billing_usd_fenced() RETURNS trigger LANGUAGE plpgsql AS
  $$ BEGIN
       RAISE EXCEPTION '% takes no new rows: billing is in CNY from migration 0027', TG_TABLE_NAME;
     END $$;
CREATE TRIGGER pricing_charges_usd_fenced BEFORE INSERT ON pricing_charges
  FOR EACH ROW EXECUTE FUNCTION billing_usd_fenced();
CREATE TRIGGER billing_ledger_usd_fenced BEFORE INSERT ON billing_ledger
  FOR EACH ROW EXECUTE FUNCTION billing_usd_fenced();
CREATE TRIGGER billing_topups_usd_fenced BEFORE INSERT ON billing_topups
  FOR EACH ROW EXECUTE FUNCTION billing_usd_fenced();
CREATE TRIGGER pricing_adjustments_usd_fenced BEFORE INSERT ON pricing_adjustments
  FOR EACH ROW EXECUTE FUNCTION billing_usd_fenced();
CREATE TRIGGER billing_repricing_runs_usd_fenced BEFORE INSERT ON billing_repricing_runs
  FOR EACH ROW EXECUTE FUNCTION billing_usd_fenced();
CREATE TRIGGER pricing_sell_revisions_usd_fenced BEFORE INSERT ON pricing_sell_revisions
  FOR EACH ROW EXECUTE FUNCTION billing_usd_fenced();
