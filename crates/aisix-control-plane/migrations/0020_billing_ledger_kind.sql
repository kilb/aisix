-- A ledger listing filtered by kind pages on seq within one kind of one
-- tenant; without this index a sparse kind (top-ups, adjustments) would walk
-- the tenant's whole ledger of usage entries.
CREATE INDEX billing_ledger_tenant_kind_seq ON billing_ledger (tenant_id, kind, seq);
