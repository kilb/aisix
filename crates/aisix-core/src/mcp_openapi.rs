//! OpenAPI-to-MCP tool generation for `type: openapi` MCP servers.
//!
//! Each `paths` operation becomes one tool: the tool name is the sanitized
//! `operationId` (fallback `<method>_<path>`), path/query parameters become
//! top-level schema properties, and a JSON request body becomes a single
//! `body` property. A parameter is identified by its name and location
//! (<https://spec.openapis.org/oas/v3.1.1.html#parameter-object>), so a name
//! that two of them share, or that a parameter shares with `body`, is
//! published as `<name>__<in>` for each of them. Local `$ref`s are resolved
//! (bounded) so referenced schemas keep their shape.
//!
//! This lives in the core crate, not beside the bridge that executes the
//! tools, so the resource loaders (file source, etcd, `aisix validate`) judge
//! a document with the very walk the runtime serves tools from.

use std::collections::HashSet;

use serde_json::{json, Map, Value};

/// Tool names must survive every major LLM provider's `^[a-zA-Z0-9_-]+$`
/// name check; 128 is the most restrictive cap.
pub const TOOL_NAME_MAX_LEN: usize = 128;

/// HTTP methods that map to tools, in generation order.
const METHODS: [&str; 5] = ["get", "post", "put", "delete", "patch"];

/// `$ref` inlining bounds, per operation: a cyclic or pathologically nested
/// schema degrades to `{}` (schema "anything") instead of recursing forever
/// or exploding the inlined size.
const MAX_REF_DEPTH: usize = 16;
const MAX_REF_EXPANSIONS: usize = 256;

/// One OpenAPI operation, resolved into a callable tool.
#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Lowercase HTTP method.
    pub method: String,
    /// The path template as written in the spec, e.g. `/items/{id}`.
    pub path: String,
    pub path_params: Vec<ToolParam>,
    pub query_params: Vec<ToolParam>,
    /// The request body's media type, the `requestBody.content` key it was
    /// generated from; `None` when the operation takes no body.
    pub body_media_type: Option<String>,
}

/// A path or query parameter of a [`GeneratedTool`].
#[derive(Debug, Clone, PartialEq)]
pub struct ToolParam {
    /// The parameter's name in the API, as the request carries it.
    pub name: String,
    /// The tool argument that carries its value: `name`, or `<name>__<in>`
    /// when another parameter of the operation, or its body, shares the name.
    pub argument: String,
}

impl ToolParam {
    /// A parameter whose argument is its own name.
    pub fn named(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            argument: name.clone(),
            name,
        }
    }
}

/// Map an `operationId` (or fallback) to a provider-safe tool name:
/// lowercase, any character outside `[a-zA-Z0-9_-]` replaced with `_`,
/// capped at [`TOOL_NAME_MAX_LEN`].
pub fn sanitize_tool_name(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .take(TOOL_NAME_MAX_LEN)
        .collect()
}

/// The load-time verdict on a `type: openapi` server's document: an error
/// when the gateway would serve no tools from it — no usable `paths`, or no
/// operation it can turn into a tool. Collisions are not an error: they
/// load, and every colliding operation stays callable under its suffix.
pub fn check_spec_yields_tools(spec: &Value) -> Result<(), String> {
    let generation = generate(spec)?;
    if !generation.tools.is_empty() {
        return Ok(());
    }
    let mut message =
        "openapi spec has no operations that can become tools (methods get/post/put/delete/patch)"
            .to_string();
    if !generation.skipped.is_empty() {
        message.push_str(&format!(
            "; skipped operations without an application/json request body: {}",
            generation.skipped.join(", ")
        ));
    }
    Err(message)
}

/// Outcome of walking a spec: the tools plus the anomalies a load or
/// `aisix validate` reports.
pub struct Generation {
    pub tools: Vec<GeneratedTool>,
    /// Base names that collided after sanitization (each listed once).
    /// Every colliding operation is still generated, under a `_2` / `_3` …
    /// suffix.
    pub duplicates: Vec<String>,
    /// `<METHOD> <path>` of operations skipped for an unsupported body.
    pub skipped: Vec<String>,
}

/// Generate the tool set from an OpenAPI 3.x document.
///
/// Anomalies inside a single operation (a request body without an
/// `application/json` variant, an unresolvable parameter ref) skip or degrade
/// that operation only; the error path is reserved for a document that has no
/// usable `paths` object at all.
pub fn generate(spec: &Value) -> Result<Generation, String> {
    let paths = spec
        .get("paths")
        .and_then(Value::as_object)
        .ok_or_else(|| "openapi spec has no `paths` object".to_string())?;

    let components = spec.get("components").cloned().unwrap_or(Value::Null);
    let mut used_names: HashSet<String> = HashSet::new();
    let mut tools = Vec::new();
    let mut duplicates: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for (path, path_item) in paths {
        let Some(path_item) = path_item.as_object() else {
            continue;
        };
        for method in METHODS {
            let Some(operation) = path_item.get(method).and_then(Value::as_object) else {
                continue;
            };

            let mut resolver = RefResolver::new(spec);
            let params = merged_parameters(path_item, operation, &components, &mut resolver);

            // A request body we cannot express (no JSON variant) skips the
            // operation: a tool missing its body argument would mislead the
            // agent into calls that cannot succeed.
            let request_body = operation
                .get("requestBody")
                .map(|rb| resolver.resolve(rb, 0));
            let body = match &request_body {
                Some(rb) => match json_body_schema(rb, &mut resolver) {
                    BodyOutcome::Schema { media_type, schema } => Some((media_type, schema)),
                    BodyOutcome::None => None,
                    BodyOutcome::Unsupported => {
                        skipped.push(format!("{} {}", method.to_uppercase(), path));
                        continue;
                    }
                },
                None => None,
            };

            let raw_name = operation
                .get("operationId")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("{method}_{path}"));
            let base_name = sanitize_tool_name(&raw_name);

            // Disambiguate names that collide after sanitization so every
            // tool stays reachable (`foo/list` and `foo.list` both map to
            // `foo_list`).
            let mut name = base_name.clone();
            let mut n = 1;
            while !used_names.insert(name.clone()) {
                if n == 1 && !duplicates.contains(&base_name) {
                    duplicates.push(base_name.clone());
                }
                n += 1;
                let suffix = format!("_{n}");
                let keep = TOOL_NAME_MAX_LEN - suffix.len();
                name = base_name.chars().take(keep).collect::<String>() + &suffix;
            }

            let description = operation
                .get("summary")
                .or_else(|| operation.get("description"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("{} {}", method.to_uppercase(), path));

            let mut properties = Map::new();
            let mut required = Vec::new();
            let mut path_params = Vec::new();
            let mut query_params = Vec::new();

            // header/cookie parameters are not exposed to the agent: upstream
            // headers are the gateway's to set, not the caller's.
            let exposed: Vec<(&str, &str, &Value)> = params
                .iter()
                .filter_map(|param| {
                    let name = param.get("name").and_then(Value::as_str)?;
                    let location = param.get("in").and_then(Value::as_str)?;
                    matches!(location, "path" | "query").then_some((name, location, param))
                })
                .collect();
            let shared = |name: &str| {
                (body.is_some() && name == "body")
                    || exposed.iter().filter(|(n, _, _)| *n == name).count() > 1
            };
            let mut arguments: HashSet<String> = exposed
                .iter()
                .filter(|(name, _, _)| !shared(name))
                .map(|(name, _, _)| name.to_string())
                .collect();
            if body.is_some() {
                arguments.insert("body".to_string());
            }

            for (param_name, location, param) in exposed.iter().copied() {
                let argument = if shared(param_name) {
                    let base = format!("{param_name}__{location}");
                    let mut argument = base.clone();
                    let mut n = 1;
                    while !arguments.insert(argument.clone()) {
                        n += 1;
                        argument = format!("{base}_{n}");
                    }
                    argument
                } else {
                    param_name.to_string()
                };
                let tool_param = ToolParam {
                    name: param_name.to_string(),
                    argument: argument.clone(),
                };
                if location == "path" {
                    path_params.push(tool_param);
                } else {
                    query_params.push(tool_param);
                }

                let mut schema = match param.get("schema") {
                    Some(s) => resolver.resolve(s, 0),
                    None => Value::Null,
                };
                if !schema.is_object() || schema.as_object().is_some_and(Map::is_empty) {
                    schema = json!({ "type": "string" });
                }
                if let Some(desc) = param.get("description").and_then(Value::as_str) {
                    schema
                        .as_object_mut()
                        .expect("schema coerced to object above")
                        .insert("description".to_string(), json!(desc));
                }
                properties.insert(argument.clone(), schema);

                if param.get("required").and_then(Value::as_bool) == Some(true) {
                    required.push(argument);
                }
            }

            let body_media_type = if let Some((media_type, mut schema)) = body {
                // A non-object or degraded-to-`{}` schema still gets an
                // object hint, mirroring the string default on parameters.
                if !schema.is_object() || schema.as_object().is_some_and(Map::is_empty) {
                    schema = json!({ "type": "object" });
                }
                if let Some(desc) = request_body
                    .as_ref()
                    .and_then(|rb| rb.get("description"))
                    .and_then(Value::as_str)
                {
                    schema
                        .as_object_mut()
                        .expect("schema coerced to object above")
                        .insert("description".to_string(), json!(desc));
                }
                properties.insert("body".to_string(), schema);
                if request_body
                    .as_ref()
                    .and_then(|rb| rb.get("required"))
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    required.push("body".to_string());
                }
                Some(media_type)
            } else {
                None
            };

            tools.push(GeneratedTool {
                name,
                description,
                input_schema: json!({
                    "type": "object",
                    "properties": properties,
                    "required": required,
                }),
                method: method.to_string(),
                path: path.clone(),
                path_params,
                query_params,
                body_media_type,
            });
        }
    }

    Ok(Generation {
        tools,
        duplicates,
        skipped,
    })
}

/// How an operation's `requestBody` maps onto the tool schema.
enum BodyOutcome {
    /// A JSON variant found: its media type, as the `content` key names it,
    /// and its (resolved) schema.
    Schema { media_type: String, schema: Value },
    /// The body object carries no `content` at all — treat as body-less.
    None,
    /// A body exists but has no JSON variant (multipart upload, form data…).
    Unsupported,
}

fn json_body_schema(request_body: &Value, resolver: &mut RefResolver<'_>) -> BodyOutcome {
    let Some(content) = request_body.get("content").and_then(Value::as_object) else {
        return BodyOutcome::None;
    };
    if content.is_empty() {
        return BodyOutcome::None;
    }
    // Accept `application/json` plus parameterized variants like
    // `application/json; charset=utf-8` and structured-syntax `+json` types
    // like `application/problem+json`. The body is sent under the media type
    // it was generated from, so an API that offers several is sent the plain
    // `application/json` one first, and a vendor type only when it offers
    // nothing else.
    let rank = |key: &str| {
        let (mime, params) = key.split_once(';').unwrap_or((key, ""));
        let mime = mime.trim();
        if mime.eq_ignore_ascii_case("application/json") {
            Some(if params.trim().is_empty() { 0 } else { 1 })
        } else if mime.starts_with("application/") && mime.ends_with("+json") && !mime.contains('*')
        {
            Some(2)
        } else {
            None
        }
    };
    let json_variant = content
        .iter()
        .filter_map(|(key, media)| Some((rank(key)?, key, media)))
        .min_by_key(|(rank, _, _)| *rank);
    match json_variant {
        Some((_, key, media)) => {
            let schema = media
                .get("schema")
                .map(|s| resolver.resolve(s, 0))
                .unwrap_or_else(|| json!({ "type": "object" }));
            BodyOutcome::Schema {
                media_type: key.trim().to_string(),
                schema,
            }
        }
        None => BodyOutcome::Unsupported,
    }
}

/// Merge path-level and operation-level parameters (operation wins on the
/// same `(name, in)` pair), resolving `#/components/parameters/*` refs and
/// dropping unresolvable entries.
fn merged_parameters(
    path_item: &Map<String, Value>,
    operation: &Map<String, Value>,
    components: &Value,
    resolver: &mut RefResolver<'_>,
) -> Vec<Value> {
    let resolve_list = |raw: Option<&Value>, resolver: &mut RefResolver<'_>| -> Vec<Value> {
        raw.and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|p| {
                        let resolved = resolve_parameter(p, components, resolver)?;
                        resolved.get("name")?.as_str()?;
                        Some(resolved)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    let path_level = resolve_list(path_item.get("parameters"), resolver);
    let op_level = resolve_list(operation.get("parameters"), resolver);

    let op_keys: HashSet<(String, String)> = op_level.iter().map(param_key).collect();
    let mut merged: Vec<Value> = path_level
        .into_iter()
        .filter(|p| !op_keys.contains(&param_key(p)))
        .collect();
    merged.extend(op_level);
    merged
}

fn param_key(param: &Value) -> (String, String) {
    (
        param
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        param
            .get("in")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    )
}

/// Resolve one parameter entry, following a `#/components/parameters/<name>`
/// ref if present. Returns `None` for unresolvable refs so callers drop the
/// entry instead of keeping a nameless stub.
fn resolve_parameter(
    param: &Value,
    components: &Value,
    resolver: &mut RefResolver<'_>,
) -> Option<Value> {
    let Some(reference) = param.get("$ref").and_then(Value::as_str) else {
        return Some(param.clone());
    };
    let target_name = reference.strip_prefix("#/components/parameters/")?;
    let target = components.get("parameters")?.get(target_name)?;
    Some(resolver.resolve(target, 0))
}

/// Bounded local-`$ref` inliner.
///
/// Replaces `{"$ref": "#/..."}` nodes with their (recursively resolved)
/// targets; sibling keys next to `$ref` overlay the resolved target (the
/// OpenAPI 3.1 `summary`/`description` pattern). External refs, missing
/// targets, cycles past [`MAX_REF_DEPTH`], and documents spending more than
/// [`MAX_REF_EXPANSIONS`] lookups degrade the node to `{}` — schema
/// "anything" — rather than failing the operation.
struct RefResolver<'a> {
    root: &'a Value,
    expansions: usize,
}

impl<'a> RefResolver<'a> {
    fn new(root: &'a Value) -> Self {
        Self {
            root,
            expansions: 0,
        }
    }

    fn resolve(&mut self, node: &Value, depth: usize) -> Value {
        match node {
            Value::Object(map) => {
                if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
                    let resolved = self.resolve_ref(reference, depth);
                    // Sibling keys overlay the resolved target.
                    if map.len() > 1 {
                        let mut base = match resolved {
                            Value::Object(m) => m,
                            _ => Map::new(),
                        };
                        for (k, v) in map {
                            if k != "$ref" {
                                base.insert(k.clone(), self.resolve(v, depth));
                            }
                        }
                        return Value::Object(base);
                    }
                    return resolved;
                }
                Value::Object(
                    map.iter()
                        .map(|(k, v)| (k.clone(), self.resolve(v, depth)))
                        .collect(),
                )
            }
            Value::Array(items) => {
                Value::Array(items.iter().map(|v| self.resolve(v, depth)).collect())
            }
            other => other.clone(),
        }
    }

    fn resolve_ref(&mut self, reference: &str, depth: usize) -> Value {
        if depth >= MAX_REF_DEPTH || self.expansions >= MAX_REF_EXPANSIONS {
            return json!({});
        }
        let Some(pointer) = reference.strip_prefix('#') else {
            // External refs are not fetched at runtime by design.
            return json!({});
        };
        self.expansions += 1;
        match self.root.pointer(pointer) {
            Some(target) => self.resolve(target, depth + 1),
            None => json!({}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_operation_ids_into_provider_safe_names() {
        // GitHub-style tag-namespaced ids gain `_` for `/`; uppercase folds.
        assert_eq!(
            sanitize_tool_name("actions/Download-Job.Logs"),
            "actions_download-job_logs"
        );
        let long = "x".repeat(200);
        assert_eq!(sanitize_tool_name(&long).len(), TOOL_NAME_MAX_LEN);
    }

    #[test]
    fn a_spec_is_refused_only_when_it_yields_no_tool() {
        // No paths at all.
        assert!(check_spec_yields_tools(&json!({ "openapi": "3.0.0" })).is_err());

        // Paths but nothing generatable — the skipped multipart op is named.
        let only_multipart = json!({
            "paths": { "/upload": { "post": {
                "operationId": "up",
                "requestBody": { "content": { "multipart/form-data": {} } }
            } } }
        });
        let err = check_spec_yields_tools(&only_multipart).unwrap_err();
        assert!(err.contains("no operations"), "{err}");
        assert!(err.contains("POST /upload"), "{err}");

        // A collision is reported, not refused: both operations are served.
        let dup = json!({
            "paths": {
                "/a": { "get": { "operationId": "foo/list" } },
                "/b": { "get": { "operationId": "foo.list" } }
            }
        });
        check_spec_yields_tools(&dup).unwrap();
        let generation = generate(&dup).unwrap();
        assert_eq!(generation.duplicates, vec!["foo_list"]);
        let names: Vec<_> = generation.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["foo_list", "foo_list_2"]);

        check_spec_yields_tools(&json!({
            "paths": { "/items": { "get": { "operationId": "listItems" } } }
        }))
        .unwrap();
    }

    /// A parameter is its name AND its location: two that share a name, or
    /// one named like the reserved `body` argument of an operation with a
    /// body, each get an argument of their own, and the schema publishes
    /// exactly the arguments the call reads.
    #[test]
    fn parameters_sharing_a_name_get_an_argument_each() {
        let spec = json!({
            "paths": {
                "/things/{id}/{body}": { "post": {
                    "operationId": "mixed",
                    "parameters": [
                        { "name": "id", "in": "path", "required": true,
                          "schema": { "type": "integer" } },
                        { "name": "id", "in": "query", "schema": { "type": "string" } },
                        { "name": "body", "in": "path", "required": true,
                          "schema": { "type": "string" } },
                        { "name": "limit", "in": "query", "schema": { "type": "integer" } }
                    ],
                    "requestBody": { "required": true, "content": { "application/json": {
                        "schema": { "type": "object" } } } }
                } },
                "/plain/{body}": { "get": {
                    "operationId": "plain",
                    "parameters": [ { "name": "body", "in": "path", "required": true,
                                      "schema": { "type": "string" } } ]
                } }
            }
        });
        let tools = generate(&spec).unwrap().tools;
        let mixed = tools.iter().find(|t| t.name == "mixed").unwrap();
        let arg = |name: &str, argument: &str| ToolParam {
            name: name.into(),
            argument: argument.into(),
        };
        assert_eq!(
            mixed.path_params,
            [arg("id", "id__path"), arg("body", "body__path")]
        );
        assert_eq!(
            mixed.query_params,
            [arg("id", "id__query"), arg("limit", "limit")]
        );
        let properties = mixed.input_schema["properties"].as_object().unwrap();
        let mut published: Vec<&str> = properties.keys().map(String::as_str).collect();
        published.sort_unstable();
        assert_eq!(
            published,
            ["body", "body__path", "id__path", "id__query", "limit"]
        );
        assert_eq!(properties["body"]["type"], "object");
        assert_eq!(properties["body__path"]["type"], "string");
        assert_eq!(properties["id__path"]["type"], "integer");
        let mut required: Vec<&str> = mixed.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        required.sort_unstable();
        assert_eq!(required, ["body", "body__path", "id__path"]);

        // Without a request body, `body` is an ordinary name.
        let plain = tools.iter().find(|t| t.name == "plain").unwrap();
        assert_eq!(plain.path_params, [ToolParam::named("body")]);
    }

    /// The media type a body is generated from is kept, so it is the one
    /// sent: plain JSON when the operation offers it, a vendor JSON type only
    /// when it offers nothing else.
    #[test]
    fn a_body_keeps_the_media_type_it_was_generated_from() {
        let body = |content: Value| {
            let spec = json!({ "paths": { "/x": { "post": {
                "operationId": "x", "requestBody": { "content": content }
            } } } });
            generate(&spec).unwrap().tools[0].body_media_type.clone()
        };
        let schema = json!({ "schema": { "type": "object" } });
        assert_eq!(
            body(json!({ "application/vnd.api+json": schema })).as_deref(),
            Some("application/vnd.api+json")
        );
        assert_eq!(
            body(json!({ "application/vnd.api+json": schema, "application/json": schema }))
                .as_deref(),
            Some("application/json")
        );
        assert_eq!(
            body(json!({ "application/json; charset=utf-8": schema, "application/merge-patch+json": schema }))
                .as_deref(),
            Some("application/json; charset=utf-8")
        );
        let spec = json!({ "paths": { "/x": { "get": { "operationId": "x" } } } });
        assert_eq!(generate(&spec).unwrap().tools[0].body_media_type, None);
    }
}
