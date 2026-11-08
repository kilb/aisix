-- Each key's one provider group, chosen at creation (keys::build_key_doc
-- reads it from the next release on) and fixed once set. Backfilled only
-- where one group is the only reading: a key whose override names exactly
-- one group, and a customer key with no override whose tenant has exactly
-- one. A customer key with any other reading is left NULL and closed until a
-- group is set once; every internal key left without one is given the
-- designated default group (owner answer 2), the one an upgrade creates when
-- none is designated, so it keeps serving and is charged from then on. A
-- revoked key takes none: it is neither routed nor priced.
--
-- The `ALTER TABLE` holds ACCESS EXCLUSIVE on `tenant_keys` until this
-- migration commits, so every key read and write waits that long. The column
-- is nullable with no default, so the DDL itself is metadata-only, and the
-- partial index keeps its build cheap; a very large key table still wants a
-- maintenance window, and this migration is never re-run by hand.
ALTER TABLE tenant_keys ADD COLUMN group_id uuid REFERENCES provider_groups(id);
-- For the per-group key counts a grouped key's page and a group's key list
-- need (Tasks 11 and 12). `references` reads it too, as one side of its OR,
-- though its join to the name half can only use it as a bitmap.
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
-- list it. `[]` for a group no provider key names yet, rather than leaving
-- the operator to guess.
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
         AND jsonb_typeof(r.doc->'groups') = 'array' AND r.doc->'groups' ? g.name),
      '[]'::jsonb)) AS detail
)
-- The assignment above always runs; the row is written only when it gave a
-- key a group. A row recording that nothing changed breaks the log's own
-- rule ("a change is never without its row and a row never without its
-- change"), and would make the row's absence meaningless on a database where
-- the next upgrade should have written one. A fresh install migrates before
-- `ensure_internal_tenant` creates the tenant, so it assigns no key and
-- writes no row.
INSERT INTO audit_log (actor_kind, action, target_kind, target_id, detail)
SELECT 'system', 'provider_group.default_assigned', 'provider_group', g.id::text,
       reported.detail || jsonb_build_object('name', g.name)
FROM provider_groups g, reported
WHERE g.is_default AND reported.detail->>'keys' <> '0';
