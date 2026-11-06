//! Normalisation, run on every created or replaced document before it is
//! validated: one spelling per field, and model references by id.
//!
//! - A field the gateway's strict schema still accepts under a former name
//!   is rewritten to its canonical name. The list is read from the schema
//!   itself ([`schema_aliases`]), never written here, so a rename the
//!   gateway adds is normalised (and its credential sealed and redacted
//!   under one name) without a change here.
//! - Every model reference written by display name is resolved against the
//!   live models and rewritten to the id form the gateway also reads, so a
//!   later rename of that model keeps the reference pointing at it. Which
//!   references a kind carries, and where, comes from
//!   `aisix_core::filesource`, the same list the resources file and
//!   `aisix export` use.

use std::collections::HashMap;
use std::sync::LazyLock;

use aisix_core::filesource::{for_each_model_ref_node, model_ref_id_fields};
use aisix_core::models::schema::resource_root_schema;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use super::refs::{escaped, model_ref_node_paths};
use super::Kind;
use crate::db::DbError;
use crate::error::{ApiError, Detail};

/// Every `(canonical, former)` field-name pair the strict schema of `kind`
/// accepts, read once per process.
///
/// A renamed field shows in a generated schema as a root `anyOf` of two
/// branches, each titled with one spelling and requiring exactly it, the
/// canonical one first (`accept_renamed_field` in
/// `aisix_core::models::schema`). Today that is `provider_key`'s `api_key`,
/// formerly `secret`.
pub fn schema_aliases(kind: Kind) -> &'static [(String, String)] {
    static ALIASES: LazyLock<HashMap<Kind, Vec<(String, String)>>> = LazyLock::new(|| {
        Kind::ALL
            .into_iter()
            .map(|kind| {
                let schema = resource_root_schema(kind.schema_name(), true);
                (kind, aliases_in(&schema))
            })
            .collect()
    });
    ALIASES.get(&kind).map_or(&[], Vec::as_slice)
}

/// The renamed-field pairs a root schema declares: in its own `anyOf`, or in
/// an `anyOf` of one of its `allOf` members, which is where a rename goes
/// when the root `anyOf` is already in use.
fn aliases_in(schema: &Value) -> Vec<(String, String)> {
    let mut groups: Vec<&Vec<Value>> = schema
        .get("anyOf")
        .and_then(Value::as_array)
        .into_iter()
        .collect();
    if let Some(members) = schema.get("allOf").and_then(Value::as_array) {
        groups.extend(
            members
                .iter()
                .filter_map(|member| member.get("anyOf").and_then(Value::as_array)),
        );
    }
    groups
        .into_iter()
        .filter_map(|group| rename_pair(group))
        .collect()
}

/// `(canonical, former)`, when `branches` are exactly two, each titled with
/// one spelling and requiring exactly it, the canonical one first.
fn rename_pair(branches: &[Value]) -> Option<(String, String)> {
    let spellings: Vec<&str> = branches
        .iter()
        .filter_map(|branch| {
            let title = branch.get("title")?.as_str()?;
            let required = branch.get("required")?.as_array()?;
            (required.len() == 1 && required[0].as_str() == Some(title)).then_some(title)
        })
        .collect();
    match spellings.as_slice() {
        [canonical, former] if branches.len() == 2 => {
            Some((canonical.to_string(), former.to_string()))
        }
        _ => None,
    }
}

/// Rewrites every former field name in `doc` to its canonical one. A
/// document that carries both spellings of a field is refused at the former
/// one: which of the two values was meant cannot be told.
pub fn rename_aliases(kind: Kind, doc: &mut Value) -> Result<(), Vec<Detail>> {
    let Some(fields) = doc.as_object_mut() else {
        return Ok(());
    };
    let mut problems = Vec::new();
    for (canonical, former) in schema_aliases(kind) {
        if !fields.contains_key(former) {
            continue;
        }
        if fields.contains_key(canonical) {
            problems.push(Detail {
                path: format!("/{former}"),
                message: format!("`{former}` 是 `{canonical}` 的旧写法，两者只能填写一个"),
            });
            continue;
        }
        if let Some(value) = fields.remove(former) {
            fields.insert(canonical.clone(), value);
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Normalises `doc`, a document of `kind` about to be created or replaced:
/// a U+0000 anywhere is refused, then [`rename_aliases`], then every
/// name-form model reference rewritten to its id form. A name that matches
/// no live model is a `422` at its path.
pub async fn normalize(
    conn: &mut PgConnection,
    kind: Kind,
    doc: &mut Value,
) -> Result<(), ApiError> {
    let nul: Vec<Detail> = nul_paths(doc)
        .into_iter()
        .map(|path| Detail {
            path,
            message: "不能包含 U+0000 字符".to_string(),
        })
        .collect();
    if !nul.is_empty() {
        return Err(ApiError::validation(nul));
    }
    rename_aliases(kind, doc).map_err(ApiError::validation)?;
    resolve_model_names(conn, kind, doc).await
}

/// The JSON pointer of every key and string in `doc` that holds a U+0000,
/// which Postgres stores in neither `jsonb` nor `text`.
fn nul_paths(doc: &Value) -> Vec<String> {
    fn walk(value: &Value, path: &mut String, out: &mut Vec<String>) {
        let children: Vec<(String, bool, &Value)> = match value {
            Value::String(s) => {
                if s.contains('\0') {
                    out.push(path.clone());
                }
                return;
            }
            Value::Object(fields) => fields
                .iter()
                .map(|(k, v)| (escaped(k), k.contains('\0'), v))
                .collect(),
            Value::Array(items) => items
                .iter()
                .enumerate()
                .map(|(i, v)| (i.to_string(), false, v))
                .collect(),
            _ => return,
        };
        for (segment, nul_key, child) in children {
            let len = path.len();
            path.push('/');
            path.push_str(&segment);
            if nul_key {
                out.push(path.clone());
            } else {
                walk(child, path, out);
            }
            path.truncate(len);
        }
    }
    let mut out = Vec::new();
    walk(doc, &mut String::new(), &mut out);
    out
}

/// A model reference written by name.
struct NameRef {
    /// Which of the nodes `for_each_model_ref_node` visits carries it.
    node: usize,
    id_field: &'static str,
    name_field: &'static str,
    name: String,
    /// The JSON pointer of the name field.
    path: String,
}

async fn resolve_model_names(
    conn: &mut PgConnection,
    kind: Kind,
    doc: &mut Value,
) -> Result<(), ApiError> {
    let fields = model_ref_id_fields(kind.segment());
    if fields.is_empty() {
        return Ok(());
    }
    let paths = model_ref_node_paths(kind, doc);
    let mut refs = Vec::new();
    let mut node = 0;
    for_each_model_ref_node(kind.segment(), doc, &mut |fields_of_node| {
        let path = paths.get(node).map_or("", String::as_str);
        for f in fields {
            // The id decides when present; the name beside it is not read.
            if fields_of_node.get(f.field).is_some_and(|id| !id.is_null()) {
                continue;
            }
            let Some(written) = fields_of_node.get(f.name_field).and_then(Value::as_str) else {
                continue;
            };
            let name = if kind == Kind::CachePolicies && f.name_field == "applies_to" {
                // `"model:<name>"`, read the way `CachePolicy::parsed_applies_to`
                // reads it; `"all"` and `"api_key:<id>"` name no model.
                match written.trim().strip_prefix("model:") {
                    Some(rest) => rest.trim(),
                    None => continue,
                }
            } else {
                written
            };
            refs.push(NameRef {
                node,
                id_field: f.field,
                name_field: f.name_field,
                name: name.to_string(),
                path: format!("{path}/{}", f.name_field),
            });
        }
        node += 1;
    });
    if refs.is_empty() {
        return Ok(());
    }

    let mut names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    let ids: HashMap<String, Uuid> = sqlx::query_as::<_, (String, Uuid)>(
        "SELECT identity, id FROM gw_resources \
         WHERE kind = $1 AND NOT deleted AND identity = ANY($2)",
    )
    .bind(Kind::Models.segment())
    .bind(&names)
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?
    .into_iter()
    .collect();
    let missing: Vec<Detail> = refs
        .iter()
        .filter(|r| !ids.contains_key(&r.name))
        .map(|r| Detail {
            path: r.path.clone(),
            message: format!("没有名为「{}」的模型", r.name),
        })
        .collect();
    if !missing.is_empty() {
        return Err(ApiError::validation(missing));
    }

    let mut node = 0;
    for_each_model_ref_node(kind.segment(), doc, &mut |fields_of_node| {
        for r in refs.iter().filter(|r| r.node == node) {
            if let Some(id) = ids.get(&r.name) {
                fields_of_node.insert(r.id_field.to_string(), Value::String(id.to_string()));
                fields_of_node.remove(r.name_field);
            }
        }
        node += 1;
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_provider_key_rename_is_read_from_its_schema() {
        assert_eq!(
            schema_aliases(Kind::ProviderKeys),
            [("api_key".to_string(), "secret".to_string())]
        );
        assert!(schema_aliases(Kind::Models).is_empty());
    }

    #[test]
    fn a_rename_composed_into_all_of_is_read_too() {
        let schema = json!({"allOf": [
            {"if": {"required": ["x"]}, "then": {"required": ["y"]}},
            {"anyOf": [
                {"title": "name", "required": ["name"]},
                {"title": "display_name", "required": ["display_name"]},
            ]},
        ]});
        assert_eq!(
            aliases_in(&schema),
            [("name".to_string(), "display_name".to_string())]
        );
    }

    /// Every branch shaped like a rename, anywhere in a published resource
    /// schema, is one `aliases_in` reads: a rename it cannot see would be a
    /// former spelling stored, and for a credential, stored unsealed.
    #[test]
    fn every_rename_a_resource_schema_declares_is_read() {
        fn renamed_spellings(node: &Value, out: &mut Vec<String>) {
            match node {
                Value::Object(fields) => {
                    let title = fields.get("title").and_then(Value::as_str);
                    let required = fields.get("required").and_then(Value::as_array);
                    if let (Some(title), Some([only])) = (title, required.map(Vec::as_slice)) {
                        if only.as_str() == Some(title) {
                            out.push(title.to_string());
                        }
                    }
                    fields
                        .values()
                        .for_each(|child| renamed_spellings(child, out));
                }
                Value::Array(items) => items.iter().for_each(|child| renamed_spellings(child, out)),
                _ => {}
            }
        }
        let mut seen = 0;
        for resource in aisix_core::models::schema::RESOURCES {
            let schema = resource_root_schema(resource, true);
            let mut declared = Vec::new();
            renamed_spellings(&schema, &mut declared);
            declared.sort();
            let mut read: Vec<String> = aliases_in(&schema)
                .into_iter()
                .flat_map(|(canonical, former)| [canonical, former])
                .collect();
            read.sort();
            assert_eq!(read, declared, "{resource}");
            seen += declared.len();
        }
        assert!(seen >= 2, "no rename found in any schema");
    }

    #[test]
    fn a_former_name_is_renamed_and_both_names_are_refused() {
        let mut doc = json!({"display_name": "pk", "secret": "s"});
        rename_aliases(Kind::ProviderKeys, &mut doc).expect("one spelling");
        assert_eq!(doc, json!({"display_name": "pk", "api_key": "s"}));

        let mut doc = json!({"api_key": "a", "secret": "s"});
        let problems = rename_aliases(Kind::ProviderKeys, &mut doc).expect_err("both spellings");
        assert_eq!(problems[0].path, "/secret");
        assert_eq!(doc, json!({"api_key": "a", "secret": "s"}));
    }
}
