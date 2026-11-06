//! References between gateway documents: what a document points at, which
//! must exist when it is written, and what points at a document, which
//! keeps it from being deleted.
//!
//! References are read in their id form only. Normalisation has rewritten
//! every name-form model reference by the time these run, and every other
//! reference has no name form.

use std::collections::HashMap;

use aisix_core::filesource::{for_each_model_ref_node, model_ref_id_fields};
use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::Kind;
use crate::db::DbError;
use crate::error::{ApiError, Detail};

/// One reference a document makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    /// The JSON pointer of the referencing field.
    pub path: String,
    pub kind: Kind,
    /// As written, which need not be a well-formed id.
    pub id: String,
}

/// A live document that references another, as an `in_use` answer lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Referrer {
    pub kind: Kind,
    pub id: Uuid,
    pub name: Option<String>,
}

fn address(node: &Map<String, Value>) -> usize {
    node as *const Map<String, Value> as usize
}

/// `segment` escaped for a JSON pointer.
pub(crate) fn escaped(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

/// The JSON pointer of every object `for_each_model_ref_node` visits in
/// `doc`, in the order it visits them. `doc` is not changed.
///
/// The shared walk hands over each object without saying where it sits, so
/// each one is recognised by its address among `doc`'s objects.
pub(crate) fn model_ref_node_paths(kind: Kind, doc: &mut Value) -> Vec<String> {
    fn walk(value: &Value, path: &mut String, out: &mut HashMap<usize, String>) {
        let children: Vec<(String, &Value)> = match value {
            Value::Object(fields) => {
                out.insert(address(fields), path.clone());
                fields.iter().map(|(k, v)| (escaped(k), v)).collect()
            }
            Value::Array(items) => items
                .iter()
                .enumerate()
                .map(|(i, v)| (i.to_string(), v))
                .collect(),
            _ => return,
        };
        for (segment, child) in children {
            let len = path.len();
            path.push('/');
            path.push_str(&segment);
            walk(child, path, out);
            path.truncate(len);
        }
    }

    let mut addresses = HashMap::new();
    walk(doc, &mut String::new(), &mut addresses);
    let mut paths = Vec::new();
    for_each_model_ref_node(kind.segment(), doc, &mut |node| {
        paths.push(addresses.get(&address(node)).cloned().unwrap_or_default());
    });
    paths
}

/// Every reference `doc`, a document of `kind`, makes:
/// - a model's `provider_key_id`;
/// - every id-form model reference `aisix_core::filesource` lists for the
///   kind (routing and ensemble members, semantic targets, embedders, a
///   cache policy's model scope, an API key's `allowed_model_ids`);
/// - a guardrail attachment's `guardrail_id`, and its `scope_id` when it is
///   scoped to a model, an API key or a team;
/// - a cache policy scoped `applies_to: "api_key:<id>"`;
/// - a classic rate-limit policy's `scope_ref`, read by its `scope`: a
///   model, an API key, a team (`team` and `team_member`) or a member (a
///   user); the gateway matches it against those ids, so a target that does
///   not exist would be limited by nothing;
/// - an API key's `team_id` and `user_id`.
pub fn outgoing(kind: Kind, doc: &Value) -> Vec<Ref> {
    let mut refs = Vec::new();
    let mut push = |path: String, kind: Kind, value: Option<&Value>| match value {
        Some(Value::String(id)) => refs.push(Ref {
            path,
            kind,
            id: id.clone(),
        }),
        Some(Value::Array(items)) => {
            for (index, item) in items.iter().enumerate() {
                if let Some(id) = item.as_str() {
                    refs.push(Ref {
                        path: format!("{path}/{index}"),
                        kind,
                        id: id.to_string(),
                    });
                }
            }
        }
        _ => {}
    };

    let fields = model_ref_id_fields(kind.segment());
    if !fields.is_empty() {
        let mut doc = doc.clone();
        let paths = model_ref_node_paths(kind, &mut doc);
        let mut node = 0;
        for_each_model_ref_node(kind.segment(), &mut doc, &mut |fields_of_node| {
            let path = paths.get(node).map_or("", String::as_str);
            for f in fields {
                push(
                    format!("{path}/{}", f.field),
                    Kind::Models,
                    fields_of_node.get(f.field),
                );
            }
            node += 1;
        });
    }

    let field = |name: &str| doc.get(name);
    match kind {
        Kind::Models => push(
            "/provider_key_id".to_string(),
            Kind::ProviderKeys,
            field("provider_key_id"),
        ),
        Kind::GuardrailAttachments => {
            push(
                "/guardrail_id".to_string(),
                Kind::Guardrails,
                field("guardrail_id"),
            );
            let scoped = match field("scope_type").and_then(Value::as_str) {
                Some("model") => Some(Kind::Models),
                Some("api_key") => Some(Kind::ApiKeys),
                Some("team") => Some(Kind::Teams),
                _ => None,
            };
            if let Some(scoped) = scoped {
                push("/scope_id".to_string(), scoped, field("scope_id"));
            }
        }
        // The gateway reads `applies_to` only when `applies_to_model_id` is
        // absent (`CachePolicy::parsed_applies_to`).
        Kind::CachePolicies if field("applies_to_model_id").is_none_or(Value::is_null) => {
            if let Some(key) = field("applies_to")
                .and_then(Value::as_str)
                .and_then(|scope| scope.trim().strip_prefix("api_key:"))
            {
                push(
                    "/applies_to".to_string(),
                    Kind::ApiKeys,
                    Some(&Value::String(key.trim().to_string())),
                );
            }
        }
        Kind::RateLimitPolicies => {
            let scoped = match field("scope").and_then(Value::as_str) {
                Some("model") => Some(Kind::Models),
                Some("api_key") => Some(Kind::ApiKeys),
                Some("team" | "team_member") => Some(Kind::Teams),
                Some("member") => Some(Kind::Users),
                _ => None,
            };
            if let Some(scoped) = scoped {
                push("/scope_ref".to_string(), scoped, field("scope_ref"));
            }
        }
        Kind::ApiKeys => {
            push("/team_id".to_string(), Kind::Teams, field("team_id"));
            push("/user_id".to_string(), Kind::Users, field("user_id"));
        }
        _ => {}
    }
    refs
}

/// Refuses `doc` unless every resource it references is live, each missing
/// one a detail at its path.
///
/// An id must be written the way resource ids are (lowercase, hyphenated):
/// the gateway compares ids as text, so another spelling of a live id
/// references nothing there.
pub async fn check_outgoing(
    conn: &mut PgConnection,
    kind: Kind,
    doc: &Value,
) -> Result<(), ApiError> {
    let mut missing = Vec::new();
    for r in outgoing(kind, doc) {
        let canonical = Uuid::parse_str(&r.id)
            .ok()
            .filter(|id| id.to_string() == r.id);
        let live = match canonical {
            Some(id) => sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM gw_resources \
                 WHERE kind = $1 AND id = $2 AND NOT deleted)",
            )
            .bind(r.kind.segment())
            .bind(id)
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::from)?,
            None => false,
        };
        if !live {
            missing.push(Detail {
                path: r.path,
                message: format!("引用的 {} 资源不存在", r.kind.segment()),
            });
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(ApiError::validation(missing))
    }
}

/// The live documents, of any kind but those in `except`, that reference
/// the row `kind`/`id`. No row references itself: writes refuse that.
pub async fn incoming(
    conn: &mut PgConnection,
    kind: Kind,
    id: Uuid,
    except: &[Kind],
) -> Result<Vec<Referrer>, DbError> {
    let except: Vec<&str> = except.iter().map(|k| k.segment()).collect();
    // Any reference holds the id as text, which narrows the documents to
    // read before each one's references are worked out exactly.
    let candidates: Vec<(String, Uuid, Value)> = sqlx::query_as(
        "SELECT kind, id, doc FROM gw_resources \
         WHERE NOT deleted AND NOT (kind = ANY($1)) AND strpos(doc::text, $2) > 0 \
         ORDER BY kind, id",
    )
    .bind(&except)
    .bind(id.to_string())
    .fetch_all(&mut *conn)
    .await?;
    let target = id.to_string();
    Ok(candidates
        .into_iter()
        .filter_map(|(referrer_kind, referrer_id, doc)| {
            let referrer_kind = Kind::from_segment(&referrer_kind)?;
            outgoing(referrer_kind, &doc)
                .iter()
                .any(|r| r.kind == kind && r.id == target)
                .then(|| Referrer {
                    kind: referrer_kind,
                    id: referrer_id,
                    name: doc
                        .get("display_name")
                        .or_else(|| doc.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_models_references_are_found_where_they_sit() {
        let doc = json!({"display_name": "r", "routing": {"targets": [
            {"model_id": "m-1"}, {"model_id": "m-2", "weight": 1}
        ]}});
        let refs = outgoing(Kind::Models, &doc);
        assert_eq!(
            refs,
            [
                Ref {
                    path: "/routing/targets/0/model_id".into(),
                    kind: Kind::Models,
                    id: "m-1".into()
                },
                Ref {
                    path: "/routing/targets/1/model_id".into(),
                    kind: Kind::Models,
                    id: "m-2".into()
                },
            ]
        );
    }

    #[test]
    fn attachment_cache_and_key_references() {
        let attachment =
            json!({"guardrail_id": "g-1", "priority": 1, "scope_type": "team", "scope_id": "t-1"});
        let kinds: Vec<(Kind, String)> = outgoing(Kind::GuardrailAttachments, &attachment)
            .into_iter()
            .map(|r| (r.kind, r.id))
            .collect();
        assert_eq!(
            kinds,
            [
                (Kind::Guardrails, "g-1".into()),
                (Kind::Teams, "t-1".into())
            ]
        );

        let env_wide = json!({"guardrail_id": "g-1", "priority": 1, "scope_type": "env"});
        assert_eq!(outgoing(Kind::GuardrailAttachments, &env_wide).len(), 1);

        let cache = json!({"name": "c", "applies_to": " api_key: k-1 "});
        assert_eq!(outgoing(Kind::CachePolicies, &cache)[0].id, "k-1");
        let by_model =
            json!({"name": "c", "applies_to": "api_key:k-1", "applies_to_model_id": "m-1"});
        let refs = outgoing(Kind::CachePolicies, &by_model);
        assert_eq!(refs.len(), 1);
        assert_eq!((refs[0].kind, refs[0].id.as_str()), (Kind::Models, "m-1"));

        for (scope, target) in [
            ("model", Kind::Models),
            ("api_key", Kind::ApiKeys),
            ("team", Kind::Teams),
            ("team_member", Kind::Teams),
            ("member", Kind::Users),
        ] {
            let policy = json!({"name": "rl", "scope": scope, "scope_ref": "s-1", "window": "minute", "max_requests": 1});
            let refs: Vec<(String, Kind, String)> = outgoing(Kind::RateLimitPolicies, &policy)
                .into_iter()
                .map(|r| (r.path, r.kind, r.id))
                .collect();
            assert_eq!(
                refs,
                [("/scope_ref".into(), target, "s-1".into())],
                "{scope}"
            );
        }
        let conditional = json!({"name": "rl", "conditions": [], "limits": {"rpm": 1}});
        assert!(outgoing(Kind::RateLimitPolicies, &conditional).is_empty());

        let key = json!({"key_hash": "h", "team_id": "t-1", "user_id": null, "allowed_model_ids": ["m-1", "m-2"]});
        let refs: Vec<(String, Kind)> = outgoing(Kind::ApiKeys, &key)
            .into_iter()
            .map(|r| (r.path, r.kind))
            .collect();
        assert_eq!(
            refs,
            [
                ("/allowed_model_ids/0".into(), Kind::Models),
                ("/allowed_model_ids/1".into(), Kind::Models),
                ("/team_id".into(), Kind::Teams),
            ]
        );
    }

    #[test]
    fn a_pointer_escapes_its_segments() {
        let mut doc = json!({"semantic": {"routes": [{"target_id": "m"}]}});
        assert_eq!(
            model_ref_node_paths(Kind::Models, &mut doc),
            ["/semantic/routes/0", "/semantic"]
        );
        assert_eq!(escaped("a/b~c"), "a~1b~0c");
    }
}
