-- Re-pricing: correcting past charges at the prices in force now. An
-- operator previews a run over a filter of stored charges, which stores the
-- run with its totals by tenant and posts nothing; an admin applies it,
-- which records one `pricing_adjustments` row per changed charge and posts
-- one `adjustment` ledger entry per tenant (`repricing_run_id`), in one
-- transaction. A charge's effective amount is its `charge_micro_usd` plus
-- the sum of its adjustments: charges are written once and never updated,
-- and the adjustments are append-only, like the ledger.
CREATE TABLE billing_repricing_runs (
  id uuid PRIMARY KEY, seq bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
  status text NOT NULL CHECK (status IN ('previewed','applied')),
  filter jsonb NOT NULL, reason text NOT NULL, pricing_version bigint NOT NULL, engine_version bigint NOT NULL,
  events bigint NOT NULL, changed bigint NOT NULL, still_unpriced bigint NOT NULL, skipped_unattributed bigint NOT NULL,
  delta_micro_usd bigint NOT NULL, by_tenant jsonb NOT NULL,
  created_by uuid NOT NULL REFERENCES operators(id), created_at timestamptz NOT NULL DEFAULT now(),
  applied_by uuid REFERENCES operators(id), applied_at timestamptz,
  CHECK ((status = 'applied') = (applied_at IS NOT NULL)));
CREATE TABLE pricing_adjustments (            -- the table P2a Ruling 25 fixed; run_id added
  run_id uuid NOT NULL REFERENCES billing_repricing_runs(id),
  event_id text NOT NULL REFERENCES pricing_charges(event_id),
  delta_micro_usd bigint NOT NULL CHECK (delta_micro_usd <> 0),
  sell_revision_id bigint REFERENCES pricing_sell_revisions(revision_id),
  engine_version bigint NOT NULL, reason text NOT NULL,
  created_by uuid NOT NULL REFERENCES operators(id), created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (run_id, event_id));
CREATE INDEX pricing_adjustments_event ON pricing_adjustments (event_id);
CREATE TRIGGER pricing_adjustments_append_only BEFORE UPDATE OR DELETE ON pricing_adjustments
  FOR EACH ROW EXECUTE FUNCTION billing_append_only();
-- A row trigger never fires on TRUNCATE, so it is refused on its own, as for the ledger.
CREATE TRIGGER pricing_adjustments_no_truncate BEFORE TRUNCATE ON pricing_adjustments
  FOR EACH STATEMENT EXECUTE FUNCTION billing_append_only();
ALTER TABLE billing_ledger ADD CONSTRAINT billing_ledger_run_fk FOREIGN KEY (repricing_run_id) REFERENCES billing_repricing_runs(id);
-- A run selects charges by `occurred_at` alone when it names no tenant, and
-- an apply selects them again while it holds its tenants' accounts, which
-- holds their usage intake: without this index both would scan every charge.
CREATE INDEX pricing_charges_occurred ON pricing_charges (occurred_at);
