-- 实时扩展与主归属表共用同一 BEGIN IMMEDIATE；不改变 user_version 或新增投递器。
CREATE TABLE IF NOT EXISTS native_realtime_credentials (
 credential_id TEXT PRIMARY KEY, operation_id TEXT NOT NULL UNIQUE REFERENCES native_operations(operation_id),
 token_hash TEXT NOT NULL UNIQUE, key_id TEXT NOT NULL, owner TEXT NOT NULL,
 creator_key_hash TEXT NOT NULL, purpose TEXT NOT NULL,
 expires_at TEXT NOT NULL, expiry_ms INTEGER NOT NULL, defaults TEXT NOT NULL, revoked_at INTEGER,
 CHECK(length(token_hash)=64), CHECK(purpose IN ('realtime','transcription','translation'))
);
CREATE TRIGGER IF NOT EXISTS native_realtime_credential_identity BEFORE UPDATE OF
 credential_id,operation_id,token_hash,key_id,owner,creator_key_hash,purpose,expires_at,expiry_ms,defaults
 ON native_realtime_credentials
BEGIN SELECT RAISE(ABORT,'immutable native realtime credential'); END;
CREATE TRIGGER IF NOT EXISTS native_realtime_credential_revocation BEFORE UPDATE OF revoked_at
 ON native_realtime_credentials WHEN OLD.revoked_at IS NOT NULL AND NEW.revoked_at IS NOT OLD.revoked_at
BEGIN SELECT RAISE(ABORT,'immutable native realtime credential revocation'); END;
CREATE TRIGGER IF NOT EXISTS native_realtime_credential_no_delete BEFORE DELETE ON native_realtime_credentials
BEGIN SELECT RAISE(ABORT,'immutable native realtime credential'); END;

CREATE TABLE IF NOT EXISTS native_live_sessions (
 operation_id TEXT PRIMARY KEY REFERENCES native_operations(operation_id), api_key_id TEXT NOT NULL,
 owner TEXT NOT NULL, credential_id TEXT REFERENCES native_realtime_credentials(credential_id),
 purpose TEXT NOT NULL, transport TEXT NOT NULL, provider_session_id TEXT,
 state TEXT NOT NULL, coverage TEXT NOT NULL, accounting_state TEXT NOT NULL,
 lease_worker TEXT NOT NULL, lease_generation INTEGER NOT NULL, lease_expires_at INTEGER NOT NULL,
 deadline_at TEXT NOT NULL, deadline_ms INTEGER NOT NULL, closed_at INTEGER,
 CHECK(purpose IN ('realtime','transcription','translation')),
 CHECK(transport IN ('web_socket','web_rtc','sip')),
 CHECK(state IN ('live','requires_stop','closed')),
 CHECK(coverage IN ('partial','complete','unknown')),
 CHECK(accounting_state IN ('pending','observed','awaiting_reconciliation')),
 CHECK(lease_generation>0), CHECK((state='closed')=(closed_at IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS native_live_recovery ON native_live_sessions(lease_expires_at,operation_id)
 WHERE state<>'closed';
CREATE TRIGGER IF NOT EXISTS native_live_session_identity BEFORE UPDATE OF
 operation_id,api_key_id,owner,credential_id,purpose,transport,provider_session_id,deadline_at,deadline_ms
 ON native_live_sessions
BEGIN SELECT RAISE(ABORT,'immutable native live session'); END;
CREATE TRIGGER IF NOT EXISTS native_live_session_no_delete BEFORE DELETE ON native_live_sessions
BEGIN SELECT RAISE(ABORT,'immutable native live session'); END;

CREATE TABLE IF NOT EXISTS native_live_entries (
 operation_id TEXT NOT NULL REFERENCES native_live_sessions(operation_id), entry_id TEXT NOT NULL,
 entry TEXT NOT NULL, PRIMARY KEY(operation_id,entry_id), CHECK(length(entry_id) BETWEEN 1 AND 256)
);
CREATE TRIGGER IF NOT EXISTS native_live_entry_no_update BEFORE UPDATE ON native_live_entries
BEGIN SELECT RAISE(ABORT,'immutable native live entry'); END;
CREATE TRIGGER IF NOT EXISTS native_live_entry_no_delete BEFORE DELETE ON native_live_entries
BEGIN SELECT RAISE(ABORT,'immutable native live entry'); END;

-- 日志本身就是本地不可变事实；已授权执行身份与用量一起保存，不重新套当前模型。
CREATE TABLE IF NOT EXISTS native_live_journal (
 journal_id TEXT PRIMARY KEY, operation_id TEXT NOT NULL REFERENCES native_live_sessions(operation_id),
 event_id TEXT NOT NULL, response_id TEXT, observer_worker TEXT NOT NULL,
 usage TEXT NOT NULL, raw_usage TEXT, coverage TEXT NOT NULL, terminal INTEGER NOT NULL,
 accepted TEXT NOT NULL, created_at INTEGER NOT NULL,
 UNIQUE(operation_id,event_id), UNIQUE(operation_id,response_id),
 CHECK(length(event_id) BETWEEN 1 AND 512), CHECK(terminal IN (0,1)),
 CHECK(coverage IN ('partial','complete','unknown')), CHECK(json_valid(usage) AND json_type(usage)='array')
);
CREATE TRIGGER IF NOT EXISTS native_live_journal_no_update BEFORE UPDATE ON native_live_journal
BEGIN SELECT RAISE(ABORT,'immutable native live journal'); END;
CREATE TRIGGER IF NOT EXISTS native_live_journal_no_delete BEFORE DELETE ON native_live_journal
BEGIN SELECT RAISE(ABORT,'immutable native live journal'); END;

CREATE TABLE IF NOT EXISTS native_sip_intakes (
 boundary TEXT NOT NULL, provider_event_id TEXT NOT NULL, webhook_id TEXT NOT NULL,
 call_id TEXT NOT NULL, route_id TEXT NOT NULL, provider_key_id TEXT NOT NULL,
 operation_id TEXT NOT NULL REFERENCES native_live_sessions(operation_id),
 PRIMARY KEY(boundary,provider_event_id), UNIQUE(boundary,call_id), UNIQUE(boundary,webhook_id)
);
CREATE TRIGGER IF NOT EXISTS native_sip_intake_no_update BEFORE UPDATE ON native_sip_intakes
BEGIN SELECT RAISE(ABORT,'immutable native SIP intake'); END;
CREATE TRIGGER IF NOT EXISTS native_sip_intake_no_delete BEFORE DELETE ON native_sip_intakes
BEGIN SELECT RAISE(ABORT,'immutable native SIP intake'); END;
