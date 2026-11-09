-- 1. A display name of its own for the two first-boot groups. A group's
-- `name` is the routing name provider keys are matched on and must never
-- reach a tenant, while its `display_name` is what `/api` shows. 0023 gave
-- every group it backfilled its name as its display name, so on a fresh
-- install every tenant was shown `shared`. Two rows are renamed, each only
-- while its display name is still its routing name:
--   - `shared`, the group every new tenant is assigned first, becomes
--     通用分组 (not 默认分组, which already labels the designated default
--     group and the tenant defaults);
--   - a backfilled `default` becomes 默认, the name 0023 gives the `default`
--     it inserts itself.
-- No other group is renamed: a group an operator created, whatever its
-- display name, keeps it. The routing names are unchanged.
UPDATE provider_groups SET display_name = '通用分组'
  WHERE name = 'shared' AND display_name = name;
UPDATE provider_groups SET display_name = '默认'
  WHERE name = 'default' AND display_name = name;

-- 2. A confirmed proposal names its attester. 0026's trigger refuses
-- `attested_by` on anything but a confirmation; this is the other half: no
-- confirmation without one. The confirmation route is the only write that
-- confirms a proposal, and has set `attested_by` in that same update since
-- 0026 shipped, so no confirmed row without an attester can exist and the
-- constraint is validated against every row as it is added.
ALTER TABLE pricing_import_proposals ADD CONSTRAINT pricing_import_proposals_attested
  CHECK ((status = 'confirmed') = (attested_by IS NOT NULL));
