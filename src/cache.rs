//! Strictly bounded, project-isolated derived caches.
//!
//! Cache entries are never canonical state. Callers supply an estimated byte
//! weight, and every key is scoped by both project and storage generation.

use std::collections::VecDeque;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use moka::sync::Cache;
use serde::Serialize;

use crate::ids::ProjectId;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct VersionedKey<K> {
    generation: u64,
    key: K,
}

#[derive(Debug)]
struct WeightedValue<V> {
    value: Arc<V>,
    weight_bytes: u32,
}

struct ProjectPartition<K, V> {
    project_id: ProjectId,
    cache: Cache<VersionedKey<K>, Arc<WeightedValue<V>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheAdmission {
    Admitted,
    RejectedOversized,
    RejectedUnavailable,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct CacheStats {
    pub active_projects: usize,
    pub entries: u64,
    pub weighted_bytes: u64,
    pub total_budget_bytes: u64,
    pub per_project_budget_bytes: u64,
}

/// A byte-weighted cache partitioned by project.
///
/// At most `max_active_projects` partitions exist. Each receives an equal hard
/// share of the global budget, which guarantees bounded aggregate capacity.
/// Recently accessed projects displace the least-recently accessed partition.
pub struct PartitionedCache<K, V> {
    partitions: Mutex<VecDeque<ProjectPartition<K, V>>>,
    total_budget_bytes: u64,
    per_project_budget_bytes: u64,
    max_item_bytes: u32,
    max_active_projects: usize,
}

impl<K, V> PartitionedCache<K, V>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    /// Construct a cache with deterministic hard capacity limits.
    ///
    /// # Panics
    ///
    /// Panics when any budget is zero or when `max_item_bytes` exceeds a
    /// project's budget. Configuration validation prevents this in production.
    #[must_use]
    pub fn new(total_budget_bytes: u64, max_active_projects: usize, max_item_bytes: u32) -> Self {
        assert!(total_budget_bytes > 0, "cache budget must be positive");
        assert!(max_active_projects > 0, "active projects must be positive");
        let per_project_budget_bytes =
            total_budget_bytes / u64::try_from(max_active_projects).expect("usize fits u64");
        assert!(
            per_project_budget_bytes > 0,
            "per-project budget must be positive"
        );
        assert!(
            u64::from(max_item_bytes) <= per_project_budget_bytes,
            "maximum item must fit a project partition"
        );
        Self {
            partitions: Mutex::new(VecDeque::with_capacity(max_active_projects)),
            total_budget_bytes,
            per_project_budget_bytes,
            max_item_bytes,
            max_active_projects,
        }
    }

    #[must_use]
    pub fn get(&self, project_id: &ProjectId, generation: u64, key: &K) -> Option<Arc<V>> {
        let cache = self.partition(project_id, false)?;
        cache
            .get(&VersionedKey {
                generation,
                key: key.clone(),
            })
            .map(|entry| Arc::clone(&entry.value))
    }

    pub fn insert(
        &self,
        project_id: &ProjectId,
        generation: u64,
        key: K,
        value: V,
        weight_bytes: u32,
    ) -> CacheAdmission {
        if weight_bytes > self.max_item_bytes
            || u64::from(weight_bytes) > self.per_project_budget_bytes
        {
            return CacheAdmission::RejectedOversized;
        }
        let Some(cache) = self.partition(project_id, true) else {
            return CacheAdmission::RejectedUnavailable;
        };
        cache.insert(
            VersionedKey { generation, key },
            Arc::new(WeightedValue {
                value: Arc::new(value),
                weight_bytes,
            }),
        );
        CacheAdmission::Admitted
    }

    pub fn invalidate_project(&self, project_id: &ProjectId) {
        let mut partitions = self
            .partitions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = partitions
            .iter()
            .position(|partition| &partition.project_id == project_id)
            && let Some(partition) = partitions.remove(index)
        {
            partition.cache.invalidate_all();
            partition.cache.run_pending_tasks();
        }
    }

    pub fn invalidate_all(&self) {
        let mut partitions = self
            .partitions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for partition in partitions.drain(..) {
            partition.cache.invalidate_all();
            partition.cache.run_pending_tasks();
        }
    }

    /// Flush maintenance queues and report cache-resident logical weight.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let partitions = self
            .partitions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut entries = 0;
        let mut weighted_bytes = 0;
        for partition in partitions.iter() {
            partition.cache.run_pending_tasks();
            entries += partition.cache.entry_count();
            weighted_bytes += partition.cache.weighted_size();
        }
        CacheStats {
            active_projects: partitions.len(),
            entries,
            weighted_bytes,
            total_budget_bytes: self.total_budget_bytes,
            per_project_budget_bytes: self.per_project_budget_bytes,
        }
    }

    fn partition(
        &self,
        project_id: &ProjectId,
        create: bool,
    ) -> Option<Cache<VersionedKey<K>, Arc<WeightedValue<V>>>> {
        let mut partitions = self
            .partitions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = partitions
            .iter()
            .position(|partition| &partition.project_id == project_id)
        {
            let partition = partitions.remove(index)?;
            let cache = partition.cache.clone();
            partitions.push_back(partition);
            return Some(cache);
        }
        if !create {
            return None;
        }
        if partitions.len() == self.max_active_projects
            && let Some(evicted) = partitions.pop_front()
        {
            evicted.cache.invalidate_all();
            evicted.cache.run_pending_tasks();
        }
        let cache = Cache::builder()
            .max_capacity(self.per_project_budget_bytes)
            .weigher(|_key: &VersionedKey<K>, value: &Arc<WeightedValue<V>>| value.weight_bytes)
            .build();
        partitions.push_back(ProjectPartition {
            project_id: project_id.clone(),
            cache: cache.clone(),
        });
        Some(cache)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(value: &str) -> ProjectId {
        ProjectId::parse(value).expect("project id")
    }

    #[test]
    fn project_scope_isolation_is_enforced() {
        let cache = PartitionedCache::new(1_024, 2, 256);
        let first = project("prj_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        let second = project("prj_bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
        cache.insert(&first, 1, "key", "first", 32);
        cache.insert(&second, 1, "key", "second", 32);
        assert_eq!(cache.get(&first, 1, &"key").as_deref(), Some(&"first"));
        assert_eq!(cache.get(&second, 1, &"key").as_deref(), Some(&"second"));
    }

    #[test]
    fn storage_generation_is_part_of_every_key() {
        let cache = PartitionedCache::new(1_024, 2, 256);
        let project = project("prj_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        cache.insert(&project, 7, "key", "value", 32);
        assert!(cache.get(&project, 7, &"key").is_some());
        assert!(cache.get(&project, 8, &"key").is_none());
    }

    #[test]
    fn oversized_objects_bypass_cache() {
        let cache = PartitionedCache::new(1_024, 2, 128);
        let project = project("prj_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        let admission = cache.insert(&project, 1, "large", vec![0_u8; 256], 256);
        assert_eq!(admission, CacheAdmission::RejectedOversized);
        assert!(cache.get(&project, 1, &"large").is_none());
    }

    #[test]
    fn active_project_partitions_are_bounded() {
        let cache = PartitionedCache::new(1_024, 2, 128);
        let first = project("prj_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        let second = project("prj_bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
        let third = project("prj_cccccccc-cccc-4ccc-8ccc-cccccccccccc");
        cache.insert(&first, 1, "key", "first", 32);
        cache.insert(&second, 1, "key", "second", 32);
        cache.insert(&third, 1, "key", "third", 32);
        assert!(cache.get(&first, 1, &"key").is_none());
        assert_eq!(cache.stats().active_projects, 2);
    }

    #[test]
    fn invalidation_drops_entire_project_partition() {
        let cache = PartitionedCache::new(1_024, 2, 128);
        let project = project("prj_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        cache.insert(&project, 1, "a", "one", 32);
        cache.insert(&project, 1, "b", "two", 32);
        cache.invalidate_project(&project);
        assert!(cache.get(&project, 1, &"a").is_none());
        assert_eq!(cache.stats().active_projects, 0);
    }

    #[test]
    fn weighted_capacity_is_hard_bounded() {
        let cache = PartitionedCache::new(1_024, 2, 256);
        let project = project("prj_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        for index in 0..20 {
            cache.insert(&project, 1, index, index, 128);
        }
        let stats = cache.stats();
        assert!(stats.weighted_bytes <= stats.per_project_budget_bytes);
    }
}
