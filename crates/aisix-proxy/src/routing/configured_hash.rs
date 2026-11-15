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

impl RoutingRegistry {
    /// 缓存以请求地址所在的配置组和层为身份，不以派发模型或请求子集为身份。
    /// direct、wildcard、ensemble、semantic 与 embedding 不经过此组内排序。
    /// model_id 的名字取自本次快照；模型更名改变完整层 fingerprint，旧快照不借用新名字。
    pub(super) fn pick_configured_hash_indices(
        &self,
        snapshot: &AisixSnapshot,
        virtual_name: &str,
        routing: &Routing,
        targets: &[RoutingTarget],
        hash_key: &str,
    ) -> Vec<usize> {
        if targets.len() < 2 {
            return (0..targets.len()).collect();
        }
        let mut order = Vec::with_capacity(targets.len());
        let priorities = priority_order(targets);
        let configured = priority_order(&routing.targets);
        let mut configured_tiers = configured
            .chunk_by(|left, right| {
                routing.targets[*left].priority_or_default()
                    == routing.targets[*right].priority_or_default()
            })
            .peekable();
        let key_hash = stable_hash(hash_key);
        for tier in priorities.chunk_by(|left, right| {
            targets[*left].priority_or_default() == targets[*right].priority_or_default()
        }) {
            if tier.len() == 1 {
                order.push(tier[0]);
                continue;
            }
            let priority = targets[tier[0]].priority_or_default();
            while configured_tiers.peek().is_some_and(|configured| {
                routing.targets[configured[0]].priority_or_default() > priority
            }) {
                configured_tiers.next();
            }
            let Some(configured) = configured_tiers.next_if(|configured| {
                routing.targets[configured[0]].priority_or_default() == priority
            }) else {
                unreachable!("surviving routing targets retain their configured priority");
            };
            let scale = RingScale::of(configured.iter().map(|index| &routing.targets[*index]));
            let mut small = [0usize; 8];
            let mut larger;
            let declarations = if tier.len() <= small.len() {
                &mut small[..tier.len()]
            } else {
                larger = vec![0usize; tier.len()];
                &mut larger[..]
            };
            let mut next = 0;
            let mut fingerprint = FNV_OFFSET_BASIS;
            for (declaration, &index) in configured.iter().enumerate() {
                let target = &routing.targets[index];
                // 持有当前快照条目并借用名称；热环查询不为每个 ID 声明复制字符串。
                let resolved = target.model_id.as_deref().and_then(|id| {
                    #[cfg(test)]
                    performance_regression_tests::note_configured_id_lookup();
                    snapshot.models.get_by_id(id)
                });
                let name = resolved
                    .as_ref()
                    .map(|entry| entry.value.display_name.as_str())
                    .unwrap_or_else(|| target.model_id.as_deref().unwrap_or(target.model.as_str()));
                fingerprint = fnv1a_extend(fingerprint, name.as_bytes());
                fingerprint = fnv1a_extend(fingerprint, &[0, 0]);
                fingerprint = fnv1a_extend(fingerprint, &target.weight_or_default().to_le_bytes());
                fingerprint = fnv1a_extend(fingerprint, &[1]);
                // available 保持配置子序列，且只改过 model/model_id；一次单调归并恢复声明位置。
                // 若两声明归一化后连 tags 都相同，它们在去重前的 group/tag 筛选也相同，
                // 所以被保留的一定是前者，无需逐候选从配置头部反查。
                if tier.get(next).is_some_and(|&index| {
                    let candidate = &targets[index];
                    name == candidate.model
                        && target.weight == candidate.weight
                        && target.priority == candidate.priority
                        && target.tags == candidate.tags
                }) {
                    declarations[next] = declaration;
                    next += 1;
                }
            }
            debug_assert_eq!(next, tier.len());
            fingerprint = fnv1a_extend(fingerprint, &scale.gcd.to_le_bytes());
            fingerprint = fnv1a_extend(fingerprint, &scale.sum.to_le_bytes());
            let key = (virtual_name.to_string(), priority);
            let cached = self
                .rings
                .get(&key)
                .filter(|ring| ring.fingerprint == fingerprint)
                .map(|ring| ring.clone());
            let ring = cached.unwrap_or_else(|| {
                let ring = std::sync::Arc::new(HashRing::build_named(
                    fingerprint,
                    configured.iter().map(|index| &routing.targets[*index]),
                    scale,
                    |target| target.model_ref(snapshot),
                ));
                self.rings.insert(key, ring.clone());
                ring
            });
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
