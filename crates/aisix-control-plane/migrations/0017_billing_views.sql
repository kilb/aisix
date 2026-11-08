-- A member's own-calls listing (`GET /api/usage/calls`, `/api/usage/summary`
-- as a member) reads the charges filed under the member's id, newest first on
-- `seq`. The tenant-wide listing is served by `pricing_charges_tenant_seq`.
CREATE INDEX pricing_charges_user_seq ON pricing_charges (user_id, seq);
