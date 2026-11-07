-- An index for `GET /admin/pricing/charges?status=`, which pages newest first
-- on `seq` like the tenant and key filters of 0011: `(status, seq)` is read
-- backwards from the page's `before` for any status, so the listing does not
-- depend on the planner choosing another index and filtering.
CREATE INDEX pricing_charges_status_seq ON pricing_charges (status, seq);
