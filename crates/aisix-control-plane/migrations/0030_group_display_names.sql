-- 1. A display name of its own for every group still shown by its routing
-- name. A group's `name` is the routing name provider keys are matched on and
-- must never reach a tenant, while its `display_name` is what `/api` shows.
-- 0023 created the first-boot group `shared` with `display_name = 'shared'`,
-- and gave every backfilled group its name as its display name, so on a fresh
-- install every tenant was shown `shared`. Each such group, retired or not
-- (a retired group's display name still labels the charges billed in it),
-- gets a readable name: `shared`, the group every new tenant is assigned
-- first, becomes 默认分组; a backfilled `default` becomes 默认, the name 0023
-- gives the `default` it creates; and any other becomes 分组 1, 分组 2, ... in
-- the order the groups were created. A display name an operator chose that
-- differs from the routing name is left alone. An operator can rename any of
-- them; the routing names are unchanged.
UPDATE provider_groups SET display_name = '默认分组'
  WHERE name = 'shared' AND display_name = name;
UPDATE provider_groups SET display_name = '默认'
  WHERE name = 'default' AND display_name = name;
UPDATE provider_groups g SET display_name = '分组 ' || n.ordinal
  FROM (SELECT id, row_number() OVER (ORDER BY created_at, id) AS ordinal
        FROM provider_groups WHERE display_name = name) n
  WHERE g.id = n.id;

-- 2. A confirmed proposal names its attester. 0026's trigger refuses
-- `attested_by` on anything but a confirmation; this is the other half: no
-- confirmation without one. The confirmation route is the only write that
-- confirms a proposal, and has set `attested_by` in that same update since
-- 0026 shipped, so no confirmed row without an attester can exist and the
-- constraint is validated against every row as it is added.
ALTER TABLE pricing_import_proposals ADD CONSTRAINT pricing_import_proposals_attested
  CHECK ((status = 'confirmed') = (attested_by IS NOT NULL));
