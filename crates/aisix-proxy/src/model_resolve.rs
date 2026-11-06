//! Wildcard-aware model resolution (`provider/*` aliases).
//!
//! Client-facing handlers resolve `req.model` through [`resolve_model`] instead
//! of calling `snapshot.models.get_by_name` directly, so a request for
//! `openai/gpt-4o` can be served by an operator-defined `openai/*` wildcard
//! Model. Exact names always win; among wildcards the most specific (longest
//! literal) pattern wins.
//!
//! A wildcard match returns a synthetic clone of the Model with its
//! `model_name` resolved to the concrete upstream id (the captured segment
//! substituted into the template's `*`, or the template kept as-is when it has
//! no `*`). Everything else — id, provider, provider_key_id, rate_limit, cost,
//! guardrail scope — is inherited from the wildcard Model, so downstream
//! dispatch/telemetry treat it like a normal Model and attribution stays on the
//! wildcard Model's id. The client still sees the name it requested, because
//! every handler echoes `req.model` rather than the resolved `display_name`.

use std::sync::Arc;

use aisix_core::wildcard::wildcard_capture;
use aisix_core::{AisixSnapshot, Model, ResourceEntry};

use crate::error::ProxyError;

/// Resolve `requested` against the snapshot's Model table, honoring `provider/*`
/// wildcard Models when no exact match exists. Returns `None` if nothing matches.
pub(crate) fn resolve_model(
    snapshot: &AisixSnapshot,
    requested: &str,
) -> Option<Arc<ResourceEntry<Model>>> {
    // Every client-facing handler resolves the caller's `model` here, which
    // makes this the one place that knows what a request ASKED for — the
    // terminal emitters that run after the handler is gone read it back off
    // the request's attribution cell (see `attribution`).
    if let Some(exact) = snapshot.models.get_by_name(requested) {
        crate::attribution::note_requested_model(requested, &exact.id);
        note_dispatchable_entry(&exact);
        return Some(exact);
    }
    let (entry, upstream) = best_wildcard_row(snapshot, requested)?;
    crate::attribution::note_requested_model(requested, &entry.id);
    // A wildcard row is a direct model, and attribution stays on the ROW
    // (see the module docs), so the synthetic clone below inherits its id.
    note_dispatchable_entry(&entry);
    // The wildcard row is shared; this materialises the one concrete copy
    // that carries the resolved `model_name`.
    let mut model = Arc::clone(&entry.value);
    Arc::make_mut(&mut model).model_name = Some(upstream);
    Some(Arc::new(ResourceEntry::from_arc(
        entry.id.clone(),
        model,
        entry.revision,
    )))
}

/// Refuse an ensemble to an API key restricted by `allowed_provider_groups`,
/// with the error a name nothing serves gets.
///
/// An ensemble dispatches its panel and judge members itself, past the
/// routing path that applies the key's provider groups, and ensemble is an
/// experimental surface whose member-level gates wait for one design pass.
/// Serving it would reach upstreams outside the key's groups, so for such a
/// key it is answered as a model that does not exist rather than with a
/// refusal of its own. Keys on the REQUESTED entry, the only identity an
/// ensemble has; every other kind passes, and its targets meet the group
/// filter in `routing::resolve_attempt_models` or `semantic::select_eligible`.
///
/// Called from `routing::resolve_attempt_models`, which `/v1/chat/completions`
/// runs before it branches to the ensemble dispatch; the other endpoints
/// cannot dispatch an ensemble and pass through it too.
pub(crate) fn refuse_ensemble_to_grouped_key(
    requested: &str,
    model: &Model,
    allowed_groups: &[String],
) -> Result<(), ProxyError> {
    if model.is_ensemble() && !allowed_groups.is_empty() {
        return Err(ProxyError::ModelNotFound(requested.to_string()));
    }
    Ok(())
}

/// The `model_id` an event that selected no target reports for `entry`:
/// its own id when it dispatches to an upstream itself (a direct or
/// embedding model, or the wildcard row an alias resolved to), empty for a
/// routing group, an ensemble or a semantic router. Those are addressed by
/// the caller and served by something else; their uuid prices nothing, so
/// `model_id` waits for the target an attempt selects.
pub(crate) fn dispatchable_entry_id(entry: &ResourceEntry<Model>) -> &str {
    let model = &entry.value;
    if model.is_routing() || model.is_ensemble() || model.is_semantic() {
        return "";
    }
    &entry.id
}

/// Record [`dispatchable_entry_id`] for the emitters that read the
/// request's attribution cell rather than the entry — the terminal ones
/// that run after the handler is gone (AISIX-Cloud#1571) and
/// [`crate::attribution::unattempted_model_id`].
fn note_dispatchable_entry(entry: &ResourceEntry<Model>) {
    let id = dispatchable_entry_id(entry);
    if !id.is_empty() {
        crate::attribution::note_resolved_entry(id);
    }
}

/// Wildcard fallback: the most specific direct Model whose `*`-glob
/// display name matches the request, plus the substituted upstream model
/// id. Only meaningful when the exact lookup missed.
fn best_wildcard_row(
    snapshot: &AisixSnapshot,
    requested: &str,
) -> Option<(Arc<ResourceEntry<Model>>, String)> {
    let mut best: Option<(usize, Arc<ResourceEntry<Model>>, String)> = None;
    // Only the wildcard rows, not the whole catalog. This runs on the
    // model-resolution path AND on the metric-label path that every endpoint
    // reaches — including for a name that resolves to nothing, which reaches
    // the sentinel only after the search completes.
    for entry in snapshot.models.wildcard_entries() {
        let model = &entry.value;
        // Only direct Models can serve a wildcard alias — routers / ensembles /
        // semantic routers have no upstream `model_name` to dispatch.
        if model.is_routing() || model.is_ensemble() || model.is_semantic() {
            continue;
        }
        let Some(capture) = wildcard_capture(&model.display_name, requested) else {
            continue;
        };
        // Specificity = literal (non-`*`) length; longest wins. A tie goes to
        // the lowest entry id: the table iterates in no fixed order, and a
        // tie decided by iteration order could pick a different row on
        // another replica or after a restart, which also breaks a video id
        // bound to the row its submit resolved.
        let specificity = model.display_name.len() - 1;
        if best
            .as_ref()
            .is_none_or(|(s, e, _)| specificity > *s || (specificity == *s && entry.id < e.id))
        {
            let upstream = resolve_upstream_model_name(model, &capture);
            best = Some((specificity, entry.clone(), upstream));
        }
    }
    best.map(|(_, entry, upstream)| (entry, upstream))
}

/// Whether `model` is a row that would actually serve the caller-facing
/// name `requested` — an exact `display_name`, or a wildcard glob covering
/// it. Used where a name arrives from somewhere other than a live request
/// body (a client-supplied video id) and must not be echoed back as though
/// the gateway had attested it.
pub(crate) fn row_serves_name(model: &Model, requested: &str) -> bool {
    // The same kind gate `best_wildcard_row` applies: only a direct row can
    // serve a caller-minted alias. Unreachable today on the one surface that
    // calls this — `dispatch::require_provider` rejects those kinds first —
    // but this sits beside the function it mirrors, and it judges an entry
    // the CLIENT named, so the two answer alike rather than by coincidence.
    if model.is_routing() || model.is_ensemble() || model.is_semantic() {
        return false;
    }
    // Exact equality FIRST, and for wildcard rows too. `resolve_model` starts
    // with `get_by_name(requested)`, so a caller can address `wan/*`
    // literally and be served by that row — which makes the pattern a name
    // the row serves, however odd it looks. Narrowing this to the glob would
    // make the two functions disagree about the same request.
    model.display_name == requested
        || (model.display_name.contains('*')
            && wildcard_capture(&model.display_name, requested).is_some())
}

/// The `display_name` of the wildcard row that would serve `requested`,
/// for metric-label bounding: successful wildcard traffic must label as
/// the configured row (`openai/*`), never as the caller-minted concrete
/// string — the #451 cardinality guard extended to resolvable names.
pub(crate) fn wildcard_row_name(snapshot: &AisixSnapshot, requested: &str) -> Option<String> {
    best_wildcard_row(snapshot, requested).map(|(entry, _)| entry.value.display_name.clone())
}

/// The `(display_name, model_name template)` pair of the wildcard row
/// serving `requested` — the bounded identities for BOTH metric labels:
/// with `model_name: "*"` the substituted upstream id is caller-derived
/// too, so `upstream_model` must label as the configured template, not
/// the capture.
pub(crate) fn wildcard_row_identity(
    snapshot: &AisixSnapshot,
    requested: &str,
) -> Option<(String, String)> {
    best_wildcard_row(snapshot, requested).map(|(entry, _)| {
        (
            entry.value.display_name.clone(),
            entry
                .value
                .model_name
                .clone()
                .unwrap_or_else(|| "*".to_string()),
        )
    })
}

/// The concrete upstream model a wildcard `row` dispatches for the
/// caller-facing name `requested`, as [`resolve_model`] builds it. `None`
/// when the row's pattern does not cover the name.
pub(crate) fn wildcard_upstream_model(row: &Model, requested: &str) -> Option<String> {
    let capture = wildcard_capture(&row.display_name, requested)?;
    Some(resolve_upstream_model_name(row, &capture))
}

/// Concrete upstream model id for a wildcard match: substitute the captured
/// segment into the `model_name` template's `*`, keep the template as-is when it
/// has no `*` (a fixed upstream for every match), or send the captured segment
/// verbatim when there is no template.
fn resolve_upstream_model_name(model: &Model, capture: &str) -> String {
    match model.model_name.as_deref() {
        Some(t) if t.contains('*') => t.replacen('*', capture, 1),
        Some(t) => t.to_string(),
        None => capture.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aisix_core::snapshot::ResourceTable;

    fn direct_model(display_name: &str, model_name: Option<&str>) -> Model {
        serde_json::from_value(serde_json::json!({
            "display_name": display_name,
            "provider": "openai",
            "model_name": model_name,
            "provider_key_id": "pk-1",
        }))
        .unwrap()
    }

    /// `row_serves_name` gates a name that did NOT arrive on a live request
    /// body — it decides whether the gateway will echo a caller-supplied
    /// string back as its own `model`. A row must accept every name it
    /// really serves and refuse everything else.
    #[test]
    fn row_serves_name_accepts_only_names_the_row_would_serve() {
        let exact = direct_model("wan-turbo", Some("wan-upstream"));
        assert!(row_serves_name(&exact, "wan-turbo"));
        assert!(!row_serves_name(&exact, "wan-turbo-forged"));
        assert!(!row_serves_name(&exact, "anything-at-all"));

        let wildcard = direct_model("wan/*", Some("wan-*"));
        // Every name the glob covers — the whole reason the poll echoes the
        // caller's string rather than the row's own.
        assert!(row_serves_name(&wildcard, "wan/turbo"));
        assert!(row_serves_name(&wildcard, "wan/plus"));
        // The pattern itself IS accepted, and deliberately so: `resolve_model`
        // resolves `wan/*` by exact name lookup before it ever tries globbing,
        // so that string is a name this row really serves. Narrowing it here
        // would make the echo refuse a name the dispatcher accepts.
        assert!(row_serves_name(&wildcard, "wan/*"));
        // A bare prefix is not covered by the glob.
        assert!(!row_serves_name(&wildcard, "wan"));
        // Outside the glob: a forged id must not get its string echoed.
        assert!(!row_serves_name(&wildcard, "other/turbo"));
        assert!(!row_serves_name(&wildcard, "anything-at-all"));
    }

    fn snapshot_with(models: Vec<(&str, Model)>) -> AisixSnapshot {
        let table = ResourceTable::default();
        for (id, model) in models {
            table.insert(ResourceEntry::new(id, model, 1));
        }
        AisixSnapshot {
            models: table,
            ..Default::default()
        }
    }

    #[test]
    fn exact_match_wins_over_wildcard() {
        let snap = snapshot_with(vec![
            ("m-star", direct_model("openai/*", Some("*"))),
            (
                "m-exact",
                direct_model("openai/gpt-4o", Some("gpt-4o-2024")),
            ),
        ]);
        let resolved = resolve_model(&snap, "openai/gpt-4o").unwrap();
        assert_eq!(resolved.id, "m-exact");
        assert_eq!(resolved.value.model_name.as_deref(), Some("gpt-4o-2024"));
    }

    /// Two equally specific wildcards both match: every table, whatever
    /// its insertion and iteration order, picks the same row.
    #[test]
    fn tied_wildcards_resolve_to_the_same_row_in_every_table() {
        for round in 0..64 {
            let rows = [
                ("m-a", direct_model("wan/*", Some("*"))),
                ("m-b", direct_model("*-pro", Some("*"))),
            ];
            let rows = if round % 2 == 0 {
                rows.into_iter().collect::<Vec<_>>()
            } else {
                rows.into_iter().rev().collect()
            };
            let snap = snapshot_with(rows);
            assert_eq!(resolve_model(&snap, "wan/x-pro").unwrap().id, "m-a");
        }
    }

    #[test]
    fn wildcard_substitutes_capture_into_template() {
        let snap = snapshot_with(vec![("m-star", direct_model("openai/*", Some("*")))]);
        let resolved = resolve_model(&snap, "openai/gpt-4o").unwrap();
        // Attribution stays on the wildcard Model; upstream id is the capture.
        assert_eq!(resolved.id, "m-star");
        assert_eq!(resolved.value.model_name.as_deref(), Some("gpt-4o"));
        // The resolved clone keeps the ROW's display_name — the bounded
        // identity metric labels and rate-limit buckets key on.
        assert_eq!(resolved.value.display_name, "openai/*");
    }

    #[test]
    fn wildcard_row_name_bounds_caller_minted_aliases() {
        let snap = snapshot_with(vec![
            ("m-star", direct_model("openai/*", Some("*"))),
            ("m-exact", direct_model("openai/gpt-4o", Some("gpt-4o"))),
        ]);
        // A caller-minted suffix maps to the serving row's name…
        assert_eq!(
            wildcard_row_name(&snap, "openai/anything-i-like").as_deref(),
            Some("openai/*")
        );
        // …and an unservable name maps to nothing.
        assert_eq!(wildcard_row_name(&snap, "azure/gpt-4o"), None);
    }

    #[test]
    fn wildcard_with_fixed_template_pins_all_matches() {
        let snap = snapshot_with(vec![("m-star", direct_model("gpt-*", Some("gpt-4o")))]);
        let resolved = resolve_model(&snap, "gpt-anything").unwrap();
        assert_eq!(resolved.value.model_name.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn most_specific_wildcard_wins() {
        let snap = snapshot_with(vec![
            ("m-broad", direct_model("openai/*", Some("*"))),
            ("m-narrow", direct_model("openai/gpt-*", Some("*"))),
        ]);
        let resolved = resolve_model(&snap, "openai/gpt-4o").unwrap();
        assert_eq!(resolved.id, "m-narrow");
        // `openai/gpt-*` captures only `4o`.
        assert_eq!(resolved.value.model_name.as_deref(), Some("4o"));
    }

    #[test]
    fn no_match_returns_none() {
        let snap = snapshot_with(vec![("m-star", direct_model("openai/*", Some("*")))]);
        assert!(resolve_model(&snap, "anthropic/claude").is_none());
    }

    /// An ensemble's members are not filtered by provider group, so to a key
    /// that names groups it answers exactly as a name nothing serves does.
    #[test]
    fn a_key_with_groups_cannot_address_an_ensemble() {
        let ensemble: Model = serde_json::from_value(serde_json::json!({
            "display_name": "panel",
            "ensemble": {
                "panel": [{"model": "m"}],
                "judge": {"model": "m"},
            },
        }))
        .unwrap();
        let allowed = vec!["grp-alpha".to_string()];
        let err = refuse_ensemble_to_grouped_key("panel", &ensemble, &allowed).unwrap_err();
        assert!(
            matches!(err, ProxyError::ModelNotFound(ref m) if m == "panel"),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            ProxyError::ModelNotFound("panel".into()).to_string()
        );
        // A key without groups addresses it as before, and a direct model is
        // not this check's business.
        assert!(refuse_ensemble_to_grouped_key("panel", &ensemble, &[]).is_ok());
        assert!(
            refuse_ensemble_to_grouped_key("m", &direct_model("m", Some("up")), &allowed).is_ok()
        );
    }

    #[test]
    fn wildcard_upstream_model_rebuilds_what_dispatch_sent() {
        let row = direct_model("openai/*", Some("*"));
        assert_eq!(
            wildcard_upstream_model(&row, "openai/gpt-4o").as_deref(),
            Some("gpt-4o")
        );
        assert_eq!(wildcard_upstream_model(&row, "anthropic/claude"), None);
    }
}
