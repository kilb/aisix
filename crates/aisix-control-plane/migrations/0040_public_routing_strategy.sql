-- Retires the `least_cost` strategy a public model may hold stored, which
-- this release refuses to store again.
--
-- A public model's targets carry no price of their own: every one is a
-- generated direct document, and the platform prices the model, not the
-- target (`channels::direct_doc`). `least_cost` therefore ranks every
-- target as unpriced and dispatches in the order the targets are listed,
-- which is `round_robin`'s order — so a stored `least_cost` reads as "sort
-- by price" while nothing is sorted by price. The write path refuses it
-- since the strategy became settable (`channels::checked_routing`), but a
-- row stored before that refusal keeps it, and a refusal of new writes is
-- not a repair of stored ones.
--
-- The repair removes the key, which means `round_robin` — the gateway's
-- default and the order that strategy actually produced — and the model's
-- routing document is regenerated from the stored settings by
-- `channels::repair_routing` (boot and every housekeeping tick), which
-- audits each model it rewrites as `public_model.routing_regenerated`. No
-- target, weight or priority changes, so no request is routed differently.
--
-- Additive and idempotent: a row with any other stored strategy, or none, is
-- untouched, and the update matches nothing the second time it runs. The
-- retired rows are included: a retired model keeps its settings, and
-- `least_cost` means the same nothing there.
UPDATE public_models
   SET routing = routing - 'strategy'
 WHERE routing -> 'strategy' = '"least_cost"'::jsonb;
