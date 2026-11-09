-- A tenant's model access is an explicit scope: every public model (`all`),
-- or the list it holds (`selected`). A new tenant of either origin starts on
-- `all`, which `tenants::insert` writes explicitly rather than relying on a
-- column default.
--
-- The column's own default is `selected`, the fail-closed reading: a row
-- written without a scope — by an older replica during a rolling upgrade, or
-- by a restore that omits the column — keeps exactly the list it carries,
-- and lists nothing therefore reaches nothing. The opposite default would
-- turn every such row into one that reaches the whole catalogue.
--
-- The upgrade reads the stored grants: a tenant that already lists models
-- becomes `selected` with exactly those, and one that lists none becomes
-- `all`. A tenant row's absence of a grant is not a statement about the
-- public models, which is what it meant before this release, so the two are
-- distinguished by what it lists rather than by when it was created.
--
-- The platform's own tenant is the one exception: it lists nothing and its
-- keys are the platform's, so making it `all` would widen them from the
-- hand-made models an operator grants by id to every public model as well.
-- It stays `selected` with its (normally empty) list, which is what it
-- reaches today. `tenants::insert` writes the same scope for it on a fresh
-- install, so an upgraded database and a new one agree.
ALTER TABLE tenants ADD COLUMN model_scope text NOT NULL DEFAULT 'selected'
  CONSTRAINT tenants_model_scope CHECK (model_scope IN ('all', 'selected'));

-- Widening a tenant from nothing to every public model is a change of access,
-- and it is recorded like any other: one `tenant.model_scope_widened` audit
-- row per tenant widened, naming the tenant, its name, and the scope it moved
-- from and to.
--
-- The statement speaks for the owner's rule (「不授权时默认使用全部模型」, the
-- switch explicit and all the default): before this release an empty list
-- meant "no model", and after it an empty list under `all` means "every public
-- model", so a tenant left on the old reading is read as the new default
-- rather than carried over as a block nobody asked for. A tenant that must
-- stay blocked is set to `selected` with an empty list, which reaches nothing
-- and says so; `GET /admin/tenants/{id}` and the Dashboard show the scope.
--
-- The UPDATE is a data-modifying CTE: the audit rows and the scope change are
-- written by one statement, in one transaction, so a database cannot end up
-- widened without a record of it. It runs once, when this migration is
-- applied; nothing repeats it.
WITH widened AS (
  INSERT INTO audit_log (actor_kind, action, target_kind, target_id, tenant_id, detail)
  SELECT 'system', 'tenant.model_scope_widened', 'tenant', t.id::text, t.id,
         jsonb_build_object('name', t.name, 'from', 'selected', 'to', 'all')
    FROM tenants t
   WHERE NOT t.is_internal AND cardinality(t.allowed_model_ids) = 0
  RETURNING tenant_id
)
UPDATE tenants SET model_scope = 'all'
  WHERE id IN (SELECT tenant_id FROM widened);
