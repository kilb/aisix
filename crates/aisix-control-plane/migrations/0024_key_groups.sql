-- Each key's one provider group, chosen at creation (keys::build_key_doc
-- reads it from the next release on) and fixed once set. Backfilled only
-- where one group is the only reading: a key whose override names exactly
-- one group, and a customer key with no override whose tenant has exactly
-- one. A customer key with any other reading is left NULL and closed until a
-- group is set once; every internal key left without one is given the
-- designated default group (owner answer 2), the one an upgrade creates when
-- none is designated, so it keeps serving and is charged from then on. A
-- revoked key takes none: it is neither routed nor priced.
ALTER TABLE tenant_keys ADD COLUMN group_id uuid REFERENCES provider_groups(id);
-- `references` counts live keys only, which is what the partial index covers.
CREATE INDEX tenant_keys_group ON tenant_keys (group_id) WHERE revoked_at IS NULL;
UPDATE tenant_keys k SET group_id = g.id FROM provider_groups g
  WHERE k.revoked_at IS NULL AND k.group_id IS NULL
    AND cardinality(k.allowed_provider_groups) = 1 AND g.name = k.allowed_provider_groups[1];
UPDATE tenant_keys k SET group_id = g.id FROM tenants t, provider_groups g
  WHERE k.tenant_id = t.id AND NOT t.is_internal AND k.revoked_at IS NULL AND k.group_id IS NULL
    AND coalesce(cardinality(k.allowed_provider_groups), 0) = 0
    AND cardinality(t.allowed_provider_groups) = 1 AND g.name = t.allowed_provider_groups[1];
-- The internal tenant's keys, and one audit row naming what the default
-- group reaches: the keys it was given, and the live upstream accounts that
-- list it. `[]` for a group no provider key names yet (routio.ai's shape),
-- rather than leaving the operator to guess.
WITH assigned AS (
  UPDATE tenant_keys k SET group_id = g.id FROM tenants t, provider_groups g
    WHERE k.tenant_id = t.id AND t.is_internal AND k.revoked_at IS NULL AND k.group_id IS NULL
      AND g.is_default
    RETURNING k.id
), reported AS (
  SELECT jsonb_build_object(
    'keys', (SELECT count(*) FROM assigned),
    'provider_keys', COALESCE(
      (SELECT jsonb_agg(r.doc->>'display_name' ORDER BY r.doc->>'display_name', r.id)
       FROM gw_resources r, provider_groups g
       WHERE r.kind = 'provider_keys' AND NOT r.deleted AND g.is_default
         AND r.doc->'groups' ? g.name),
      '[]'::jsonb)) AS detail
)
INSERT INTO audit_log (actor_kind, action, target_kind, target_id, detail)
SELECT 'system', 'provider_group.default_assigned', 'provider_group', g.id::text,
       reported.detail || jsonb_build_object('name', g.name)
FROM provider_groups g, reported WHERE g.is_default;
