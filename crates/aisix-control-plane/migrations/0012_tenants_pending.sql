-- The tenants waiting for approval, which signup counts under its lock
-- against their cap (`members::MAX_PENDING_TENANTS`) and the operators list
-- with `?status=pending`.
CREATE INDEX tenants_pending ON tenants (created_at) WHERE status = 'pending';
