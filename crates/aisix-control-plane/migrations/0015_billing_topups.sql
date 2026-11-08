-- Top-ups: money coming into a tenant's balance. A tenant's owner, admin or
-- billing member requests one (`origin 'tenant'`, `pending`), and an operator
-- approves it, posting one `topup` ledger entry in the same transaction, or
-- rejects it with a reason. An operator credit is inserted already approved
-- (`origin 'operator'`, with its `credit_reason`). A row leaves `pending`
-- once and never returns to it: the decision is an `UPDATE … WHERE status =
-- 'pending'`, so a second decision finds no row, and each top-up's ledger
-- entry is unique on `topup_id`.
CREATE TABLE billing_topups (
  id uuid PRIMARY KEY, seq bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
  tenant_id uuid NOT NULL REFERENCES billing_accounts(tenant_id),
  amount_micro_usd bigint NOT NULL CHECK (amount_micro_usd > 0),
  origin text NOT NULL DEFAULT 'tenant' CHECK (origin IN ('tenant','operator')),
  note text NOT NULL DEFAULT '', reference text NOT NULL DEFAULT '',
  status text NOT NULL CHECK (status IN ('pending','approved','rejected')),
  requested_by uuid REFERENCES members(id), requested_at timestamptz NOT NULL DEFAULT now(),
  decided_by uuid REFERENCES operators(id), decided_at timestamptz, reject_reason text, credit_reason text,
  CHECK ((status = 'pending') = (decided_at IS NULL)),
  CHECK ((status = 'rejected') = (reject_reason IS NOT NULL)),
  CHECK ((origin = 'tenant') = (requested_by IS NOT NULL)),
  CHECK (origin = 'tenant' OR (status = 'approved' AND credit_reason IS NOT NULL AND decided_by IS NOT NULL)));
CREATE INDEX billing_topups_tenant_seq ON billing_topups (tenant_id, seq);
CREATE INDEX billing_topups_pending_seq ON billing_topups (seq) WHERE status = 'pending';
ALTER TABLE billing_ledger ADD CONSTRAINT billing_ledger_topup_fk FOREIGN KEY (topup_id) REFERENCES billing_topups(id);
