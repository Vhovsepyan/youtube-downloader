use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tracing::{info, warn};

/// A cached video/audio file keyed by "{video_id}_{format}".
struct CacheEntry {
    path: PathBuf,
    size: u64,
    last_accessed: SystemTime,
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
                },
            );
        }

        info!(
            count = entries.len(),
            total_bytes, "loaded cache index from disk"
        );

        Ok(CacheIndex {
            dir: dir.to_path_buf(),
            max_bytes,
            total_bytes,
            entries,
        })
    }

    /// Returns the path to a cached file if present, and marks it as
    /// just-accessed for LRU purposes.
    pub fn get(&mut self, key: &str) -> Option<PathBuf> {
        let entry = self.entries.get_mut(key)?;
        entry.last_accessed = SystemTime::now();
        Some(entry.path.clone())
    }

    pub fn path_for(&self, key: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("{key}.{ext}"))
    }

    /// Registers a newly-downloaded file in the index and evicts the
    /// least-recently-accessed entries if the cache now exceeds its cap.
    pub fn insert(&mut self, key: String, path: PathBuf, size: u64) {
        self.total_bytes += size;
        self.entries.insert(
            key,
            CacheEntry {
                path,
                size,
                last_accessed: SystemTime::now(),
            },
        );
        self.evict_if_needed();
    }

    fn evict_if_needed(&mut self) {
        while self.total_bytes > self.max_bytes {
            let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_accessed)
                .map(|(k, _)| k.clone())
            else {
                break;
            };

            if let Some(entry) = self.entries.remove(&oldest_key) {
                if let Err(e) = std::fs::remove_file(&entry.path) {
                    warn!(path = %entry.path.display(), error = %e, "failed to remove evicted cache file");
                }
                self.total_bytes = self.total_bytes.saturating_sub(entry.size);
                info!(key = %oldest_key, "evicted cache entry");
            }
        }
    }
}
