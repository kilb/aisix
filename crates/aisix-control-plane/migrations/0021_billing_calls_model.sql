-- Indexes for the call log's `model_id` filter and a member's summaries.
-- `(tenant_id, requested_model_id, seq)` serves `?model_id=` newest first
-- without walking the tenant's whole charge history; `(user_id,
-- occurred_at)` lets a member's usage summary read only its own rows in the
-- range instead of the tenant's.
CREATE INDEX pricing_charges_tenant_model_seq ON pricing_charges (tenant_id, requested_model_id, seq);
CREATE INDEX pricing_charges_user_at ON pricing_charges (user_id, occurred_at);
