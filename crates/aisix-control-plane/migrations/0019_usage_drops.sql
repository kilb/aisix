-- The usage events each gateway instance reports it dropped since its process
-- started, as an object of reason to count (`{"sink_full": 7}`). Every
-- heartbeat replaces it, like the lists of 0005: a beat without the field,
-- or with anything but an object of non-negative integers, stores `{}`.
ALTER TABLE dp_instances ADD COLUMN usage_event_drops jsonb NOT NULL DEFAULT '{}';
