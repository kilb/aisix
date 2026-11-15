use super::*;

impl HashRing {
    pub(super) fn from_sorted_points(
        fingerprint: u64,
        points: Vec<(u64, u32)>,
        declarations: usize,
    ) -> Self {
        let mut declaration_offsets = vec![0usize; declarations + 1];
        for &(_, declaration) in &points {
            declaration_offsets[declaration as usize + 1] += 1;
        }
        for declaration in 0..declarations {
            declaration_offsets[declaration + 1] += declaration_offsets[declaration];
        }
        let mut next = declaration_offsets[..declarations].to_vec();
        let mut point_positions = vec![0u32; points.len()];
        for (position, &(_, declaration)) in points.iter().enumerate() {
            let declaration = declaration as usize;
            point_positions[next[declaration]] = position as u32;
            next[declaration] += 1;
        }
        Self {
            fingerprint,
            points,
            point_positions,
            declaration_offsets,
            configured: None,
        }
    }

    /// 首次遇见每个存活声明的位置决定其整个 fallback 次序；不遍历被过滤的点。
    pub(super) fn subset_preference_order(
        &self,
        key_hash: u64,
        declarations: &[usize],
    ) -> Vec<u32> {
        if declarations.len() == self.declaration_offsets.len() - 1 {
            return self.preference_order(key_hash, declarations.len());
        }
        let start = self.points.partition_point(|(point, _)| *point < key_hash);
        let mut small = [(0usize, 0usize); 8];
        let mut larger;
        let successors = if declarations.len() <= small.len() {
            &mut small[..declarations.len()]
        } else {
            larger = vec![(0usize, 0usize); declarations.len()];
            &mut larger[..]
        };
        for (index, &declaration) in declarations.iter().enumerate() {
            let positions = &self.point_positions
                [self.declaration_offsets[declaration]..self.declaration_offsets[declaration + 1]];
            let successor = positions.partition_point(|position| (*position as usize) < start);
            let position = positions[if successor == positions.len() {
                0
            } else {
                successor
            }];
            successors[index] = (position as usize, index);
        }
        successors.sort_unstable_by_key(|&(position, _)| (position < start, position));
        successors.iter().map(|&(_, index)| index as u32).collect()
    }
}

pub(super) struct ConfiguredTier {
    owner: std::sync::Arc<Model>,
    model_generation: std::sync::atomic::AtomicU64,
    declarations: Vec<usize>,
    names: Vec<String>,
    lookup: Vec<u32>,
}

fn with_configured_name<T>(
    snapshot: &AisixSnapshot,
    target: &RoutingTarget,
    read: impl FnOnce(&str) -> T,
) -> T {
    let resolved = target.model_id.as_deref().and_then(|id| {
        #[cfg(test)]
        performance_regression_tests::note_configured_id_lookup();
        snapshot.models.get_by_id(id)
    });
    let name = resolved
        .as_ref()
        .map(|entry| entry.value.display_name.as_str())
        .unwrap_or_else(|| target.model_id.as_deref().unwrap_or(target.model.as_str()));
    read(name)
}

fn target_identity_cmp(
    left_name: &str,
    left: &RoutingTarget,
    right_name: &str,
    right: &RoutingTarget,
) -> std::cmp::Ordering {
    left_name
        .cmp(right_name)
        .then_with(|| left.weight.cmp(&right.weight))
        .then_with(|| left.priority.cmp(&right.priority))
        .then_with(|| left.tags.cmp(&right.tags))
}

impl ConfiguredTier {
    fn matches(
        &self,
        snapshot: &AisixSnapshot,
        owner: &std::sync::Arc<Model>,
        routing: &Routing,
        generation: u64,
    ) -> bool {
        use std::sync::atomic::Ordering;
        if !std::sync::Arc::ptr_eq(&self.owner, owner) {
            return false;
        }
        if self.model_generation.load(Ordering::Acquire) == generation {
            return true;
        }
        // 只在相关模型表变更时核对引用；改动未改变名字则复用原点，不因 API key 写入失效。
        let unchanged = self
            .declarations
            .iter()
            .zip(&self.names)
            .all(|(&index, name)| {
                with_configured_name(snapshot, &routing.targets[index], |current| current == name)
            });
        if unchanged {
            self.model_generation.store(generation, Ordering::Release);
        }
        unchanged
    }

    fn find(&self, routing: &Routing, target: &RoutingTarget) -> usize {
        let compare = |declaration: u32| {
            let declaration = declaration as usize;
            target_identity_cmp(
                &self.names[declaration],
                &routing.targets[self.declarations[declaration]],
                &target.model,
                target,
            )
        };
        // 相同归一化元数据的声明在去重前有相同 group/tag 资格，因此保留第一次声明。
        let position = self
            .lookup
            .partition_point(|&declaration| compare(declaration).is_lt());
        let declaration = self.lookup[position];
        debug_assert!(compare(declaration).is_eq());
        declaration as usize
    }
}

impl RoutingRegistry {
    /// 缓存以请求地址所在的配置组和层为身份；direct、wildcard、ensemble、semantic 与
    /// embedding 不经过此排序。owner 由 Arc 保活，避免地址复用；只读取 models generation，
    /// 不读取全 snapshot version。模型更名重新建环，并发旧快照请求仍持有自己的环。
    pub(super) fn pick_configured_hash_indices(
        &self,
        snapshot: &AisixSnapshot,
        virtual_name: &str,
        owner: &std::sync::Arc<Model>,
        targets: &[RoutingTarget],
        hash_key: &str,
    ) -> Vec<usize> {
        if targets.len() < 2 {
            return (0..targets.len()).collect();
        }
        let Some(routing) = owner.routing.as_ref() else {
            unreachable!("configured hash selection requires a routing model");
        };
        let mut order = Vec::with_capacity(targets.len());
        let priorities = priority_order(targets);
        let generation = snapshot.models.generation();
        let key_hash = stable_hash(hash_key);
        for tier in priorities.chunk_by(|left, right| {
            targets[*left].priority_or_default() == targets[*right].priority_or_default()
        }) {
            if tier.len() == 1 {
                order.push(tier[0]);
                continue;
            }
            let priority = targets[tier[0]].priority_or_default();
            let key = (virtual_name.to_string(), priority);
            let cached = self
                .rings
                .get(&key)
                .filter(|ring| {
                    ring.configured.as_ref().is_some_and(|configured| {
                        configured.matches(snapshot, owner, routing, generation)
                    })
                })
                .map(|ring| ring.clone());
            let ring = cached.unwrap_or_else(|| {
                let declarations: Vec<_> = routing
                    .targets
                    .iter()
                    .enumerate()
                    .filter(|(_, target)| target.priority_or_default() == priority)
                    .map(|(index, _)| index)
                    .collect();
                let names: Vec<_> = declarations
                    .iter()
                    .map(|&index| {
                        with_configured_name(snapshot, &routing.targets[index], str::to_owned)
                    })
                    .collect();
                let mut lookup: Vec<_> = (0..declarations.len() as u32).collect();
                lookup.sort_unstable_by(|&left, &right| {
                    let left_index = left as usize;
                    let right_index = right as usize;
                    target_identity_cmp(
                        &names[left_index],
                        &routing.targets[declarations[left_index]],
                        &names[right_index],
                        &routing.targets[declarations[right_index]],
                    )
                    .then_with(|| left.cmp(&right))
                });
                let scale =
                    RingScale::of(declarations.iter().map(|&index| &routing.targets[index]));
                let fingerprint =
                    tier_fingerprint(declarations.iter().map(|&index| &routing.targets[index]));
                let mut next = 0;
                let mut ring = HashRing::build_named(
                    fingerprint,
                    declarations.iter().map(|&index| &routing.targets[index]),
                    scale,
                    |_| {
                        let name = names[next].as_str();
                        next += 1;
                        std::borrow::Cow::Borrowed(name)
                    },
                );
                ring.configured = Some(ConfiguredTier {
                    owner: owner.clone(),
                    model_generation: std::sync::atomic::AtomicU64::new(generation),
                    declarations,
                    names,
                    lookup,
                });
                let ring = std::sync::Arc::new(ring);
                self.rings.insert(key, ring.clone());
                ring
            });
            let Some(configured) = ring.configured.as_ref() else {
                unreachable!("configured hash rings retain declaration identity");
            };
            let mut small = [0usize; 8];
            let mut larger;
            let declarations = if tier.len() <= small.len() {
                &mut small[..tier.len()]
            } else {
                larger = vec![0usize; tier.len()];
                &mut larger[..]
            };
            let configured_len = configured.declarations.len();
            let binary_work = tier
                .len()
                .saturating_mul(configured_len.ilog2() as usize + 1);
            if binary_work >= configured_len {
                // 密集候选仍是配置的稳定子序列；归并避免为每个候选重复二分字符串。
                let mut next = 0;
                for (declaration, &index) in configured.declarations.iter().enumerate() {
                    if target_identity_cmp(
                        &configured.names[declaration],
                        &routing.targets[index],
                        &targets[tier[next]].model,
                        &targets[tier[next]],
                    )
                    .is_eq()
                    {
                        declarations[next] = declaration;
                        next += 1;
                        if next == tier.len() {
                            break;
                        }
                    }
                }
                debug_assert_eq!(next, tier.len());
            } else {
                for (declaration, &index) in declarations.iter_mut().zip(tier) {
                    *declaration = configured.find(routing, &targets[index]);
                }
            }
            order.extend(
                ring.subset_preference_order(key_hash, declarations)
                    .into_iter()
                    .map(|index| tier[index as usize]),
            );
        }
        order.truncate(routing.max_fallbacks_or_default() + 1);
        order
    }
}
