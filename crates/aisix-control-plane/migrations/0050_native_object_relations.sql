-- 来源、归属与集合成员身份分离；旧 parent_id 保留供滚动升级读取。
ALTER TABLE native_objects DROP CONSTRAINT native_objects_kind_check;
ALTER TABLE native_objects ADD CONSTRAINT native_objects_kind_check CHECK (kind IN (
  'response','response_item','response_container','file','message_batch','chat_completion','upload',
  'conversation','container_file','vector_store','vector_store_file_batch','skill','voice','voice_consent',
  'message_container','eval','eval_run','eval_output_item','fine_tuning_job','fine_tuning_checkpoint',
  'fine_tuned_model','video','video_character','realtime_call'
));
ALTER TABLE native_objects ADD COLUMN membership_epoch bigint NOT NULL DEFAULT 0;
CREATE TABLE native_object_memberships (
  id uuid PRIMARY KEY,
  environment_id uuid NOT NULL,
  parent_id uuid NOT NULL REFERENCES native_objects(id),
  child_id uuid NOT NULL REFERENCES native_objects(id),
  relation text NOT NULL CHECK (relation IN ('conversation_item','container_file','vector_store_file',
    'vector_store_batch','vector_store_batch_file','response_output','eval_run','eval_output',
    'fine_tuning_checkpoint','fine_tuning_result','fine_tuned_model','video_source','video_character')),
  created_operation_id uuid NOT NULL,
  generation bigint NOT NULL CHECK (generation > 0),
  version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
  deleted_at timestamptz,
  FOREIGN KEY (environment_id,created_operation_id) REFERENCES native_operations(environment_id,operation_id),
  UNIQUE(environment_id,parent_id,child_id,relation,generation),
  CHECK(parent_id <> child_id)
);
CREATE UNIQUE INDEX native_membership_live ON native_object_memberships(environment_id,parent_id,child_id,relation) WHERE deleted_at IS NULL;
CREATE TABLE native_parent_fences (
  environment_id uuid NOT NULL,
  parent_id uuid NOT NULL REFERENCES native_objects(id),
  operation_id uuid NOT NULL,
  state text NOT NULL CHECK(state IN ('held','released','uncertain')),
  PRIMARY KEY(environment_id,parent_id,operation_id)
);
CREATE UNIQUE INDEX native_parent_fence_live ON native_parent_fences(environment_id,parent_id) WHERE state IN ('held','uncertain');
CREATE TABLE native_relation_reservations (
  environment_id uuid NOT NULL,
  operation_id uuid NOT NULL,
  dp_id uuid NOT NULL,
  api_key_id uuid NOT NULL,
  parent_id uuid NOT NULL REFERENCES native_objects(id),
  relation text NOT NULL,
  action text NOT NULL CHECK(action IN ('attach','detach','discover')),
  parent_epoch bigint NOT NULL,
  parent_version bigint NOT NULL,
  child_ids uuid[] NOT NULL,
  state text NOT NULL CHECK(state IN ('reserved','committed','uncertain','rejected')),
  payload jsonb,
  PRIMARY KEY(environment_id,operation_id)
);
CREATE UNIQUE INDEX native_relation_reservation_live ON native_relation_reservations(environment_id,parent_id) WHERE state IN ('reserved','uncertain') AND action <> 'discover';
CREATE TABLE native_related_operations (
  environment_id uuid NOT NULL,
  operation_id uuid NOT NULL,
  manifest jsonb NOT NULL,
  owner_record_id uuid REFERENCES native_objects(id),
  bound_reply jsonb,
  PRIMARY KEY(environment_id,operation_id),
  FOREIGN KEY(environment_id,operation_id) REFERENCES native_operations(environment_id,operation_id)
);
CREATE TABLE native_operation_references (
  environment_id uuid NOT NULL,
  operation_id uuid NOT NULL,
  record_id uuid NOT NULL REFERENCES native_objects(id),
  record_version bigint NOT NULL,
  parent_epoch bigint NOT NULL,
  consumer_family text NOT NULL,
  PRIMARY KEY(environment_id,operation_id,record_id),
  FOREIGN KEY(environment_id,operation_id) REFERENCES native_operations(environment_id,operation_id)
);
-- 单次历史迁移之外，旧 Bind 写入也在应用事务中建立同一关系。
INSERT INTO native_object_memberships(id,environment_id,parent_id,child_id,relation,created_operation_id,generation)
SELECT gen_random_uuid(),child.environment_id,parent.id,child.id,'response_output',parent.operation_id,1
FROM native_objects child JOIN native_objects parent ON child.parent_id=parent.id
WHERE parent.kind='response' AND child.kind IN ('response_item','response_container');
CREATE FUNCTION native_membership_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF (NEW.id,NEW.environment_id,NEW.parent_id,NEW.child_id,NEW.relation,NEW.created_operation_id,NEW.generation)
    IS DISTINCT FROM (OLD.id,OLD.environment_id,OLD.parent_id,OLD.child_id,OLD.relation,OLD.created_operation_id,OLD.generation)
    OR (OLD.deleted_at IS NOT NULL AND NEW.deleted_at IS DISTINCT FROM OLD.deleted_at) THEN
    RAISE EXCEPTION 'native membership identity is immutable' USING ERRCODE='check_violation';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER native_membership_immutable BEFORE UPDATE ON native_object_memberships FOR EACH ROW EXECUTE FUNCTION native_membership_immutable();

CREATE TABLE native_delete_reservations (
 environment_id uuid NOT NULL, operation_id uuid NOT NULL, dp_id uuid NOT NULL, api_key_id uuid NOT NULL,
 record_id uuid NOT NULL REFERENCES native_objects(id), object_version bigint NOT NULL,
 state text NOT NULL CHECK(state IN ('reserved','committed','uncertain','rejected')),
 PRIMARY KEY(environment_id,operation_id)
);
CREATE TABLE native_observed_entries (
 environment_id uuid NOT NULL, owner_record_id uuid NOT NULL REFERENCES native_objects(id),
 entry_id text NOT NULL, payload jsonb NOT NULL, reply jsonb NOT NULL,
 PRIMARY KEY(environment_id,owner_record_id,entry_id)
);

CREATE TABLE native_operation_origin_sources (
 environment_id uuid NOT NULL, operation_id uuid NOT NULL, source_record_id uuid NOT NULL REFERENCES native_objects(id),
 PRIMARY KEY(environment_id,operation_id),
 FOREIGN KEY(environment_id,operation_id) REFERENCES native_operations(environment_id,operation_id)
);
-- 接受凭据和回执只能追加；状态推进不允许重写准入证明。
CREATE FUNCTION native_relation_proof_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_TABLE_NAME='native_parent_fences' THEN
  IF (to_jsonb(NEW)-'state') IS DISTINCT FROM (to_jsonb(OLD)-'state') OR (OLD.state='released' AND NEW.state<>'released') THEN
   RAISE EXCEPTION 'native parent fence is immutable' USING ERRCODE='check_violation';
  END IF;
 ELSIF TG_TABLE_NAME IN ('native_relation_reservations','native_delete_reservations') THEN
  IF (to_jsonb(NEW)-'state'-'payload') IS DISTINCT FROM (to_jsonb(OLD)-'state'-'payload')
    OR (OLD.state IN ('committed','rejected') AND to_jsonb(NEW) IS DISTINCT FROM to_jsonb(OLD)) THEN
   RAISE EXCEPTION 'native reservation is immutable' USING ERRCODE='check_violation';
  END IF;
 ELSIF TG_TABLE_NAME='native_related_operations' THEN
  IF (NEW.environment_id,NEW.operation_id,NEW.manifest,NEW.owner_record_id) IS DISTINCT FROM (OLD.environment_id,OLD.operation_id,OLD.manifest,OLD.owner_record_id)
    OR (OLD.bound_reply IS NOT NULL AND NEW.bound_reply IS DISTINCT FROM OLD.bound_reply) THEN
   RAISE EXCEPTION 'native related submission is immutable' USING ERRCODE='check_violation';
  END IF;
 ELSE
  IF to_jsonb(NEW) IS DISTINCT FROM to_jsonb(OLD) THEN
   RAISE EXCEPTION 'native accepted proof is immutable' USING ERRCODE='check_violation';
  END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER native_parent_fence_immutable BEFORE UPDATE ON native_parent_fences FOR EACH ROW EXECUTE FUNCTION native_relation_proof_immutable();
CREATE TRIGGER native_relation_reservation_immutable BEFORE UPDATE ON native_relation_reservations FOR EACH ROW EXECUTE FUNCTION native_relation_proof_immutable();
CREATE TRIGGER native_delete_reservation_immutable BEFORE UPDATE ON native_delete_reservations FOR EACH ROW EXECUTE FUNCTION native_relation_proof_immutable();
CREATE TRIGGER native_related_operation_immutable BEFORE UPDATE ON native_related_operations FOR EACH ROW EXECUTE FUNCTION native_relation_proof_immutable();
CREATE TRIGGER native_operation_reference_immutable BEFORE UPDATE ON native_operation_references FOR EACH ROW EXECUTE FUNCTION native_relation_proof_immutable();
CREATE TRIGGER native_operation_origin_source_immutable BEFORE UPDATE ON native_operation_origin_sources FOR EACH ROW EXECUTE FUNCTION native_relation_proof_immutable();
CREATE TRIGGER native_observed_entry_immutable BEFORE UPDATE ON native_observed_entries FOR EACH ROW EXECUTE FUNCTION native_relation_proof_immutable();

CREATE FUNCTION native_membership_scope() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE p native_objects; c native_objects;
BEGIN
 SELECT * INTO STRICT p FROM native_objects WHERE id=NEW.parent_id;
 SELECT * INTO STRICT c FROM native_objects WHERE id=NEW.child_id;
 IF (p.environment_id,p.tenant_id,p.protocol,p.provider_key_id,p.boundary) IS DISTINCT FROM
    (c.environment_id,c.tenant_id,c.protocol,c.provider_key_id,c.boundary)
    OR NEW.environment_id<>p.environment_id OR p.protocol<>'openai'
    OR NOT (
      (NEW.relation='conversation_item' AND p.kind='conversation' AND c.kind='response_item') OR
      (NEW.relation='container_file' AND p.kind='response_container' AND c.kind='container_file') OR
      (NEW.relation='vector_store_file' AND p.kind='vector_store' AND c.kind='file') OR
      (NEW.relation='vector_store_batch' AND p.kind='vector_store' AND c.kind='vector_store_file_batch') OR
      (NEW.relation='vector_store_batch_file' AND p.kind='vector_store_file_batch' AND c.kind='file') OR
      (NEW.relation='response_output' AND p.kind='response' AND c.kind IN ('response_item','response_container')) OR
      (NEW.relation='eval_run' AND p.kind='eval' AND c.kind='eval_run') OR
      (NEW.relation='eval_output' AND p.kind='eval_run' AND c.kind='eval_output_item') OR
      (NEW.relation='fine_tuning_checkpoint' AND p.kind='fine_tuning_job' AND c.kind='fine_tuning_checkpoint') OR
      (NEW.relation='fine_tuning_result' AND p.kind='fine_tuning_job' AND c.kind='file') OR
      (NEW.relation='fine_tuned_model' AND p.kind IN ('fine_tuning_job','fine_tuning_checkpoint') AND c.kind='fine_tuned_model') OR
      (NEW.relation='video_source' AND p.kind='video' AND c.kind IN ('video','file')) OR
      (NEW.relation='video_character' AND p.kind='video' AND c.kind='video_character')
    ) THEN
   RAISE EXCEPTION 'native membership scope does not match' USING ERRCODE='check_violation';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER native_membership_scope BEFORE INSERT ON native_object_memberships FOR EACH ROW EXECUTE FUNCTION native_membership_scope();
