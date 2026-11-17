use super::*;
use std::sync::Arc;

// 仅缓存配置账号池，不保存任何会话标识或会话归属。
pub(super) struct FixedHashPool {
    owner: Arc<Model>,
    models_generation: u64,
    accounts_generation: u64,
    allowed_groups: Vec<String>,
    targets: Vec<AttemptModel>,
    tag_targets: Vec<RoutingTarget>,
    ring: HashRing,
}

impl FixedHashPool {
    fn matches(&self, snapshot: &AisixSnapshot, owner: &Arc<Model>, groups: &[String]) -> bool {
        Arc::ptr_eq(&self.owner, owner)
            && self.models_generation == snapshot.models.generation()
            && self.accounts_generation == snapshot.provider_keys.generation()
            && self.allowed_groups == groups
    }

    fn build(
        snapshot: &AisixSnapshot,
        owner: &Arc<Model>,
        routing: &Routing,
        groups: &[String],
    ) -> Result<Self, ProxyError> {
        let mut targets = Vec::with_capacity(routing.targets.len());
        let mut declarations = Vec::with_capacity(routing.targets.len());
        let mut seen = std::collections::HashSet::with_capacity(routing.targets.len());
        let mut excluded_by_groups = false;
        for configured in &routing.targets {
            // 先完整解析配置；缺失或已撤销的引用不能悄悄从环中消失。
            let entry = match configured.model_id.as_deref() {
                Some(id) => snapshot.models.get_by_id(id),
                None => snapshot.models.get_by_name(&configured.model),
            }
            .ok_or(ProxyError::ProviderUnavailable)?;
            let account_id = entry
                .value
                .provider_key_id
                .as_deref()
                .ok_or(ProxyError::ProviderUnavailable)?;
            let account = snapshot
                .provider_keys
                .get_by_id(account_id)
                .ok_or(ProxyError::ProviderUnavailable)?;
            if !provider_key_in_groups(&account.value, groups) {
                excluded_by_groups = true;
                continue;
            }
            if !seen.insert(entry.id.clone()) {
                continue;
            }
            targets.push(AttemptModel {
                id: entry.id.clone(),
                model: entry.value.clone(),
                priority: configured.priority_or_default(),
                weight: configured.weight_or_default(),
            });
            // 环点以真实账号资源标识命名，展示别名不参与本模式的归属。
            declarations.push(RoutingTarget {
                model: account_id.to_string(),
                model_id: Some(entry.id.clone()),
                ..configured.clone()
            });
        }
        if targets.is_empty() && excluded_by_groups {
            log_provider_group_refusal();
            return Err(ProxyError::ProviderGroupsPoolEmpty);
        }
        let positive = targets.iter().any(|target| target.weight > 0);
        let priority = targets
            .iter()
            .filter(|target| !positive || target.weight > 0)
            .map(|target| target.priority)
            .max()
            .ok_or(ProxyError::ProviderUnavailable)?;
        let keep: Vec<_> = targets
            .iter()
            .enumerate()
            .filter_map(|(index, target)| {
                (target.priority == priority && (!positive || target.weight > 0)).then_some(index)
            })
            .collect();
        if keep.is_empty() {
            return Err(ProxyError::ProviderUnavailable);
        }
        let selected_declarations: Vec<_> = keep
            .iter()
            .map(|&index| declarations[index].clone())
            .collect();
        let ring = HashRing::build(
            tier_fingerprint(&selected_declarations),
            &selected_declarations,
            RingScale::of(selected_declarations.iter()),
        );
        Ok(Self {
            owner: owner.clone(),
            models_generation: snapshot.models.generation(),
            accounts_generation: snapshot.provider_keys.generation(),
            allowed_groups: groups.to_vec(),
            targets: keep.iter().map(|&index| targets[index].clone()).collect(),
            tag_targets: declarations,
            ring,
        })
    }
}

pub(super) fn request_key(
    routing: &Routing,
    req: &RoutingRequest<'_>,
) -> Result<String, ProxyError> {
    if routing.strategy != RoutingStrategy::ConsistentHash
        || routing.hash_on.as_ref().is_none_or(Vec::is_empty)
    {
        return Err(ProxyError::InvalidRequest(
            "routing.hash_failover=false requires consistent_hash and an explicit hash_on chain"
                .into(),
        ));
    }
    let hash_key = resolve_hash_key(routing, req);
    if hash_key.is_empty() {
        return Err(ProxyError::InvalidRequest(
            "routing.hash_on did not find a non-empty request header or other configured source"
                .into(),
        ));
    }
    Ok(hash_key)
}

#[inline(never)]
pub(super) fn resolve(
    registry: &RoutingRegistry,
    runtime: &crate::ModelRuntimeStatusTracker,
    snapshot: &AisixSnapshot,
    name: &str,
    owner: &Arc<Model>,
    req: RoutingRequest<'_>,
) -> Result<Vec<AttemptModel>, ProxyError> {
    let routing = owner
        .routing
        .as_ref()
        .ok_or(ProxyError::ProviderUnavailable)?;
    let hash_key = request_key(routing, &req)?;
    let mut scope = FNV_OFFSET_BASIS;
    for group in req.allowed_groups {
        scope = fnv1a_extend(scope, &(group.len() as u64).to_le_bytes());
        scope = fnv1a_extend(scope, group.as_bytes());
    }
    let key = (name.to_string(), scope);
    let cached = registry
        .fixed_pools
        .get(&key)
        .filter(|pool| pool.matches(snapshot, owner, req.allowed_groups))
        .map(|pool| pool.clone());
    let pool = match cached {
        Some(pool) => pool,
        None => {
            let pool = Arc::new(FixedHashPool::build(
                snapshot,
                owner,
                routing,
                req.allowed_groups,
            )?);
            registry.fixed_pools.insert(key, pool.clone());
            pool
        }
    };
    let key_hash = stable_hash(&hash_key);
    let start = pool
        .ring
        .points
        .partition_point(|(point, _)| *point < key_hash);
    let point = pool
        .ring
        .points
        .get(start)
        .or_else(|| pool.ring.points.first())
        .ok_or(ProxyError::ProviderUnavailable)?;
    let selected = pool
        .targets
        .get(point.1 as usize)
        .ok_or(ProxyError::ProviderUnavailable)?;

    // 从这里起只能检查选中的账号，任何拒绝都不能触发缩池重算。
    if !selected.model.ip_allowed(req.source_ip) {
        return Err(ProxyError::ModelIpRestricted(name.to_string()));
    }
    let allowed_by_tags = eligible_targets(&pool.tag_targets, req.tags);
    if !allowed_by_tags
        .iter()
        .any(|target| target.model_id.as_deref() == Some(selected.id.as_str()))
        || req.serves.is_some_and(|serves| !serves(&selected.model))
    {
        return Err(ProxyError::ProviderUnavailable);
    }
    if let Some(gate) = req.budget {
        gate.check_model(&selected.model)?;
    }
    if let Some(origin) = req.context_origin {
        let boundary = crate::responses_affinity::Boundary::of(snapshot, origin);
        if !boundary.contains(&selected.id, selected.model.provider_key_id.as_deref()) {
            return Err(ProxyError::ProviderUnavailable);
        }
    }
    let stale_after = selected
        .model
        .background_model_check
        .as_ref()
        .map(|check| Duration::from_secs(check.stale_after_seconds));
    if matches!(
        runtime.status_with_stale(&selected.id, stale_after).status,
        crate::RuntimeStatus::Unhealthy | crate::RuntimeStatus::Cooldown
    ) {
        return Err(ProxyError::AllCandidatesUnavailable {
            retry_after_secs: Some(FALLBACK_ALL_UNHEALTHY_RETRY_AFTER.as_secs()),
        });
    }
    Ok(vec![selected.clone()])
}
