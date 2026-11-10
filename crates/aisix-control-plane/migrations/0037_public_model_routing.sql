-- The routing settings an operator set on a public model, which belong to the
-- routing model the public model generates rather than to any account that
-- serves it.
--
-- Only the NON-target settings are stored. `targets` is generated from
-- `channel_models` on every write (`channels::routing_doc`), so it can never
-- drift from the account rows; a stored copy could. A key that is absent
-- means "the gateway's own default", and a setting equal to that default is
-- dropped before it is stored, so a default that moves in a later gateway
-- release reaches every public model that never overrode it.
--
-- Additive and defaulted: a row written before this column reads as `{}`,
-- which is exactly what the generator wrote for it already — `strategy:
-- round_robin` over the generated targets — so no existing public model
-- changes behaviour.
ALTER TABLE public_models
  ADD COLUMN routing jsonb NOT NULL DEFAULT '{}';
