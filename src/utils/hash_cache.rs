use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::error::{CoreError, CoreResult};

const HASH_BUF_SIZE: usize = 64 * 1024;
const CACHE_SCHEMA_VERSION: u32 = 1;

/// Cached metadata fingerprint and lowercase hex SHA-1 for a single file on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedFileEntry {
    pub size: u64,
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
    pub sha1: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedHashCacheFile {
    #[serde(default = "default_schema_version")]
    version: u32,
    #[serde(default)]
    entries: HashMap<String, CachedFileEntry>,
}

fn default_schema_version() -> u32 {
    CACHE_SCHEMA_VERSION
}

#[derive(Debug)]
struct HashCacheInner {
    cache_path: Option<PathBuf>,
    entries: HashMap<String, CachedFileEntry>,
    dirty: bool,
}

/// Thread-safe incremental `(size, mtime_secs, mtime_nanos) -> sha1` file hash cache
/// persisted atomically at `<root>/loader/.file_hash_cache_v1.json`.
#[derive(Debug, Clone)]
pub struct FileHashCache {
    inner: Arc<Mutex<HashCacheInner>>,
}

impl Default for FileHashCache {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl FileHashCache {
    /// Canonical persistence location (`<root>/loader/.file_hash_cache_v1.json`).
    pub fn cache_file_path(root_dir: &Path) -> PathBuf {
        root_dir.join("loader").join(".file_hash_cache_v1.json")
    }

    /// Creates an ephemeral in-memory cache that is not backed by a file on disk.
    pub fn in_memory() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashCacheInner {
                cache_path: None,
                entries: HashMap::new(),
                dirty: false,
            })),
        }
    }

    /// Loads `<root>/loader/.file_hash_cache_v1.json` synchronously (returning an empty cache
    /// bound to that path if the file does not exist yet or cannot be parsed).
    pub fn load_sync(root_dir: &Path) -> Self {
        let path = Self::cache_file_path(root_dir);
        let entries = Self::read_entries_from_disk(&path).unwrap_or_default();
        Self {
            inner: Arc::new(Mutex::new(HashCacheInner {
                cache_path: Some(path),
                entries,
                dirty: false,
            })),
        }
    }

    /// Loads `<root>/loader/.file_hash_cache_v1.json` on a blocking worker thread.
    pub async fn load(root_dir: &Path) -> Self {
        let root = root_dir.to_path_buf();
        tokio::task::spawn_blocking(move || Self::load_sync(&root))
            .await
            .unwrap_or_else(|_| Self::load_sync(root_dir))
    }

    fn read_entries_from_disk(path: &Path) -> Option<HashMap<String, CachedFileEntry>> {
        let file = File::open(path).ok()?;
        let reader = BufReader::with_capacity(HASH_BUF_SIZE, file);
        let parsed: PersistedHashCacheFile = serde_json::from_reader(reader).ok()?;
        if parsed.version != CACHE_SCHEMA_VERSION {
            return None;
        }
        Some(parsed.entries)
    }

    /// Returns `true` if the cache has unsaved in-memory updates and a backing disk path.
    pub fn is_dirty(&self) -> bool {
        self.inner
            .lock()
            .map(|g| g.dirty && g.cache_path.is_some())
            .unwrap_or(false)
    }

    /// Returns the number of cached entries currently tracked in memory.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.entries.len()).unwrap_or(0)
    }

    /// Returns `true` when no entries are currently cached.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Looks up the cached SHA-1 for `path` using already-fetched `std::fs::Metadata`
    /// with **0 file opens and 0 disk reads**. Returns `None` on cache miss or if
    /// `(size, mtime_secs, mtime_nanos)` changed.
    pub fn get_if_fresh(&self, path: &Path, meta: &std::fs::Metadata) -> Option<String> {
        let (size, mtime_secs, mtime_nanos) = metadata_fingerprint(meta)?;
        let key = normalize_cache_path_key(path);
        let guard = self.inner.lock().ok()?;
        let entry = guard.entries.get(&key)?;
        if entry.size == size && entry.mtime_secs == mtime_secs && entry.mtime_nanos == mtime_nanos
        {
            Some(entry.sha1.clone())
        } else {
            None
        }
    }

    /// Convenience wrapper that stats `path` and checks [`Self::get_if_fresh`].
    pub fn get_if_fresh_path(&self, path: &Path) -> Option<String> {
        let meta = fs::metadata(path).ok()?;
        self.get_if_fresh(path, &meta)
    }

    /// Records a verified `sha1` for `path` using already-fetched `std::fs::Metadata`.
    pub fn record_with_metadata(
        &self,
        path: &Path,
        meta: &std::fs::Metadata,
        sha1: impl Into<String>,
    ) {
        let Some((size, mtime_secs, mtime_nanos)) = metadata_fingerprint(meta) else {
            return;
        };
        let key = normalize_cache_path_key(path);
        let new_entry = CachedFileEntry {
            size,
            mtime_secs,
            mtime_nanos,
            sha1: sha1.into().trim().to_ascii_lowercase(),
        };

        if let Ok(mut guard) = self.inner.lock() {
            let changed = guard.entries.get(&key) != Some(&new_entry);
            if changed {
                guard.entries.insert(key, new_entry);
                guard.dirty = true;
            }
        }
    }

    /// Stats `path` on disk and records its verified `sha1` into the cache.
    pub fn record_path(&self, path: &Path, sha1: impl Into<String>) {
        if let Ok(meta) = fs::metadata(path) {
            self.record_with_metadata(path, &meta, sha1);
        }
    }

    /// Computes or retrieves the SHA-1 for `path` synchronously, updating the cache on miss.
    pub fn verify_or_hash_sync(&self, path: &Path) -> CoreResult<String> {
        let meta = fs::metadata(path)?;
        if let Some(cached) = self.get_if_fresh(path, &meta) {
            return Ok(cached);
        }
        let mut buf = vec![0u8; HASH_BUF_SIZE];
        let (sha1, fresh_meta) = sha1_file_sync_with_buf(path, &mut buf)?;
        self.record_with_metadata(path, &fresh_meta, sha1.clone());
        Ok(sha1)
    }

    /// Hashes a batch of cache-miss files concurrently across bounded `spawn_blocking` workers,
    /// reusing a single `64 KiB` buffer per worker, updating [`FileHashCache`], and returning
    /// a map from normalized path key (`normalize_cache_path_key`) to lowercase SHA-1 hex.
    pub async fn hash_files_concurrently(
        &self,
        paths: Vec<PathBuf>,
    ) -> CoreResult<HashMap<String, String>> {
        if paths.is_empty() {
            return Ok(HashMap::new());
        }

        let worker_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 8)
            .min(paths.len());

        let shared_paths = Arc::new(paths);
        let next_idx = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let paths_ref = Arc::clone(&shared_paths);
            let idx_ref = Arc::clone(&next_idx);
            let cache = self.clone();

            handles.push(tokio::task::spawn_blocking(
                move || -> CoreResult<Vec<(String, CachedFileEntry)>> {
                    let mut buf = vec![0u8; HASH_BUF_SIZE];
                    let mut local_results = Vec::new();

                    loop {
                        let i = idx_ref.fetch_add(1, Ordering::Relaxed);
                        if i >= paths_ref.len() {
                            break;
                        }
                        let path = &paths_ref[i];
                        let (sha1, meta) = sha1_file_sync_with_buf(path, &mut buf)?;
                        let key = normalize_cache_path_key(path);
                        if let Some((size, mtime_secs, mtime_nanos)) = metadata_fingerprint(&meta) {
                            local_results.push((
                                key,
                                CachedFileEntry {
                                    size,
                                    mtime_secs,
                                    mtime_nanos,
                                    sha1,
                                },
                            ));
                        } else {
                            local_results.push((
                                key,
                                CachedFileEntry {
                                    size: meta.len(),
                                    mtime_secs: 0,
                                    mtime_nanos: 0,
                                    sha1,
                                },
                            ));
                        }
                    }

                    let _ = &cache;
                    Ok(local_results)
                },
            ));
        }

        let mut computed_map = HashMap::with_capacity(shared_paths.len());
        let mut new_cache_entries = Vec::with_capacity(shared_paths.len());

        for handle in handles {
            let worker_entries = handle
                .await
                .map_err(|e| CoreError::Other(format!("Hash worker panicked: {e}")))??;
            for (key, entry) in worker_entries {
                computed_map.insert(key.clone(), entry.sha1.clone());
                new_cache_entries.push((key, entry));
            }
        }

        if !new_cache_entries.is_empty() {
            if let Ok(mut guard) = self.inner.lock() {
                for (key, entry) in new_cache_entries {
                    if guard.entries.get(&key) != Some(&entry) {
                        guard.entries.insert(key, entry);
                        guard.dirty = true;
                    }
                }
            }
        }

        Ok(computed_map)
    }

    /// Persists the cache atomically via `.tmp` + `rename` if modified since load/last save.
    pub fn save_sync(&self) -> CoreResult<()> {
        let (cache_path, payload) = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| CoreError::Other("FileHashCache mutex poisoned".to_string()))?;
            if !guard.dirty {
                return Ok(());
            }
            let Some(path) = guard.cache_path.clone() else {
                guard.dirty = false;
                return Ok(());
            };
            let snapshot = PersistedHashCacheFile {
                version: CACHE_SCHEMA_VERSION,
                entries: guard.entries.clone(),
            };
            guard.dirty = false;
            (path, snapshot)
        };

        if let Some(parent) = cache_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp_path = cache_path.with_file_name(format!(
            ".file_hash_cache_v1.json.{}.tmp",
            std::process::id()
        ));

        let write_res = (|| -> CoreResult<()> {
            let file = File::create(&tmp_path)?;
            let mut writer = BufWriter::with_capacity(HASH_BUF_SIZE, file);
            serde_json::to_writer(&mut writer, &payload)?;
            writer.flush()?;
            Ok(())
        })();

        if let Err(err) = write_res {
            let _ = fs::remove_file(&tmp_path);
            if let Ok(mut guard) = self.inner.lock() {
                guard.dirty = true;
            }
            return Err(err);
        }

        clear_readonly_if_exists(&cache_path);
        if let Err(err) = fs::rename(&tmp_path, &cache_path) {
            let _ = fs::remove_file(&tmp_path);
            if let Ok(mut guard) = self.inner.lock() {
                guard.dirty = true;
            }
            return Err(CoreError::Io(err));
        }

        Ok(())
    }

    /// Persists the cache asynchronously on a blocking worker if dirty.
    pub async fn save(&self) -> CoreResult<()> {
        if !self.is_dirty() {
            return Ok(());
        }
        let clone = self.clone();
        tokio::task::spawn_blocking(move || clone.save_sync())
            .await
            .map_err(|e| CoreError::Other(format!("Hash cache save task failed: {e}")))?
    }
}

/// Extracts `(size, mtime_secs, mtime_nanos)` from [`std::fs::Metadata`].
pub fn metadata_fingerprint(meta: &std::fs::Metadata) -> Option<(u64, i64, u32)> {
    let modified = meta.modified().ok()?;
    let (secs, nanos) = match modified.duration_since(UNIX_EPOCH) {
        Ok(dur) => (dur.as_secs() as i64, dur.subsec_nanos()),
        Err(err) => {
            let dur = err.duration();
            if dur.subsec_nanos() == 0 {
                (-(dur.as_secs() as i64), 0)
            } else {
                (-(dur.as_secs() as i64) - 1, 1_000_000_000 - dur.subsec_nanos())
            }
        }
    };
    Some((meta.len(), secs, nanos))
}

/// Computes SHA-1 synchronously using a caller-supplied reusable buffer and returns `(sha1_hex, metadata)`.
pub fn sha1_file_sync_with_buf(
    path: &Path,
    buf: &mut [u8],
) -> CoreResult<(String, std::fs::Metadata)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha1::new();
    loop {
        let n = file.read(buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let meta = file.metadata()?;
    Ok((hex::encode(hasher.finalize()), meta))
}

pub(crate) fn normalize_cache_path_key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s
    }
}

pub(crate) fn clear_readonly_if_exists(path: &Path) {
    if let Ok(meta) = fs::metadata(path) {
        let perms = meta.permissions();
        if perms.readonly() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = perms.mode();
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o200));
            }
            #[cfg(not(unix))]
            {
                let mut p = perms;
                #[allow(clippy::permissions_set_readonly_false)]
                p.set_readonly(false);
                let _ = fs::set_permissions(path, p);
            }
        }
    }
}
