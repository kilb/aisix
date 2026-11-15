-- 单机共享文件事务；raw_id 与 boundary 一起限定身份，租约绝不靠进程内锁。
CREATE TABLE IF NOT EXISTS native_meta (name TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS native_operations (
 operation_id TEXT PRIMARY KEY, owner TEXT NOT NULL, caller TEXT NOT NULL DEFAULT '{}', submission TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('accepted','bound','rejected','uncertain')), related TEXT NOT NULL,
 root_id TEXT, worker_id TEXT NOT NULL, telemetry TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS native_objects (
 record_id TEXT PRIMARY KEY, operation_id TEXT NOT NULL UNIQUE REFERENCES native_operations(operation_id),
 protocol TEXT NOT NULL, kind TEXT NOT NULL, boundary TEXT NOT NULL, raw_id TEXT NOT NULL,
 owner TEXT NOT NULL, record TEXT NOT NULL, pending INTEGER NOT NULL DEFAULT 0,
 poll_after INTEGER NOT NULL DEFAULT 0, lease_worker TEXT, lease_until INTEGER,
 lease_generation INTEGER NOT NULL DEFAULT 0, terminal TEXT,
 UNIQUE(protocol,kind,boundary,raw_id)
);
CREATE TRIGGER IF NOT EXISTS native_object_identity_immutable BEFORE UPDATE OF
 operation_id,protocol,kind,boundary,raw_id,owner ON native_objects
BEGIN SELECT RAISE(ABORT,'immutable native object identity'); END;
CREATE TABLE IF NOT EXISTS native_reservations (
 operation_id TEXT PRIMARY KEY, request TEXT NOT NULL, parent_id TEXT NOT NULL,
 parent_version INTEGER NOT NULL, owner TEXT NOT NULL, submission TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('reserved','uncertain','committed','rejected')),
 exclusive INTEGER NOT NULL CHECK(exclusive IN (0,1)), result TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS native_parent_reservation ON native_reservations(parent_id)
 WHERE exclusive=1 AND state IN ('reserved','uncertain');
CREATE TABLE IF NOT EXISTS native_memberships (
 membership_id TEXT PRIMARY KEY, parent_id TEXT NOT NULL REFERENCES native_objects(record_id),
 child_id TEXT NOT NULL REFERENCES native_objects(record_id), relation TEXT NOT NULL,
 generation INTEGER NOT NULL, version INTEGER NOT NULL DEFAULT 1,
 deleted INTEGER NOT NULL DEFAULT 0, operation_id TEXT NOT NULL,
 UNIQUE(parent_id,child_id,relation,generation)
);
CREATE UNIQUE INDEX IF NOT EXISTS native_membership_active ON native_memberships(parent_id,child_id,relation)
 WHERE deleted=0;
CREATE TRIGGER IF NOT EXISTS native_membership_identity_immutable BEFORE UPDATE OF
 parent_id,child_id,relation,generation,operation_id ON native_memberships
BEGIN SELECT RAISE(ABORT,'immutable native membership identity'); END;
CREATE TABLE IF NOT EXISTS native_terminal_outbox (
 event_id TEXT PRIMARY KEY, record_id TEXT NOT NULL REFERENCES native_objects(record_id),
 entry_id TEXT NOT NULL, submission TEXT NOT NULL, owner TEXT NOT NULL, usage TEXT NOT NULL,
 delivered INTEGER NOT NULL DEFAULT 0, delivery_worker TEXT, delivery_until INTEGER, delivery_generation INTEGER NOT NULL DEFAULT 0, event TEXT NOT NULL, UNIQUE(record_id,entry_id)
);
PRAGMA user_version=1;
CREATE TABLE IF NOT EXISTS native_operation_parents (
 operation_id TEXT NOT NULL REFERENCES native_operations(operation_id),
 parent_id TEXT NOT NULL REFERENCES native_objects(record_id), version INTEGER NOT NULL,
 active INTEGER NOT NULL DEFAULT 1, PRIMARY KEY(operation_id,parent_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS native_operation_parent_active ON native_operation_parents(parent_id) WHERE active=1;
CREATE TABLE IF NOT EXISTS native_deletions (
 operation_id TEXT PRIMARY KEY, request TEXT NOT NULL, parent_id TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('reserved','uncertain','committed','rejected'))
);
CREATE UNIQUE INDEX IF NOT EXISTS native_delete_active ON native_deletions(parent_id) WHERE state IN ('reserved','uncertain');
CREATE TABLE IF NOT EXISTS native_reconciliation (
 receipt_id TEXT PRIMARY KEY, target_id TEXT NOT NULL, request TEXT NOT NULL,
 created_at INTEGER NOT NULL, UNIQUE(target_id,request)
);

CREATE TABLE IF NOT EXISTS native_reservation_references (
 operation_id TEXT NOT NULL REFERENCES native_reservations(operation_id),
 record_id TEXT NOT NULL REFERENCES native_objects(record_id), version INTEGER NOT NULL,
 PRIMARY KEY(operation_id,record_id)
);
CREATE TRIGGER IF NOT EXISTS native_operation_identity_immutable BEFORE UPDATE OF
 owner,caller,submission,related,worker_id,telemetry ON native_operations
BEGIN SELECT RAISE(ABORT,'immutable native operation identity'); END;
CREATE TRIGGER IF NOT EXISTS native_record_identity_immutable BEFORE UPDATE OF record ON native_objects
WHEN json_extract(OLD.record,'$.record_id') IS NOT json_extract(NEW.record,'$.record_id')
 OR json_extract(OLD.record,'$.operation_id') IS NOT json_extract(NEW.record,'$.operation_id')
 OR json_extract(OLD.record,'$.api_key_id') IS NOT json_extract(NEW.record,'$.api_key_id')
 OR json_extract(OLD.record,'$.protocol') IS NOT json_extract(NEW.record,'$.protocol')
 OR json_extract(OLD.record,'$.kind') IS NOT json_extract(NEW.record,'$.kind')
 OR json_extract(OLD.record,'$.origin') IS NOT json_extract(NEW.record,'$.origin')
 OR json_extract(OLD.record,'$.raw_id') IS NOT json_extract(NEW.record,'$.raw_id')
 OR json_extract(OLD.record,'$.parent_id') IS NOT json_extract(NEW.record,'$.parent_id')
BEGIN SELECT RAISE(ABORT,'immutable native record identity'); END;
CREATE TABLE IF NOT EXISTS native_bindings (
 operation_id TEXT PRIMARY KEY REFERENCES native_operations(operation_id),
 request TEXT NOT NULL, reply TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS native_observed_entries (
 record_id TEXT NOT NULL REFERENCES native_objects(record_id), entry_id TEXT NOT NULL,
 payload TEXT NOT NULL, reply TEXT NOT NULL, PRIMARY KEY(record_id,entry_id)
);

CREATE TABLE IF NOT EXISTS native_metering_records (
 operation_id TEXT PRIMARY KEY REFERENCES native_operations(operation_id),
 object_id TEXT REFERENCES native_objects(record_id), state TEXT NOT NULL,
 terminal_status TEXT, reason TEXT, usage TEXT NOT NULL, artifacts TEXT NOT NULL,
 receipt_version INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS native_metering_receipts (
 operation_id TEXT NOT NULL REFERENCES native_operations(operation_id), version INTEGER NOT NULL,
 receipt TEXT NOT NULL, PRIMARY KEY(operation_id,version)
);
CREATE TRIGGER IF NOT EXISTS native_metering_receipt_immutable_update BEFORE UPDATE ON native_metering_receipts
BEGIN SELECT RAISE(ABORT,'immutable native metering receipt'); END;
CREATE TRIGGER IF NOT EXISTS native_metering_receipt_immutable_delete BEFORE DELETE ON native_metering_receipts
BEGIN SELECT RAISE(ABORT,'immutable native metering receipt'); END;

CREATE TRIGGER IF NOT EXISTS native_terminal_payload_immutable BEFORE UPDATE OF
 event_id,record_id,entry_id,submission,owner,usage,event ON native_terminal_outbox
BEGIN SELECT RAISE(ABORT,'immutable native terminal payload'); END;

-- 已完成历史不进入后台租约和终态投递的轮询范围。
CREATE INDEX IF NOT EXISTS native_objects_poll_pending
 ON native_objects(poll_after,record_id) WHERE pending=1;
CREATE INDEX IF NOT EXISTS native_terminal_delivery_pending
 ON native_terminal_outbox(event_id) WHERE delivered=0;
