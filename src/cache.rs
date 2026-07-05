use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use tracing::{info, warn};

/// A cached video/audio file keyed by "{video_id}_{format}".
struct CacheEntry {
    path: PathBuf,
    size: u64,
    last_accessed: SystemTime,
    /// Number of in-progress reads of this entry (see `get_for_read`).
    /// Entries with active readers are never chosen for eviction.
    active_readers: Arc<AtomicUsize>,
}

/// In-memory index of the on-disk cache, rebuilt from a directory scan at
/// startup. Eviction uses each file's last-accessed time (mtime at scan
/// time, updated in-memory on every serve) as the LRU signal.
pub struct CacheIndex {
    dir: PathBuf,
    max_bytes: u64,
    total_bytes: u64,
    entries: HashMap<String, CacheEntry>,
}

/// Held by a caller that is actively serving a cache entry's file, so that
/// `evict_if_needed` never removes it out from under them. Decrements the
/// entry's reader count when dropped.
pub struct ReadPin {
    counter: Arc<AtomicUsize>,
}

impl Drop for ReadPin {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

impl CacheIndex {
    pub fn load(dir: &Path, max_bytes: u64) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;

        let mut entries = HashMap::new();
        let mut total_bytes = 0u64;

        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(key) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let metadata = entry.metadata()?;
            let size = metadata.len();
            let last_accessed = metadata.modified().unwrap_or(SystemTime::now());

            total_bytes += size;
            entries.insert(
                key.to_string(),
                CacheEntry {
                    path,
                    size,
                    last_accessed,
                    active_readers: Arc::new(AtomicUsize::new(0)),
                },
            );
        }

        info!(
            count = entries.len(),
            total_bytes, "loaded cache index from disk"
        );

        let mut cache = CacheIndex {
            dir: dir.to_path_buf(),
            max_bytes,
            total_bytes,
            entries,
        };
        // The on-disk cache may already be over the configured cap (e.g. it
        // was populated under a larger MAX_CACHE_GB before a restart) —
        // bring it back under budget now rather than waiting for the next
        // insert to happen to notice.
        cache.evict_if_needed();
        Ok(cache)
    }

    /// Returns the path to a cached file if present, and marks it as
    /// just-accessed for LRU purposes.
    pub fn get(&mut self, key: &str) -> Option<PathBuf> {
        let entry = self.entries.get_mut(key)?;
        entry.last_accessed = SystemTime::now();
        Some(entry.path.clone())
    }

    /// Like `get`, but also pins the entry against eviction until the
    /// returned `ReadPin` is dropped. Use this whenever the path will be
    /// handed off to actually read the file's contents.
    pub fn get_for_read(&mut self, key: &str) -> Option<(PathBuf, ReadPin)> {
        let entry = self.entries.get_mut(key)?;
        entry.last_accessed = SystemTime::now();
        entry.active_readers.fetch_add(1, Ordering::SeqCst);
        Some((
            entry.path.clone(),
            ReadPin {
                counter: Arc::clone(&entry.active_readers),
            },
        ))
    }

    pub fn path_for(&self, key: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("{key}.{ext}"))
    }

    /// Registers a newly-downloaded file in the index and evicts the
    /// least-recently-accessed entries if the cache now exceeds its cap.
    /// Fails (and removes the file) if the file alone is larger than the
    /// cache's cap, since it could never coexist with anything else — and
    /// without this check `evict_if_needed` would immediately evict the
    /// entry that was just inserted, silently leaving a `Ready` job with no
    /// file behind it.
    pub fn insert(&mut self, key: String, path: PathBuf, size: u64) -> Result<(), String> {
        if size > self.max_bytes {
            let _ = std::fs::remove_file(&path);
            return Err(format!(
                "downloaded file ({size} bytes) exceeds the cache capacity ({} bytes) and cannot be cached",
                self.max_bytes
            ));
        }

        self.total_bytes += size;
        self.entries.insert(
            key,
            CacheEntry {
                path,
                size,
                last_accessed: SystemTime::now(),
                active_readers: Arc::new(AtomicUsize::new(0)),
            },
        );
        self.evict_if_needed();
        Ok(())
    }

    fn evict_if_needed(&mut self) {
        // Entries whose file removal failed on this pass — retried on the
        // next insert rather than looped on forever right now.
        let mut skip: HashSet<String> = HashSet::new();

        while self.total_bytes > self.max_bytes {
            let Some(oldest_key) = self
                .entries
                .iter()
                .filter(|(k, e)| {
                    !skip.contains(*k) && e.active_readers.load(Ordering::SeqCst) == 0
                })
                .min_by_key(|(_, e)| e.last_accessed)
                .map(|(k, _)| k.clone())
            else {
                break;
            };

            let Some(entry) = self.entries.get(&oldest_key) else {
                break;
            };

            match std::fs::remove_file(&entry.path) {
                Ok(()) => {
                    let entry = self
                        .entries
                        .remove(&oldest_key)
                        .expect("just looked up above");
                    self.total_bytes = self.total_bytes.saturating_sub(entry.size);
                    info!(key = %oldest_key, "evicted cache entry");
                }
                Err(e) => {
                    warn!(path = %entry.path.display(), error = %e, "failed to remove evicted cache file; leaving it indexed");
                    // Don't touch total_bytes/entries — the file is still
                    // there, so the index must keep reflecting that.
                    skip.insert(oldest_key);
                }
            }
        }
    }
}
