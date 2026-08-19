//! Lock-free configuration snapshot.
//!
//! The gateway holds an `ArcSwap` publication containing a snapshot and its
//! generation. Reads are a single atomic load — no mutex, no RCU dance in user
//! code. Writes build a fresh snapshot off the etcd watch thread and atomically
//! replace the publication (spec §2: "no mutex on the read path, atomic replace
//! on write").
//!
//! A [`Snapshot`] holds a [`ResourceTable<T>`] per entity kind. Each table
//! provides:
//! - O(1) `get_by_id` via a primary `DashMap<id, Arc<ResourceEntry<T>>>`
//! - O(1) `get_by_name` via a secondary `DashMap<name, id>` index
//! - `len()` / `iter()` for listing
//!
//! Concrete Snapshot shape (which tables it holds) lives closer to the
//! business types in `models::AisixSnapshot`. This crate provides the
//! primitive only.

use crate::resource::{Resource, ResourceEntry};
use arc_swap::ArcSwap;
use dashmap::DashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Per-kind table with primary id-index and secondary name-index.
///
/// Both indices point at the same `Arc<ResourceEntry<T>>` so there is no
/// duplicate storage — the name map just holds ids.
#[derive(Debug)]
pub struct ResourceTable<T: Resource> {
    by_id: DashMap<String, Arc<ResourceEntry<T>>>,
    by_name: DashMap<String, String>,
    /// Ids of the entries whose name carries a `*`, maintained alongside
    /// `by_name` so it cannot drift from it.
    ///
    /// Wildcard rows are a handful; the table is not. Resolving a name that
    /// no exact entry serves used to walk every row — and materialise a
    /// `Vec` of the whole table to do it — on the model-resolution path AND
    /// on the metric-label path, which every endpoint reaches. Requests
    /// naming a model that resolves to nothing paid the same full scan
    /// before landing on the sentinel.
    wildcards: DashMap<String, Arc<ResourceEntry<T>>>,
    /// Cached entry count, maintained by [`ResourceTable::insert`] /
    /// [`ResourceTable::remove`]. DashMap's own `len()` / `is_empty()`
    /// visit every shard (a CAS pair per shard), so per-request
    /// emptiness checks on the hot path go through this counter
    /// instead — one relaxed load, O(1) regardless of shard count.
    count: AtomicUsize,
}

/// Manual impl: `AtomicUsize` is not `Clone`. The count is re-seeded
/// from the cloned map's length, which the etcd watch supervisor's
/// clone-then-mutate update cycle relies on being exact.
impl<T: Resource> Clone for ResourceTable<T> {
    fn clone(&self) -> Self {
        let by_id = self.by_id.clone();
        let count = AtomicUsize::new(by_id.len());
        Self {
            by_id,
            by_name: self.by_name.clone(),
            wildcards: self.wildcards.clone(),
            count,
        }
    }
}

impl<T: Resource> Default for ResourceTable<T> {
    fn default() -> Self {
        Self {
            by_id: DashMap::new(),
            by_name: DashMap::new(),
            wildcards: DashMap::new(),
            count: AtomicUsize::new(0),
        }
    }
}

impl<T: Resource> ResourceTable<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Insert or replace an entry, updating both indices.
    ///
    /// If an entry with the same id already exists, the old name index entry
    /// is removed first (handles rename on update).
    pub fn insert(&self, entry: ResourceEntry<T>) {
        let id = entry.id.clone();
        let name = entry.value.name().to_string();

        if let Some(old) = self.by_id.get(&id) {
            let old_name = old.value.name().to_string();
            if old_name != name {
                // Only clear the old mapping if it still points at us.
                self.by_name.remove_if(&old_name, |_, v| v == &id);
            }
        }

        self.by_name.insert(name.clone(), id.clone());
        // Provisional increment BEFORE the map insert, corrected after a
        // replace. Orders the count so it can only ever read high during
        // a mutation window, never low: the empty fast paths may take
        // one redundant full scan, but can never skip an entry that is
        // already visible in the map.
        self.count.fetch_add(1, Ordering::Relaxed);
        let entry = Arc::new(entry);
        if name.contains('*') {
            self.wildcards.insert(id.clone(), Arc::clone(&entry));
        } else {
            // A rename off a wildcard name must not leave the old row behind.
            self.wildcards.remove(&id);
        }
        if self.by_id.insert(id, entry).is_some() {
            self.count.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Remove by id; also removes the matching name index entry.
    pub fn remove(&self, id: &str) -> Option<Arc<ResourceEntry<T>>> {
        let (_, entry) = self.by_id.remove(id)?;
        self.wildcards.remove(id);
        self.count.fetch_sub(1, Ordering::Relaxed);
        let name = entry.value.name().to_string();
        self.by_name.remove_if(&name, |_, v| v == id);
        Some(entry)
    }

    pub fn get_by_id(&self, id: &str) -> Option<Arc<ResourceEntry<T>>> {
        self.by_id.get(id).map(|r| r.clone())
    }

    pub fn get_by_name(&self, name: &str) -> Option<Arc<ResourceEntry<T>>> {
        let id = self.by_name.get(name)?.clone();
        self.get_by_id(&id)
    }

    /// True if a different id already owns `name`. Used for duplicate-name
    /// detection on admin create/update (`self_id` = the id being updated,
    /// None for create).
    pub fn name_conflicts(&self, name: &str, self_id: Option<&str>) -> bool {
        match self.by_name.get(name) {
            Some(existing_id) => match self_id {
                Some(me) => existing_id.as_str() != me,
                None => true,
            },
            None => false,
        }
    }

    /// Snapshot of all entries. Callers get owned `Arc` clones, so iteration
    /// does not hold DashMap shards. O(1) when the table is empty — the
    /// per-request callers (exporter fan-out, policy scans) skip the
    /// all-shards walk on unconfigured deployments.
    /// The entries whose name carries a `*`, for callers resolving a name no
    /// exact entry serves. Typically a handful, versus the whole table.
    pub fn wildcard_entries(&self) -> Vec<Arc<ResourceEntry<T>>> {
        if self.wildcards.is_empty() {
            return Vec::new();
        }
        self.wildcards.iter().map(|kv| kv.value().clone()).collect()
    }

    pub fn entries(&self) -> Vec<Arc<ResourceEntry<T>>> {
        if self.is_empty() {
            return Vec::new();
        }
        self.by_id.iter().map(|kv| kv.value().clone()).collect()
    }

    /// True when any entry satisfies `pred`, without materialising the
    /// table into a `Vec`. Cheaper than `entries().iter().any(...)` on the
    /// hot path (no allocation, no per-row `Arc` clone). A DashMap shard
    /// guard is held during the scan, so `pred` must not call back into
    /// this table.
    pub fn any(&self, pred: impl Fn(&ResourceEntry<T>) -> bool) -> bool {
        !self.is_empty() && self.by_id.iter().any(|kv| pred(kv.value()))
    }

    /// The single entry satisfying `pred`, without materialising the
    /// table. Returns `(None, true)` when more than one entry matches so
    /// the caller can fail closed on an ambiguous lookup and log the
    /// misconfiguration — a security-sensitive resolver must never pick
    /// one of several matches silently. `(Some(_), false)` on exactly
    /// one match; `(None, false)` on none.
    pub fn find_unique_by(
        &self,
        pred: impl Fn(&ResourceEntry<T>) -> bool,
    ) -> (Option<Arc<ResourceEntry<T>>>, bool) {
        if self.is_empty() {
            return (None, false);
        }
        let mut found: Option<Arc<ResourceEntry<T>>> = None;
        for kv in self.by_id.iter() {
            if !pred(kv.value()) {
                continue;
            }
            if found.is_some() {
                return (None, true);
            }
            found = Some(kv.value().clone());
        }
        (found, false)
    }
}

/// Handle consumers clone to reach the current snapshot.
///
/// `SnapshotHandle<S>` is the type actually stored in axum state — consumers
/// call [`SnapshotHandle::load`] on every request to get the current `Arc<S>`
/// without any locking.
///
/// The manual `Clone` impl deliberately does *not* require `S: Clone` — the
/// handle only clones its inner `Arc`, the `S` is never duplicated.
#[derive(Debug)]
pub struct SnapshotHandle<S> {
    inner: Arc<ArcSwap<PublishedSnapshot<S>>>,
    listeners: Arc<SnapshotListeners<S>>,
}

type SnapshotListener<S> = dyn Fn(SnapshotView<S>) + Send + Sync + 'static;

struct SnapshotListeners<S> {
    /// Serializes publication with initial listener delivery so a newly
    /// subscribed observer can never see generation N+1 before generation N.
    publication: Mutex<()>,
    before_publish_callbacks: Mutex<Vec<Arc<SnapshotListener<S>>>>,
    callbacks: Mutex<Vec<Arc<SnapshotListener<S>>>>,
}

impl<S> std::fmt::Debug for SnapshotListeners<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotListeners")
            .field(
                "before_publish_count",
                &self.before_publish_callbacks.lock().unwrap().len(),
            )
            .field("count", &self.callbacks.lock().unwrap().len())
            .finish()
    }
}

#[derive(Debug)]
struct PublishedSnapshot<S> {
    version: u64,
    snapshot: Arc<S>,
}

/// One atomically-observed snapshot generation.
///
/// Consumers that cache state derived from a snapshot must use this view so
/// the generation and the data it names cannot come from different stores.
#[derive(Debug)]
pub struct SnapshotView<S> {
    pub version: u64,
    pub snapshot: Arc<S>,
}

impl<S> Clone for SnapshotView<S> {
    fn clone(&self) -> Self {
        Self {
            version: self.version,
            snapshot: Arc::clone(&self.snapshot),
        }
    }
}

impl<S> Clone for SnapshotHandle<S> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            listeners: Arc::clone(&self.listeners),
        }
    }
}

impl<S> SnapshotHandle<S> {
    pub fn new(initial: S) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(PublishedSnapshot {
                version: 0,
                snapshot: Arc::new(initial),
            })),
            listeners: Arc::new(SnapshotListeners {
                publication: Mutex::new(()),
                before_publish_callbacks: Mutex::new(Vec::new()),
                callbacks: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Atomic load. Cheap (one Acquire load).
    pub fn load(&self) -> Arc<S> {
        Arc::clone(&self.inner.load().snapshot)
    }

    /// Load the snapshot and its generation from one atomic publication.
    pub fn load_versioned(&self) -> SnapshotView<S> {
        let published = self.inner.load();
        SnapshotView {
            version: published.version,
            snapshot: Arc::clone(&published.snapshot),
        }
    }

    /// Monotonic version counter. Incremented on every `store` / `rcu`.
    /// Consumers that also need the corresponding snapshot must use
    /// [`Self::load_versioned`] instead of a separate `version` + `load` pair.
    pub fn version(&self) -> u64 {
        self.inner.load().version
    }

    /// Register a lightweight synchronous observer for snapshot publication.
    /// The callback receives the current view immediately, then every newer
    /// publication. It runs after the atomic swap and must not block or call
    /// `store`/`rcu` recursively. This is for revocation-sensitive derived
    /// state that cannot wait for request traffic to notice a new snapshot.
    pub fn subscribe(&self, listener: impl Fn(SnapshotView<S>) + Send + Sync + 'static) {
        let listener: Arc<SnapshotListener<S>> = Arc::new(listener);
        let _publication = self.listeners.publication.lock().unwrap();
        self.listeners
            .callbacks
            .lock()
            .unwrap()
            .push(Arc::clone(&listener));
        listener(self.load_versioned());
    }

    /// Register a synchronous publication barrier. The callback receives the
    /// current view immediately; for later stores it receives the next view
    /// before that generation becomes visible to [`Self::load_versioned`].
    /// It must not block or call `store`/`rcu` recursively.
    ///
    /// Use this only for revocation-sensitive derived state: once generation
    /// N is observable, the callback has already revoked anything removed by
    /// N. Ordinary observers should use [`Self::subscribe`].
    pub fn subscribe_before_publish(
        &self,
        listener: impl Fn(SnapshotView<S>) + Send + Sync + 'static,
    ) {
        let listener: Arc<SnapshotListener<S>> = Arc::new(listener);
        let _publication = self.listeners.publication.lock().unwrap();
        self.listeners
            .before_publish_callbacks
            .lock()
            .unwrap()
            .push(Arc::clone(&listener));
        listener(self.load_versioned());
    }

    fn notify(listeners: &Mutex<Vec<Arc<SnapshotListener<S>>>>, view: SnapshotView<S>) {
        let listeners = listeners.lock().unwrap().clone();
        for listener in listeners {
            listener(view.clone());
        }
    }

    fn publish(&self, snapshot: Arc<S>) {
        let current = self.inner.load_full();
        let published = Arc::new(PublishedSnapshot {
            version: current.version.wrapping_add(1),
            snapshot,
        });
        let view = SnapshotView {
            version: published.version,
            snapshot: Arc::clone(&published.snapshot),
        };
        Self::notify(&self.listeners.before_publish_callbacks, view.clone());
        self.inner.store(published);
        Self::notify(&self.listeners.callbacks, view);
    }

    /// Atomic store. Called by the etcd watch supervisor after building a
    /// fresh snapshot.
    pub fn store(&self, new: S) {
        let _publication = self.listeners.publication.lock().unwrap();
        self.publish(Arc::new(new));
    }

    /// Read-copy-update. Publication is serialized with `store`, so `f` runs
    /// once against the latest snapshot and its result is published without a
    /// lost-update window.
    pub fn rcu<F>(&self, mut f: F)
    where
        F: FnMut(&S) -> S,
    {
        let _publication = self.listeners.publication.lock().unwrap();
        let current = self.inner.load_full();
        self.publish(Arc::new(f(current.snapshot.as_ref())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Item {
        id: String,
        name: String,
    }

    impl Resource for Item {
        fn id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            &self.name
        }
        fn kind() -> &'static str {
            "items"
        }
    }

    fn entry(id: &str, name: &str) -> ResourceEntry<Item> {
        ResourceEntry::new(
            id,
            Item {
                id: id.into(),
                name: name.into(),
            },
            1,
        )
    }

    /// The wildcard index is a second index over the same rows, so the only
    /// way it can be wrong is by drifting from `by_name`. Every mutation the
    /// table supports has to keep the two agreeing.
    #[test]
    fn wildcard_index_tracks_insert_rename_replace_and_remove() {
        fn names(t: &ResourceTable<Item>) -> Vec<String> {
            let mut n: Vec<String> = t
                .wildcard_entries()
                .iter()
                .map(|e| e.value.name.clone())
                .collect();
            n.sort();
            n
        }

        let t = ResourceTable::<Item>::new();
        t.insert(entry("a-1", "openai/*"));
        t.insert(entry("b-2", "gpt-4o"));
        t.insert(entry("c-3", "anthropic/*"));
        assert_eq!(names(&t), vec!["anthropic/*", "openai/*"]);

        // Rename ONTO a wildcard name: the row joins the index.
        t.insert(entry("b-2", "azure/*"));
        assert_eq!(names(&t), vec!["anthropic/*", "azure/*", "openai/*"]);

        // Rename OFF a wildcard name: it leaves, rather than lingering with
        // a stale name that would serve requests it no longer matches.
        t.insert(entry("b-2", "gpt-4o"));
        assert_eq!(names(&t), vec!["anthropic/*", "openai/*"]);

        // Replace in place keeps one entry, not two.
        t.insert(entry("a-1", "openai/*"));
        assert_eq!(names(&t), vec!["anthropic/*", "openai/*"]);
        assert_eq!(t.len(), 3, "replacing must not inflate the count");

        t.remove("a-1");
        assert_eq!(names(&t), vec!["anthropic/*"]);
        t.remove("c-3");
        assert!(t.wildcard_entries().is_empty());
    }

    /// A clone is what the etcd supervisor publishes; the derived index has
    /// to come with it or the first request after a config change rebuilds
    /// nothing and resolves no wildcard.
    #[test]
    fn wildcard_index_survives_a_table_clone() {
        let t = ResourceTable::<Item>::new();
        t.insert(entry("a-1", "openai/*"));
        let cloned = t.clone();
        assert_eq!(cloned.wildcard_entries().len(), 1);
        cloned.remove("a-1");
        assert_eq!(
            t.wildcard_entries().len(),
            1,
            "the clone must not share the original's index"
        );
    }

    #[test]
    fn insert_lookup_by_id_and_name() {
        let t = ResourceTable::<Item>::new();
        t.insert(entry("a-1", "alpha"));
        t.insert(entry("b-2", "beta"));

        assert_eq!(t.len(), 2);
        assert_eq!(t.get_by_id("a-1").unwrap().name(), "alpha");
        assert_eq!(t.get_by_name("beta").unwrap().id(), "b-2");
        assert!(t.get_by_name("missing").is_none());
    }

    #[test]
    fn rename_on_update_cleans_old_name_index() {
        let t = ResourceTable::<Item>::new();
        t.insert(entry("a-1", "alpha"));

        // Rename a-1 from alpha → aleph.
        t.insert(entry("a-1", "aleph"));

        assert_eq!(t.len(), 1);
        assert!(t.get_by_name("alpha").is_none());
        assert_eq!(t.get_by_name("aleph").unwrap().id(), "a-1");
    }

    #[test]
    fn duplicate_name_creates_conflict() {
        let t = ResourceTable::<Item>::new();
        t.insert(entry("a-1", "alpha"));
        assert!(t.name_conflicts("alpha", None));
        assert!(!t.name_conflicts("alpha", Some("a-1"))); // updating self is fine
        assert!(t.name_conflicts("alpha", Some("other")));
    }

    #[test]
    fn remove_clears_both_indices() {
        let t = ResourceTable::<Item>::new();
        t.insert(entry("a-1", "alpha"));
        assert!(t.remove("a-1").is_some());
        assert!(t.get_by_id("a-1").is_none());
        assert!(t.get_by_name("alpha").is_none());
    }

    /// The cached count must stay exact through every mutation shape:
    /// fresh insert, same-id replace, remove, remove-miss, and clone.
    #[test]
    fn cached_count_tracks_all_mutations() {
        let t = ResourceTable::<Item>::new();
        assert_eq!(t.len(), 0);
        assert!(t.is_empty());

        t.insert(entry("a-1", "alpha"));
        t.insert(entry("b-2", "beta"));
        assert_eq!(t.len(), 2);
        assert!(!t.is_empty());

        // Same-id replace (update, incl. rename) must not double-count.
        t.insert(entry("a-1", "aleph"));
        assert_eq!(t.len(), 2);

        // Remove-miss must not decrement.
        assert!(t.remove("missing").is_none());
        assert_eq!(t.len(), 2);

        assert!(t.remove("a-1").is_some());
        assert_eq!(t.len(), 1);

        // Clone re-seeds the counter from the cloned map.
        let c = t.clone();
        assert_eq!(c.len(), 1);
        c.insert(entry("c-3", "gamma"));
        assert_eq!(c.len(), 2);
        assert_eq!(t.len(), 1); // original untouched

        assert!(t.remove("b-2").is_some());
        assert_eq!(t.len(), 0);
        assert!(t.is_empty());
    }

    #[test]
    fn snapshot_handle_atomic_swap() {
        let handle: SnapshotHandle<u64> = SnapshotHandle::new(0);
        assert_eq!(*handle.load(), 0);
        assert_eq!(handle.version(), 0);
        handle.store(42);
        assert_eq!(*handle.load(), 42);
        assert_eq!(handle.version(), 1);
    }

    #[test]
    fn version_increments_on_rcu() {
        let handle: SnapshotHandle<u64> = SnapshotHandle::new(0);
        assert_eq!(handle.version(), 0);
        handle.rcu(|v| v + 1);
        assert_eq!(handle.version(), 1);
        handle.rcu(|v| v + 1);
        assert_eq!(handle.version(), 2);
        assert_eq!(*handle.load(), 2);
    }

    #[test]
    fn publication_listener_observes_initial_store_and_rcu_views() {
        let handle: SnapshotHandle<u64> = SnapshotHandle::new(3);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_listener = Arc::clone(&observed);
        handle.subscribe(move |view| {
            observed_for_listener
                .lock()
                .unwrap()
                .push((view.version, *view.snapshot));
        });

        handle.store(5);
        handle.rcu(|value| value + 2);

        assert_eq!(&*observed.lock().unwrap(), &[(0, 3), (1, 5), (2, 7)]);
    }

    #[test]
    fn subscription_delivers_initial_view_before_concurrent_publication() {
        use std::sync::mpsc;
        use std::time::Duration;

        let handle: SnapshotHandle<u64> = SnapshotHandle::new(0);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let (initial_entered_tx, initial_entered_rx) = mpsc::channel();
        let (release_initial_tx, release_initial_rx) = mpsc::channel();
        let release_initial_rx = Arc::new(Mutex::new(release_initial_rx));

        std::thread::scope(|scope| {
            let subscriber = handle.clone();
            let listener_observed = Arc::clone(&observed);
            let listener_release = Arc::clone(&release_initial_rx);
            scope.spawn(move || {
                subscriber.subscribe(move |view| {
                    listener_observed
                        .lock()
                        .unwrap()
                        .push((view.version, *view.snapshot));
                    if view.version == 0 {
                        initial_entered_tx.send(()).unwrap();
                        listener_release.lock().unwrap().recv().unwrap();
                    }
                });
            });

            initial_entered_rx.recv().unwrap();
            let publisher = handle.clone();
            let (store_done_tx, store_done_rx) = mpsc::channel();
            scope.spawn(move || {
                publisher.store(1);
                store_done_tx.send(()).unwrap();
            });

            assert!(
                store_done_rx
                    .recv_timeout(Duration::from_millis(50))
                    .is_err(),
                "publication must wait for the subscriber's initial delivery",
            );
            release_initial_tx.send(()).unwrap();
            store_done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        });

        assert_eq!(&*observed.lock().unwrap(), &[(0, 0), (1, 1)]);
    }

    #[test]
    fn before_publish_listener_finishes_before_new_generation_is_visible() {
        use std::sync::mpsc;
        use std::time::Duration;

        let handle: SnapshotHandle<u64> = SnapshotHandle::new(0);
        let (next_entered_tx, next_entered_rx) = mpsc::channel();
        let (release_next_tx, release_next_rx) = mpsc::channel();
        let release_next_rx = Arc::new(Mutex::new(release_next_rx));
        let listener_release = Arc::clone(&release_next_rx);
        handle.subscribe_before_publish(move |view| {
            if view.version == 1 {
                next_entered_tx.send(()).unwrap();
                listener_release.lock().unwrap().recv().unwrap();
            }
        });

        std::thread::scope(|scope| {
            let publisher = handle.clone();
            let (store_done_tx, store_done_rx) = mpsc::channel();
            scope.spawn(move || {
                publisher.store(1);
                store_done_tx.send(()).unwrap();
            });

            next_entered_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap();
            let still_current = handle.load_versioned();
            assert_eq!(still_current.version, 0);
            assert_eq!(*still_current.snapshot, 0);
            assert!(
                store_done_rx
                    .recv_timeout(Duration::from_millis(50))
                    .is_err(),
                "store must not return before its publication barrier",
            );

            release_next_tx.send(()).unwrap();
            store_done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        });

        let published = handle.load_versioned();
        assert_eq!(published.version, 1);
        assert_eq!(*published.snapshot, 1);
    }

    #[test]
    fn handle_is_clone_and_share_the_same_cell() {
        let a: SnapshotHandle<u64> = SnapshotHandle::new(1);
        let b = a.clone();
        a.store(99);
        // b sees a's write — same underlying ArcSwap.
        assert_eq!(*b.load(), 99);
    }

    #[test]
    fn versioned_load_keeps_generation_and_snapshot_coherent() {
        const READERS: usize = 4;
        const UPDATES: u64 = 10_000;
        const MIDPOINT: u64 = UPDATES / 2;
        let handle = SnapshotHandle::new(0_u64);
        let start = Arc::new(std::sync::Barrier::new(READERS + 1));
        let midpoint = Arc::new(std::sync::Barrier::new(READERS + 1));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observations = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|scope| {
            let writer = handle.clone();
            let writer_start = Arc::clone(&start);
            let writer_midpoint = Arc::clone(&midpoint);
            let writer_done = Arc::clone(&done);
            scope.spawn(move || {
                writer_start.wait();
                for value in 1..=MIDPOINT {
                    writer.store(value);
                }
                // Hold the writer until every reader has observed at least one
                // publication from the first half. The old uncoordinated loop
                // could finish before a reader was ever scheduled.
                writer_midpoint.wait();
                for value in (MIDPOINT + 1)..=UPDATES {
                    writer.store(value);
                }
                writer_done.store(true, Ordering::Release);
            });

            for _ in 0..READERS {
                let reader = handle.clone();
                let reader_start = Arc::clone(&start);
                let reader_midpoint = Arc::clone(&midpoint);
                let reader_done = Arc::clone(&done);
                let reader_observations = Arc::clone(&observations);
                scope.spawn(move || {
                    reader_start.wait();
                    loop {
                        let view = reader.load_versioned();
                        assert_eq!(*view.snapshot, view.version);
                        reader_observations.fetch_add(1, Ordering::Relaxed);
                        if view.version >= MIDPOINT {
                            break;
                        }
                        std::thread::yield_now();
                    }
                    reader_midpoint.wait();

                    while !reader_done.load(Ordering::Acquire) {
                        let view = reader.load_versioned();
                        assert_eq!(*view.snapshot, view.version);
                        reader_observations.fetch_add(1, Ordering::Relaxed);
                        std::thread::yield_now();
                    }
                });
            }
        });

        assert!(
            observations.load(Ordering::Relaxed) >= READERS,
            "every reader must make an in-flight observation"
        );
        let view = handle.load_versioned();
        assert_eq!(*view.snapshot, UPDATES);
        assert_eq!(view.version, UPDATES);
    }
}
