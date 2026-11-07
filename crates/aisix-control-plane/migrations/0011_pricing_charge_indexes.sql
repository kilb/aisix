-- Indexes for the operators' charge queries (`/admin/pricing/*`).
--
-- `GET /admin/pricing/charges?tenant_id=` and `?api_key_id=` page newest
-- first on `seq`; each reads its `(column, seq)` index backwards from the
-- page's `before`, whatever the column holds. `GET /admin/pricing/status`
-- counts the unpriced charges of the last 24 hours by code; the partial
-- index holds only unpriced rows, ordered by when they were priced, with the
-- code beside it, so the count reads only the window's rows.
CREATE INDEX pricing_charges_tenant_seq ON pricing_charges (tenant_id, seq);
CREATE INDEX pricing_charges_key_seq ON pricing_charges (api_key_id, seq);
CREATE INDEX pricing_charges_unpriced_recent ON pricing_charges (priced_at, error_code)
  WHERE status = 'unpriced';
