//! On-disk snapshot cache for offline resilience.
//!
//! Goal (prd-09 §9.7.2): the DP keeps serving `/v1/chat/completions`
//! from the last known etcd contents when the control plane is
//! unreachable — including across full container restarts.
//!
//! Approach:
//!
//! - The supervisor calls [`SnapshotCache::store`] after each
//!   successful apply (resync, put, delete). The on-disk file is the
//!   serialised list of [`RawEntry`] that produced the current
//!   in-memory snapshot, plus the etcd revision they came from.
//! - At boot, the supervisor calls [`SnapshotCache::load`] before
//!   touching etcd. If the file exists and parses, the entries are
//!   handed to `apply_resync` and the proxy starts serving traffic
//!   immediately. The first successful etcd `load_all` then
//!   overwrites the cache with fresh state.
//! - If etcd never comes back, the cached snapshot keeps serving
//!   forever — the DP is degraded (no new models / keys appear) but
//!   not down.
//!
//! Atomicity: `store` writes to `<path>.tmp` first, fsyncs, then
//! renames over the destination. A torn write never corrupts the
//! committed file. Disabled when [`SnapshotCache::disabled`] is used
//! (the default unless `managed.snapshot_cache_enabled` is true).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;

use crate::provider::RawEntry;
use crate::supervisor::StaleServing;

/// File-format version. Bumped whenever the wire shape of [`CachedFile`]
/// changes incompatibly so old DPs ignore future caches instead of
/// crashing on a stale-format upgrade. The `stale` section added for
/// #871 is additive (defaulted on read, ignored by older DPs), so it
/// stays at 1.
const FORMAT_VERSION: u32 = 1;

/// Owned, sync-or-async-safe snapshot cache. Cheap to clone — internally
/// holds an `Arc<Inner>` that serialises writes through a `Mutex`.
#[derive(Clone)]
pub struct SnapshotCache {
    inner: Arc<Inner>,
}

struct Inner {
    /// Some(path) → enabled; None → no-op.
    path: Option<PathBuf>,
    /// Hands out [`WriteTicket`]s in the order snapshots are captured.
    next_ticket: AtomicU64,
    /// Serialise concurrent writes so the tmp-file rename dance can't
    /// race with itself, and carry the ticket of the last committed write
    /// so they also commit in capture order. Reads are unguarded — they
    /// go through OS caches and the rename is atomic.
    ///
    /// Applies spawn independent tasks, so a later capture may acquire
    /// the lock first. Never let a late earlier capture replace it — not
    /// even at the same revision: a delete flushes at the current
    /// revision floor, so two captures can share a revision and differ
    /// in content. Zero until the first write; tickets start at one.
    write_lock: Mutex<u64>,
    #[cfg(test)]
    write_count: std::sync::atomic::AtomicUsize,
}

/// The position of one captured snapshot in apply order. Taken where the
/// snapshot is captured, not where it is written, because the write runs
/// on a task of its own and may reach the lock out of order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WriteTicket(u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedFile {
    version: u32,
    revision: i64,
    entries: Vec<CachedEntry>,
    /// Last-known-good values pinned for keys whose current bytes (in
    /// `entries`) are rejected (#871). Restored before the boot resync so
    /// stale-serving rows survive a restart. Defaulted so pre-#871 cache
    /// files keep loading.
    #[serde(default)]
    stale: Vec<CachedStaleEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedEntry {
    key: String,
    /// Base64 because etcd values are byte arrays — typically JSON, but
    /// the cache must round-trip whatever the supervisor saw.
    value_b64: String,
    revision: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedStaleEntry {
    key: String,
    /// Base64 of the pinned last-known-good value bytes.
    value_b64: String,
    /// Revision the pinned value was accepted at.
    revision: i64,
    /// Unix seconds when stale serving began — persisted so the reported
    /// staleness age stays continuous across restarts.
    since_unix_secs: u64,
}

/// The parsed contents of a cache file: the raw entry set, the revision
/// it reflects, and the pinned last-known-good values (#871).
#[derive(Debug, Clone)]
pub struct CachedSnapshot {
    pub entries: Vec<RawEntry>,
    pub revision: i64,
    pub stale: Vec<StaleServing>,
}

impl SnapshotCache {
    /// Construct a cache backed by `path`. The file is created on the
    /// first successful [`Self::store`]; missing path on [`Self::load`]
    /// is treated as "no cache yet".
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                path: Some(path.into()),
                next_ticket: AtomicU64::new(0),
                write_lock: Mutex::new(0),
                #[cfg(test)]
                write_count: std::sync::atomic::AtomicUsize::new(0),
            }),
        }
    }

    /// No-op cache. Returned when persistence is disabled (e.g. when
    /// `managed.snapshot_cache_path` is empty). [`Self::load`] always
    /// returns `None` and [`Self::store`] is a quiet success.
    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(Inner {
                path: None,
                next_ticket: AtomicU64::new(0),
                write_lock: Mutex::new(0),
                #[cfg(test)]
                write_count: std::sync::atomic::AtomicUsize::new(0),
            }),
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.inner.path.is_some()
    }

    /// Read the cached snapshot, or `None` if no usable file exists.
    /// "Usable" means: the file is present, parses as JSON, declares a
    /// recognised [`FORMAT_VERSION`], and every entry's value decodes.
    /// Anything else is logged and treated as cache-miss so a corrupt
    /// file can never wedge the DP.
    pub fn load(&self) -> Option<CachedSnapshot> {
        let path = self.inner.path.as_ref()?;
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "snapshot cache read failed");
                return None;
            }
        };
        let cached: CachedFile = match serde_json::from_slice(&bytes) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "snapshot cache parse failed; ignoring");
                return None;
            }
        };
        if cached.version != FORMAT_VERSION {
            tracing::warn!(
                got = cached.version,
                want = FORMAT_VERSION,
                "snapshot cache format mismatch; ignoring",
            );
            return None;
        }
        let entries: Result<Vec<_>, _> = cached
            .entries
            .into_iter()
            .map(|e| {
                B64.decode(&e.value_b64).map(|value| RawEntry {
                    key: e.key,
                    value,
                    revision: e.revision,
                })
            })
            .collect();
        let stale: Result<Vec<_>, _> = cached
            .stale
            .into_iter()
            .map(|s| {
                B64.decode(&s.value_b64).map(|value| {
                    StaleServing::new(
                        RawEntry {
                            key: s.key,
                            value,
                            revision: s.revision,
                        },
                        s.since_unix_secs,
                    )
                })
            })
            .collect();
        match (entries, stale) {
            (Ok(entries), Ok(stale)) => Some(CachedSnapshot {
                entries,
                revision: cached.revision,
                stale,
            }),
            (Err(e), _) | (_, Err(e)) => {
                tracing::warn!(error = %e, "snapshot cache entry decode failed; ignoring");
                None
            }
        }
    }

    /// Write the given entries + revision + pinned last-known-good values
    /// atomically. Errors are logged-and-swallowed because losing the
    /// cache is not worth blowing up an otherwise-healthy DP — at worst
    /// the next restart rebuilds from etcd.
    pub async fn store(&self, entries: &[RawEntry], revision: i64, stale: &[StaleServing]) {
        if !self.is_enabled() {
            return;
        }
        let ticket = self.ticket();
        let entries: Vec<_> = entries.iter().map(encode_entry).collect();
        let stale: Vec<_> = stale.iter().map(encode_stale).collect();
        self.store_encoded(ticket, &entries, revision, &stale).await;
    }

    /// Reserve the next position in apply order. Take it while the
    /// snapshot being written is captured.
    pub(crate) fn ticket(&self) -> WriteTicket {
        WriteTicket(self.inner.next_ticket.fetch_add(1, Ordering::Relaxed) + 1)
    }

    pub(crate) async fn store_encoded(
        &self,
        ticket: WriteTicket,
        entries: &[Arc<[u8]>],
        revision: i64,
        stale: &[Arc<[u8]>],
    ) {
        let Some(path) = self.inner.path.as_ref() else {
            return;
        };
        let mut committed = self.inner.write_lock.lock().await;
        // A write that lost the serialisation race to a LATER capture has
        // nothing to add: committing it would roll the cache back to a
        // snapshot the gateway has already moved past, and the next
        // restart would then serve it.
        if ticket.0 <= *committed {
            return;
        }
        // A later capture already holds a ticket and its write is on the way,
        // so this one would be overwritten at once. A burst of applies costs
        // one full-snapshot write and one fsync rather than one per apply.
        if self.inner.next_ticket.load(Ordering::Relaxed) > ticket.0 {
            return;
        }
        #[cfg(test)]
        self.inner
            .write_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Err(e) = atomic_write(path, entries, revision, stale).await {
            tracing::warn!(error = %e, path = %path.display(), "snapshot cache write failed");
            return;
        }
        *committed = ticket.0;
    }

    #[cfg(test)]
    pub(crate) fn write_count(&self) -> usize {
        self.inner
            .write_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub(crate) fn encode_entry(entry: &RawEntry) -> Arc<[u8]> {
    serde_json::to_vec(&CachedEntry {
        key: entry.key.clone(),
        value_b64: B64.encode(&entry.value),
        revision: entry.revision,
    })
    .expect("snapshot entries contain only strings and integers")
    .into()
}

pub(crate) fn encode_stale(stale: &StaleServing) -> Arc<[u8]> {
    serde_json::to_vec(&CachedStaleEntry {
        key: stale.entry.key.clone(),
        value_b64: B64.encode(&stale.entry.value),
        revision: stale.entry.revision,
        since_unix_secs: stale.since_unix_secs,
    })
    .expect("stale entries contain only strings and integers")
    .into()
}

/// Write the snapshot to `path` atomically: write to a sibling tmp file,
/// fsync, rename. Survives crashes between any two of those steps. Preserves
/// the v1 JSON envelope while writing shared records through a bounded
/// buffer, so no full-snapshot byte buffer is assembled per apply.
///
/// Created owner-only. This file holds every document under the watched
/// prefix, verbatim — `provider_keys` included, whose upstream credential
/// sits in there in the clear (the entries are base64-encoded, which is
/// transport encoding, not protection). At the usual umask of `0022`
/// `File::create` would land it at `0644`, readable by every local account
/// and every sidecar sharing the filesystem, which is a strictly wider
/// audience than the process that holds those secrets in memory.
///
/// The mode goes on the TMP file, because `rename` carries the source
/// inode — so the published file inherits `0600` rather than whatever a
/// previous build left at the destination. The directory mode applies only
/// to components this call creates; an operator-provided directory keeps
/// the permissions the operator chose.
async fn atomic_write(
    path: &Path,
    entries: &[Arc<[u8]>],
    revision: i64,
    stale: &[Arc<[u8]>],
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let mut b = std::fs::DirBuilder::new();
                b.recursive(true).mode(0o700);
                b.create(parent)?;
            }
            #[cfg(not(unix))]
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    let tmp = path.with_extension("tmp");
    {
        let mut opts = tokio::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        // tokio 的 `OpenOptions` 在 unix 上自带 `mode()`，不需要 `OpenOptionsExt`
        // （`aisix-portal/src/resources.rs` 里也记着这一条）。
        #[cfg(unix)]
        opts.mode(0o600);
        let f = opts.open(&tmp).await?;
        // Re-assert: `mode()` applies only to a file this call CREATES, and a
        // crash between the create and the rename leaves a `.tmp` behind. The
        // next write reuses it, keeps whatever mode it had, and the rename
        // then publishes that mode. Same reason `export::write_owner_only`
        // re-asserts.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))
                .await?;
        }
        let mut writer = BufWriter::with_capacity(64 * 1024, f);
        writer
            .write_all(
                format!(r#"{{"version":{FORMAT_VERSION},"revision":{revision},"entries":["#)
                    .as_bytes(),
            )
            .await?;
        write_records(&mut writer, entries).await?;
        writer.write_all(b"],\"stale\":[").await?;
        write_records(&mut writer, stale).await?;
        writer.write_all(b"]}").await?;
        writer.flush().await?;
        writer.get_ref().sync_all().await?;
    }
    tokio::fs::rename(&tmp, path).await
}

async fn write_records(
    writer: &mut BufWriter<tokio::fs::File>,
    records: &[Arc<[u8]>],
) -> std::io::Result<()> {
    for (index, record) in records.iter().enumerate() {
        if index > 0 {
            writer.write_all(b",").await?;
        }
        writer.write_all(record).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn entry(key: &str, value: &[u8], rev: i64) -> RawEntry {
        RawEntry {
            key: key.into(),
            value: value.to_vec(),
            revision: rev,
        }
    }

    #[tokio::test]
    async fn round_trips_entries() {
        let dir = tempdir().unwrap();
        let cache = SnapshotCache::new(dir.path().join("snap.json"));
        let entries = vec![
            entry("/aisix/models/m-1", br#"{"name":"m1"}"#, 7),
            entry("/aisix/api_keys/k-1", b"\xff\x00\x01raw", 8),
        ];
        cache.store(&entries, 42, &[]).await;

        let cached = cache.load().expect("cache file exists");
        assert_eq!(cached.revision, 42);
        assert_eq!(cached.entries, entries);
        assert!(cached.stale.is_empty());
    }

    #[tokio::test]
    async fn a_write_that_lost_its_race_cannot_roll_the_cache_back() {
        // The supervisor captures a snapshot per apply and spawns its
        // write, so the write for an earlier apply can reach the lock
        // after a later one. Committing it would roll the cache back to a
        // snapshot the gateway has already moved past, and the next
        // restart would serve it.
        let dir = tempdir().unwrap();
        let cache = SnapshotCache::new(dir.path().join("snap.json"));

        let older = [encode_entry(&entry(
            "/aisix/models/m-1",
            br#"{"name":"old"}"#,
            8,
        ))];
        let newer_entry = entry("/aisix/models/m-1", br#"{"name":"new"}"#, 9);
        let older_ticket = cache.ticket();
        let newer_ticket = cache.ticket();
        cache
            .store_encoded(newer_ticket, &[encode_entry(&newer_entry)], 9, &[])
            .await;
        cache.store_encoded(older_ticket, &older, 8, &[]).await;

        let cached = cache.load().expect("cache file exists");
        assert_eq!(
            cached.revision, 9,
            "the older apply must not overwrite the newer one"
        );
        assert_eq!(cached.entries, vec![newer_entry]);
    }

    #[tokio::test]
    async fn a_late_write_at_the_same_revision_cannot_restore_a_deleted_row() {
        // A delete flushes at the current revision floor, so the capture
        // before it and the capture after it share a revision. If the
        // earlier one lands last, the deleted row comes back on restart.
        let dir = tempdir().unwrap();
        let cache = SnapshotCache::new(dir.path().join("snap.json"));

        let before_delete = [encode_entry(&entry(
            "/aisix/models/m-1",
            br#"{"name":"m1"}"#,
            9,
        ))];
        let before_ticket = cache.ticket();
        let after_ticket = cache.ticket();
        cache.store_encoded(after_ticket, &[], 9, &[]).await;
        cache
            .store_encoded(before_ticket, &before_delete, 9, &[])
            .await;

        let cached = cache.load().expect("cache file exists");
        assert!(
            cached.entries.is_empty(),
            "the deleted row must stay deleted: {:?}",
            cached.entries,
        );
    }

    #[tokio::test]
    async fn a_write_at_the_same_revision_still_commits() {
        // Same-revision captures carry different content, so a later one
        // must still replace the earlier.
        let dir = tempdir().unwrap();
        let path = dir.path().join("snap.json");
        let cache = SnapshotCache::new(&path);

        let before = vec![entry("/aisix/models/m-1", br#"{"name":"m1"}"#, 9)];
        cache.store(&before, 9, &[]).await;
        cache.store(&[], 9, &[]).await;

        let cached = cache.load().expect("cache file exists");
        assert!(
            cached.entries.is_empty(),
            "a same-revision write must still commit",
        );
    }

    #[tokio::test]
    async fn round_trips_stale_entries() {
        let dir = tempdir().unwrap();
        let cache = SnapshotCache::new(dir.path().join("snap.json"));
        let entries = vec![entry("/aisix/models/m-1", br#"{"bad":true}"#, 9)];
        let stale = vec![StaleServing::new(
            entry("/aisix/models/m-1", br#"{"name":"last-good"}"#, 7),
            1_770_000_000,
        )];
        cache.store(&entries, 9, &stale).await;

        let cached = cache.load().expect("cache file exists");
        assert_eq!(cached.stale, stale);
    }

    /// A pre-#871 cache file (no `stale` section) must keep loading.
    #[tokio::test]
    async fn legacy_file_without_stale_section_loads() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("snap.json");
        let legacy = serde_json::json!({
            "version": 1,
            "revision": 5,
            "entries": [
                {"key": "/aisix/models/m-1", "value_b64": B64.encode(br#"{"a":1}"#), "revision": 5}
            ],
        });
        tokio::fs::write(&path, serde_json::to_vec(&legacy).unwrap())
            .await
            .unwrap();
        let cached = SnapshotCache::new(&path).load().expect("legacy file loads");
        assert_eq!(cached.entries.len(), 1);
        assert!(cached.stale.is_empty());
    }

    #[tokio::test]
    async fn missing_file_returns_none() {
        let dir = tempdir().unwrap();
        let cache = SnapshotCache::new(dir.path().join("never-written.json"));
        assert!(cache.load().is_none());
    }

    #[tokio::test]
    async fn disabled_cache_is_a_noop() {
        let cache = SnapshotCache::disabled();
        cache.store(&[entry("/a", b"x", 1)], 1, &[]).await;
        assert!(cache.load().is_none());
    }

    #[tokio::test]
    async fn corrupt_file_is_treated_as_cache_miss() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("snap.json");
        tokio::fs::write(&path, b"this is not json").await.unwrap();
        let cache = SnapshotCache::new(&path);
        assert!(cache.load().is_none());
    }

    #[tokio::test]
    async fn unknown_format_version_is_ignored() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("snap.json");
        let bogus = serde_json::json!({
            "version": 99,
            "revision": 1,
            "entries": [],
        });
        tokio::fs::write(&path, serde_json::to_vec(&bogus).unwrap())
            .await
            .unwrap();
        let cache = SnapshotCache::new(&path);
        assert!(cache.load().is_none());
    }

    #[tokio::test]
    async fn store_overwrites_atomically() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("snap.json");
        let cache = SnapshotCache::new(&path);
        cache.store(&[entry("/a", b"v1", 1)], 1, &[]).await;
        cache.store(&[entry("/a", b"v2", 2)], 2, &[]).await;
        let cached = cache.load().unwrap();
        assert_eq!(cached.revision, 2);
        assert_eq!(cached.entries[0].value, b"v2".to_vec());
    }

    /// The cache holds every watched document verbatim — `provider_keys`
    /// among them, whose upstream credential is in there in the clear. At the
    /// usual umask of `0022` the previous `File::create` produced `0644`:
    /// readable by every local account and every sidecar on the same
    /// filesystem, a strictly wider audience than the one process that is
    /// supposed to hold those secrets.
    ///
    /// Asserted on the mode rather than on the content, because the content
    /// is whatever etcd held and the exposure is the mode.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_cache_file_and_its_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        // A nested path so the directory is one this call creates, which is
        // the only case whose mode is ours to set.
        let path = dir.path().join("nested").join("snap.json");
        let cache = SnapshotCache::new(path.clone());
        cache
            .store(
                &[entry(
                    "/aisix/provider_keys/pk-1",
                    br#"{"display_name":"pk","api_key":"sk-live-secret"}"#,
                    1,
                )],
                1,
                &[],
            )
            .await;

        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            file_mode, 0o600,
            "the snapshot cache carries upstream credentials; it must not be \
             readable by other local accounts (got {file_mode:o})"
        );

        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "a directory this code creates for that file must not be \
             traversable by others (got {dir_mode:o})"
        );

        // The tmp sibling is gone — a leftover at a wider mode would defeat
        // the whole thing.
        assert!(
            !path.with_extension("tmp").exists(),
            "the temporary file must not survive the rename"
        );

        // A `.tmp` left by a crash is REUSED by the next write, and
        // `OpenOptions::mode` applies only to files it creates — so without
        // the re-assert the old, looser mode survives the rename and gets
        // published. Plant one and check.
        let stale_tmp = path.with_extension("tmp");
        std::fs::write(&stale_tmp, b"leftover").unwrap();
        std::fs::set_permissions(&stale_tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        cache
            .store(&[entry("/aisix/models/m-1", b"{}", 2)], 2, &[])
            .await;
        let after = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            after, 0o600,
            "a crash-leftover tmp file must not publish its own looser mode \
             (got {after:o})"
        );
    }
}
