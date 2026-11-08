//! `GET /admin/provider-keys/{id}/upstream-models`: an upstream account's own
//! model list, for the Dashboard's pickers.
//!
//! Operator-only, and read-only both ways: it answers the ids the account
//! serves and never writes a price, a charge or a stored fact. What it
//! returns becomes a baseline, a rate or a document only when an operator
//! acts on the same page.
//!
//! The list is cached for [`CACHE_TTL`], keyed by the provider key's id **and
//! its document version**, so editing the account (a new `api_base`, a new
//! credential) misses at once. One call per key is in flight at a time; a
//! second concurrent request waits for it and shares the answer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aisix_core::models::ProviderKey;
use axum::extract::{Path, State};
use axum::Json;
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use crate::auth::{OperatorAuth, OperatorRole};
use crate::error::ApiError;
use crate::resources::{self, store, Kind};
use crate::upstream_http::{self, Fetched};
use crate::AppState;

/// How long one answer is reused.
pub const CACHE_TTL: Duration = Duration::from_secs(60);

/// The most provider keys whose answers are held at once. Every entry has the
/// same lifetime, so the first that would expire is the one fetched first:
/// that one goes before a new one is added.
pub const CACHE_ENTRIES: usize = 64;

/// Every cached answer, and the one call in flight per provider key.
#[derive(Default)]
pub struct Cache {
    answers: Mutex<Answers>,
    /// One lock per provider key id, so concurrent requests for the same
    /// account share one upstream call.
    in_flight: Mutex<HashMap<Uuid, Arc<AsyncMutex<()>>>>,
}

#[derive(Default)]
struct Answers {
    /// `(provider key id, its document version)` → when it was fetched and
    /// what it said.
    entries: HashMap<(Uuid, i64), (Instant, Fetched)>,
}

impl Cache {
    fn get(&self, key: (Uuid, i64)) -> Option<Fetched> {
        let now = Instant::now();
        let mut answers = self.answers.lock().unwrap_or_else(|p| p.into_inner());
        let (at, fetched) = answers.entries.get(&key)?;
        if now.duration_since(*at) >= CACHE_TTL {
            answers.entries.remove(&key);
            return None;
        }
        Some(fetched.clone())
    }

    fn put(&self, key: (Uuid, i64), fetched: Fetched) {
        let now = Instant::now();
        let mut answers = self.answers.lock().unwrap_or_else(|p| p.into_inner());
        answers
            .entries
            .retain(|_, (at, _)| now.duration_since(*at) < CACHE_TTL);
        if answers.entries.len() >= CACHE_ENTRIES && !answers.entries.contains_key(&key) {
            // The entry that would expire first is the one fetched first.
            if let Some(oldest) = answers
                .entries
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| *k)
            {
                answers.entries.remove(&oldest);
            }
        }
        answers.entries.insert(key, (now, fetched));
    }

    /// The lock one request for `id` holds while its upstream call is in
    /// flight. Dropped from the map when nobody else is waiting on it, so a
    /// control plane that has listed a thousand accounts does not keep a
    /// thousand locks.
    fn gate(&self, id: Uuid) -> Arc<AsyncMutex<()>> {
        let mut in_flight = self.in_flight.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(gate) = in_flight.get(&id) {
            if Arc::strong_count(gate) > 1 {
                return gate.clone();
            }
        }
        let gate = Arc::new(AsyncMutex::new(()));
        in_flight.insert(id, gate.clone());
        gate
    }

    fn release(&self, id: Uuid, gate: &Arc<AsyncMutex<()>>) {
        let mut in_flight = self.in_flight.lock().unwrap_or_else(|p| p.into_inner());
        // This caller still holds a reference; the gate goes once no one
        // else does.
        if Arc::strong_count(gate) <= 2 && in_flight.get(&id).is_some_and(|g| Arc::ptr_eq(g, gate))
        {
            in_flight.remove(&id);
        }
    }
}

/// `GET /admin/provider-keys/{id}/upstream-models`.
pub async fn list(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    // A connection is held only while the row is read; the fetch below runs
    // without one, so an upstream that takes five seconds never ties one up.
    // The document is parsed sealed — nothing read here needs the credential
    // — so a cache hit never pays a decrypt.
    let (doc, version) = {
        let mut conn = state
            .db
            .pool()
            .acquire()
            .await
            .map_err(crate::db::DbError::from)?;
        let row = store::get(&mut conn, Kind::ProviderKeys, id)
            .await?
            .filter(|row| !row.deleted)
            .ok_or_else(ApiError::not_found)?;
        (row.doc.clone(), row.version)
    };
    let shape: ProviderKey = serde_json::from_value(doc.clone())
        .map_err(|e| ApiError::upstream_bad_response(format!("该上游账号无法读取：{e}")))?;
    // Where the call would go, reduced to the origin for the log line: a key
    // with no `api_base` has no URL of its own, and printing
    // `redact_url_for_log("")`'s `<invalid URL>` would only confuse.
    let where_ = upstream_http::base_url(&shape)
        .map(|url| aisix_core::redact_url_for_log(url.as_str()))
        .unwrap_or_else(|_| "-".to_string());

    let key = (id, version);
    let fetched = match state.upstream_models.get(key) {
        Some(fetched) => fetched,
        None => {
            // Only a request that reaches the upstream unseals: the gate is
            // held across the call, so the other waiters reuse its answer
            // and decrypt nothing.
            let mut doc = doc;
            resources::secrets::unseal(&state.cfg.master_key, Kind::ProviderKeys, id, &mut doc)
                .map_err(ApiError::internal)?;
            let pk: ProviderKey = serde_json::from_value(doc)
                .map_err(|e| ApiError::upstream_bad_response(format!("该上游账号无法读取：{e}")))?;
            let gate = state.upstream_models.gate(id);
            let answer: Result<Fetched, ApiError> = {
                let gate = gate.clone();
                async {
                    let _held = gate.lock().await;
                    // Whoever held the gate may have just filled the cache.
                    if let Some(fetched) = state.upstream_models.get(key) {
                        return Ok(fetched);
                    }
                    let fetched =
                        upstream_http::fetch_models(&state.upstream, &pk, &pk.api_key).await?;
                    state.upstream_models.put(key, fetched.clone());
                    Ok(fetched)
                }
                .await
            };
            state.upstream_models.release(id, &gate);
            answer?
        }
    };

    tracing::info!(
        provider_key = %id,
        upstream = %where_,
        models = fetched.ids.len(),
        truncated = fetched.truncated,
        "listed an upstream account's models"
    );

    Ok(Json(json!({
        "items": fetched.ids.iter().map(|id| json!({"id": id})).collect::<Vec<_>>(),
        "fetched_at": fetched.fetched_at,
        "truncated": fetched.truncated,
        "warnings": fetched.warnings,
    })))
}
