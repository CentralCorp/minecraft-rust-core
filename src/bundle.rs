use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use tokio::fs;
use tokio::sync::Semaphore;

use crate::config::LaunchOptions;
use crate::downloader::{spawn_speed_reporter, DownloadTask, Downloader, ExpectedHash};
use crate::error::{CoreError, CoreResult};
use crate::events::{EventCallback, LaunchEvent};
use crate::utils::hash_cache::{
    clear_readonly_if_exists, normalize_cache_path_key, FileHashCache,
};
use crate::utils::zip_extract::ensure_executable;

const IO_BUF_CAPACITY: usize = 64 * 1024;
const MAX_CONCURRENT_PACKAGE_EXTRACTS: usize = 4;

/// Protected root directories and files when `verify` cleaner runs at the root `<path>` without
/// an isolated `instances/<slug>` subfolder, preventing accidental deletion of worlds, screenshots,
/// logs, resourcepacks, shaderpacks, options, natives, or shared caches.
const ROOT_PROTECTED_ENTRIES: &[&str] = &[
    "assets",
    "libraries",
    "versions",
    "runtime",
    "loader",
    "instances",
    "resources",
    "natives",
    "saves",
    "screenshots",
    "logs",
    "crash-reports",
    "resourcepacks",
    "shaderpacks",
    "options.txt",
    "optionsof.txt",
    "optionsshaders.txt",
    "servers.dat",
    "servers.dat_old",
    "launcher_config",
];

/// Protected player-generated and runtime directories inside an isolated `<path>/instances/<slug>`
/// folder so `verify: true` removes unauthorized mods/configs without ever wiping worlds, logs,
/// per-instance `natives`, or `launcher_config/mods_config.json`.
const INSTANCE_ALWAYS_PROTECTED_ENTRIES: &[&str] = &[
    "saves",
    "screenshots",
    "logs",
    "crash-reports",
    "launcher_config",
    "natives",
];

/// Represents a local or remote file managed by [`BundleManager`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleItem {
    /// Absolute or root-relative destination path.
    pub path: PathBuf,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub category: String,
    /// Inline UTF-8 content written directly to disk without HTTP download (`CFILE` equivalent).
    #[serde(default)]
    pub inline_content: Option<String>,
    #[serde(default)]
    pub executable: bool,
    /// Optional identifier of the folder-partitioned ZIP package containing this file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_id: Option<String>,
}

impl BundleItem {
    pub fn download(
        path: PathBuf,
        url: impl Into<String>,
        sha1: Option<String>,
        size: Option<u64>,
        category: impl Into<String>,
    ) -> Self {
        Self {
            path,
            url: Some(url.into()),
            sha1,
            size,
            category: category.into(),
            inline_content: None,
            executable: false,
            package_id: None,
        }
    }

    pub fn inline_file(path: PathBuf, content: impl Into<String>) -> Self {
        Self {
            path,
            url: None,
            sha1: None,
            size: None,
            category: "CFILE".to_string(),
            inline_content: Some(content.into()),
            executable: false,
            package_id: None,
        }
    }

    pub fn with_package_id(mut self, package_id: impl Into<String>) -> Self {
        self.package_id = Some(package_id.into());
        self
    }
}

/// Metadata describing a small, folder-partitioned ZIP archive served by CentralPanel or the
/// Azuriom CentralCorp plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageDescriptor {
    pub id: String,
    #[serde(default)]
    pub folder: String,
    pub url: String,
    #[serde(default, alias = "hash")]
    pub sha1: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub files_count: Option<usize>,
}

/// Complete custom instance file distribution containing per-file SHA-1 entries AND optional
/// folder-partitioned ZIP packages for fast selective batch downloading.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CustomDistribution {
    pub items: Vec<BundleItem>,
    #[serde(default)]
    pub packages: Vec<PackageDescriptor>,
}

/// Download execution plan produced by [`BundleManager::plan_bundle_downloads`].
#[derive(Debug, Clone, Default)]
pub struct BundleDownloadPlan {
    /// Unpackaged files (Vanilla JRE, libraries, assets, or standalone custom files).
    pub direct_downloads: Vec<DownloadTask>,
    /// Selective ZIP package downloads — ONLY for packages that have at least one missing or
    /// modified member file locally.
    pub package_downloads: Vec<PlannedPackageDownload>,
}

/// A single dirty ZIP package scheduled for download & safe extraction, along with its affected
/// per-file fallback tasks in case the package endpoint is unreachable.
#[derive(Debug, Clone)]
pub struct PlannedPackageDownload {
    pub descriptor: PackageDescriptor,
    pub archive_task: DownloadTask,
    pub fallback_file_tasks: Vec<DownloadTask>,
}

/// Entry returned by CentralPanel (`/data`, `/utils/instances/{slug}/files`) or Azuriom Plugin
/// (`/api/centralcorp/files`, `/api/centralcorp/instances/{slug}/files`).
#[derive(Debug, Clone, Deserialize)]
struct RemoteFileEntry {
    path: String,
    #[serde(default, alias = "sha1")]
    hash: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    url: String,
    #[serde(default, alias = "package_id")]
    package: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RemoteManifestResponse {
    Packaged {
        #[serde(default)]
        files: Vec<RemoteFileEntry>,
        #[serde(default)]
        packages: Vec<PackageDescriptor>,
    },
    Flat(Vec<RemoteFileEntry>),
}

/// Manages custom instance file manifests, integrity verification (`check_bundle`), selective
/// folder-partitioned ZIP package downloading/extraction (`sync_bundle`), and safe extraneous
/// file cleanup (`check_files`).
#[derive(Debug, Clone)]
pub struct BundleManager {
    root_dir: PathBuf,
    game_dir: PathBuf,
    has_instance: bool,
    ignored_rules: Vec<String>,
    verify_mode: bool,
    hash_cache: Option<FileHashCache>,
}

impl BundleManager {
    /// Creates a [`BundleManager`] from [`LaunchOptions`].
    pub fn from_options(options: &LaunchOptions) -> Self {
        Self::new(
            options.path.clone(),
            options.game_dir(),
            options.instance_slug().is_some(),
            &options.ignored,
        )
        .with_verify_mode(options.verify)
    }

    /// Creates a [`BundleManager`] with explicit root/instance directories and `ignored` entries.
    pub fn new(
        root_dir: PathBuf,
        game_dir: PathBuf,
        has_instance: bool,
        ignored: &[String],
    ) -> Self {
        let ignored_rules = ignored
            .iter()
            .map(|s| normalize_rel_path(s))
            .filter(|s| !s.is_empty())
            .collect();

        Self {
            root_dir,
            game_dir,
            has_instance,
            ignored_rules,
            verify_mode: false,
            hash_cache: None,
        }
    }

    /// Configures strict `verify` mode (`options.verify`): when `false`, Mojang content-addressed
    /// assets (`assets/objects/<xx>/<40-hex-sha1>`) with matching byte size skip SHA-1 file opens;
    /// when `true`, all files verify SHA-1 via [`FileHashCache`] (or concurrent hashing on miss).
    pub fn with_verify_mode(mut self, verify: bool) -> Self {
        self.verify_mode = verify;
        self
    }

    /// Attaches a shared [`FileHashCache`] instance so metadata-to-SHA1 lookups and updates share
    /// the same in-memory state across the entire launch pipeline.
    pub fn with_hash_cache(mut self, hash_cache: FileHashCache) -> Self {
        self.hash_cache = Some(hash_cache);
        self
    }

    /// Fetches the custom file manifest from `manifest_url` (sending `X-Accept-Packages: 1`),
    /// returning both individual file integrity entries AND folder-partitioned ZIP packages
    /// when supported by the backend (with seamless fallback to flat file arrays).
    pub async fn fetch_custom_distribution_packaged(
        &self,
        http: &reqwest::Client,
        manifest_url: Option<&str>,
    ) -> CoreResult<CustomDistribution> {
        let Some(url) = manifest_url.map(str::trim).filter(|u| !u.is_empty()) else {
            return Ok(CustomDistribution::default());
        };

        let resp = http
            .get(url)
            .header("X-Accept-Packages", "1")
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(CoreError::DownloadFailed {
                url: url.to_string(),
                status: Some(resp.status().as_u16()),
                message: "Failed to fetch instance files manifest".to_string(),
            });
        }

        let parsed: RemoteManifestResponse = resp.json().await?;
        let (entries, raw_packages) = match parsed {
            RemoteManifestResponse::Packaged { files, packages } => (files, packages),
            RemoteManifestResponse::Flat(files) => (files, Vec::new()),
        };

        let packages: Vec<PackageDescriptor> = raw_packages
            .into_iter()
            .filter(|p| !p.id.trim().is_empty() && !p.url.trim().is_empty())
            .map(|mut p| {
                p.sha1 = p.sha1.filter(|h| !h.trim().is_empty());
                p
            })
            .collect();

        let valid_pkg_ids: HashSet<&str> = packages.iter().map(|p| p.id.as_str()).collect();
        let mut items = Vec::with_capacity(entries.len());

        for entry in entries {
            let Some(safe_rel) = sanitize_relative_bundle_path(&entry.path) else {
                continue;
            };
            if entry.url.trim().is_empty() {
                continue;
            }

            let category = safe_rel
                .components()
                .next()
                .and_then(|c| c.as_os_str().to_str())
                .unwrap_or("Instance")
                .to_string();

            let package_id = entry
                .package
                .map(|s| s.trim().to_string())
                .filter(|id| !id.is_empty() && valid_pkg_ids.contains(id.as_str()));

            items.push(BundleItem {
                path: self.game_dir.join(&safe_rel),
                url: Some(entry.url),
                sha1: entry.hash.filter(|h| !h.trim().is_empty()),
                size: entry.size,
                category,
                inline_content: None,
                executable: false,
                package_id,
            });
        }

        Ok(CustomDistribution { items, packages })
    }

    /// Fetches the custom file manifest from `manifest_url` (if provided) and maps every entry
    /// into the isolated `game_dir` (`<path>/instances/<slug>` or `<path>`).
    pub async fn fetch_custom_distribution(
        &self,
        http: &reqwest::Client,
        manifest_url: Option<&str>,
    ) -> CoreResult<Vec<BundleItem>> {
        Ok(self
            .fetch_custom_distribution_packaged(http, manifest_url)
            .await?
            .items)
    }

    /// Checks whether a path inside `game_dir` matches any `ignored` file or directory prefix
    /// recursively (used identically by [`Self::check_bundle`], [`Self::sync_bundle`], and
    /// [`Self::check_files`]!).
    pub fn is_ignored_path(&self, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(&self.game_dir) else {
            return false;
        };
        let rel_norm = normalize_rel_path(&rel.to_string_lossy());
        if rel_norm.is_empty() {
            return false;
        }

        let always_protected = if self.has_instance {
            INSTANCE_ALWAYS_PROTECTED_ENTRIES
        } else {
            ROOT_PROTECTED_ENTRIES
        };

        if matches_any_prefix(&rel_norm, always_protected.iter().copied()) {
            // Note: shared root caches (`assets/`, `libraries/`, `versions/`, `runtime/`) live under
            // `root_dir` when `has_instance == false`; `check_bundle` still verifies those shared
            // Vanilla/JRE items, whereas user-customized `ignored` entries are skipped if already present.
            if rel_norm == "launcher_config" || rel_norm.starts_with("launcher_config/") {
                return true;
            }
        }

        matches_any_prefix(&rel_norm, self.ignored_rules.iter().map(String::as_str))
    }

    /// Inspects `items`, writes inline `CFILE` entries if changed, skips disabled optional mods
    /// (`.jar-disable`), respects recursive `ignored` paths when a local copy already exists,
    /// verifies SHA-1 hashes via [`FileHashCache`], and returns the list of direct [`DownloadTask`]s needed.
    pub async fn check_bundle(
        &self,
        items: &[BundleItem],
        on_event: &EventCallback,
    ) -> CoreResult<Vec<DownloadTask>> {
        let dummy_cache = self.root_dir.join("loader").join(".pkg_cache");
        let plan = self
            .plan_bundle_downloads(items, &[], &dummy_cache, on_event)
            .await?;
        Ok(plan.direct_downloads)
    }

    /// Inspects `items` against `packages` and builds a selective [`BundleDownloadPlan`]:
    /// - Uses `<root>/loader/.file_hash_cache_v1.json` (`(size, mtime_secs, mtime_nanos) -> sha1`)
    ///   for 0-disk-read warm SHA-1 verification, plus a 0-file-open fast-path on Mojang
    ///   content-addressed `assets/objects/<xx>/<40-hex-sha1>` when `verify == false`.
    /// - Hashes cache-miss files concurrently across `spawn_blocking` workers reusing `64 KiB` buffers.
    /// - Skips rewriting inline `CFILE` JSON files when already byte-for-byte identical on disk.
    /// - Disabled optional mods (`*.jar-disable`) and existing `ignored` files are respected and
    ///   NEVER mark a ZIP package as dirty.
    /// - Throttles `LaunchEvent::Check` emissions (at most every 25ms or 64 items, always emitting
    ///   `current == 1` and `current == total`).
    pub async fn plan_bundle_downloads(
        &self,
        items: &[BundleItem],
        packages: &[PackageDescriptor],
        pkg_cache_dir: &Path,
        on_event: &EventCallback,
    ) -> CoreResult<BundleDownloadPlan> {
        if items.is_empty() {
            return Ok(BundleDownloadPlan::default());
        }

        let hash_cache = match &self.hash_cache {
            Some(c) => c.clone(),
            None => FileHashCache::load(&self.root_dir).await,
        };

        let mgr = self.clone();
        let items_vec = items.to_vec();
        let cache_for_scan = hash_cache.clone();
        let cb_for_scan = Arc::clone(on_event);

        // Phase 1: Metadata scan + inline CFILE diff-write + in-memory FileHashCache lookup
        type TaggedDownloadTask = (Option<String>, DownloadTask);
        let (mut pending_tasks, cache_misses) = tokio::task::spawn_blocking(
            move || -> CoreResult<(Vec<TaggedDownloadTask>, Vec<CacheMissCandidate>)> {
                let mut pending = Vec::new();
                let mut misses = Vec::new();
                let total = items_vec.len();
                let mut last_check_emit = Instant::now() - Duration::from_secs(1);
                let mut last_check_idx = 0usize;
                let mut created_inline_parents: HashSet<PathBuf> = HashSet::new();

                let mut maybe_emit_check = |idx: usize, category: &str| {
                    let current = idx + 1;
                    if current == 1
                        || current == total
                        || current.saturating_sub(last_check_idx) >= 64
                        || last_check_emit.elapsed() >= Duration::from_millis(25)
                    {
                        last_check_idx = current;
                        last_check_emit = Instant::now();
                        cb_for_scan(LaunchEvent::Check {
                            current,
                            total,
                            category: category.to_string(),
                        });
                    }
                };

                for (idx, item) in items_vec.into_iter().enumerate() {
                    let full_path = if item.path.is_absolute() {
                        item.path
                    } else {
                        mgr.root_dir.join(&item.path)
                    };

                    let path_str = full_path.to_string_lossy();
                    if path_str.ends_with(".jar-disable") {
                        maybe_emit_check(idx, &item.category);
                        continue;
                    }

                    // If an optional mod has been disabled by the player (`foo.jar-disable`),
                    // do NOT re-download `foo.jar` or mark its ZIP package dirty!
                    if path_str.ends_with(".jar") {
                        let disabled_sibling = PathBuf::from(format!("{path_str}-disable"));
                        if disabled_sibling.exists() {
                            maybe_emit_check(idx, &item.category);
                            continue;
                        }
                    }

                    // Direct inline content file (e.g. `assets/indexes/<id>.json`, `versions/<id>/<id>.json`)
                    if let Some(ref content) = item.inline_content {
                        let bytes = content.as_bytes();
                        let already_identical = std::fs::metadata(&full_path)
                            .ok()
                            .filter(|m| m.is_file() && m.len() == bytes.len() as u64)
                            .and_then(|_| std::fs::read(&full_path).ok())
                            .map(|existing| existing == bytes)
                            .unwrap_or(false);

                        if !already_identical {
                            if let Some(parent) = full_path.parent() {
                                if !parent.as_os_str().is_empty()
                                    && created_inline_parents.insert(parent.to_path_buf())
                                {
                                    std::fs::create_dir_all(parent)?;
                                }
                            }
                            clear_readonly_if_exists(&full_path);
                            std::fs::write(&full_path, bytes)?;
                        }
                        if item.executable {
                            ensure_executable(&full_path)?;
                        }
                        maybe_emit_check(idx, &item.category);
                        continue;
                    }

                    let Some(url) = item.url.filter(|u| !u.trim().is_empty()) else {
                        continue;
                    };

                    match std::fs::metadata(&full_path) {
                        Ok(meta) if meta.is_file() => {
                            if item.executable {
                                let _ = ensure_executable(&full_path);
                            }

                            // Respect recursive `ignored` files & folders so player customizations in `options.txt`,
                            // `config/`, `shaderpacks/`, etc. are never overwritten once created!
                            if mgr.is_ignored_path(&full_path) {
                                maybe_emit_check(idx, &item.category);
                                continue;
                            }

                            // Fast-path 1: Mojang content-addressed asset (`assets/objects/<xx>/<40-hex-sha1>`)
                            // When `verify == false` and byte size matches `meta.len()`, skip opening the file!
                            if !mgr.verify_mode
                                && item.size == Some(meta.len())
                                && is_mojang_content_addressed_asset(
                                    &full_path,
                                    item.sha1.as_deref(),
                                )
                            {
                                maybe_emit_check(idx, &item.category);
                                continue;
                            }

                            if let Some(expected_sha1) =
                                item.sha1.filter(|h| !h.trim().is_empty())
                            {
                                // Fast-path 2: `(size, mtime_secs, mtime_nanos) -> sha1` in-memory lookup (0 disk reads!)
                                if let Some(cached_sha1) =
                                    cache_for_scan.get_if_fresh(&full_path, &meta)
                                {
                                    if !cached_sha1.eq_ignore_ascii_case(expected_sha1.trim()) {
                                        pending.push((
                                            item.package_id,
                                            DownloadTask {
                                                url,
                                                destination: full_path,
                                                size: item.size,
                                                expected_hash: Some(ExpectedHash::Sha1(
                                                    expected_sha1,
                                                )),
                                                category: item.category.clone(),
                                                executable: item.executable,
                                            },
                                        ));
                                    }
                                } else {
                                    misses.push(CacheMissCandidate {
                                        package_id: item.package_id,
                                        expected_sha1,
                                        task: DownloadTask {
                                            url,
                                            destination: full_path,
                                            size: item.size,
                                            expected_hash: None,
                                            category: item.category.clone(),
                                            executable: item.executable,
                                        },
                                    });
                                }
                            }
                        }
                        _ => {
                            pending.push((
                                item.package_id,
                                DownloadTask {
                                    url,
                                    destination: full_path,
                                    size: item.size,
                                    expected_hash: item
                                        .sha1
                                        .filter(|h| !h.trim().is_empty())
                                        .map(ExpectedHash::Sha1),
                                    category: item.category.clone(),
                                    executable: item.executable,
                                },
                            ));
                        }
                    }

                    maybe_emit_check(idx, &item.category);
                }

                Ok((pending, misses))
            },
        )
        .await
        .map_err(|e| CoreError::Other(format!("Bundle scan worker panicked: {e}")))??;

        // Phase 2: Hash any cache-miss files concurrently across bounded `spawn_blocking` workers
        if !cache_misses.is_empty() {
            let mut unique_paths = Vec::with_capacity(cache_misses.len());
            let mut seen_keys = HashSet::with_capacity(cache_misses.len());
            for miss in &cache_misses {
                let key = normalize_cache_path_key(&miss.task.destination);
                if seen_keys.insert(key) {
                    unique_paths.push(miss.task.destination.clone());
                }
            }

            let computed_hashes = hash_cache.hash_files_concurrently(unique_paths).await?;

            for mut miss in cache_misses {
                let key = normalize_cache_path_key(&miss.task.destination);
                let matches = computed_hashes
                    .get(&key)
                    .map(|actual| actual.eq_ignore_ascii_case(miss.expected_sha1.trim()))
                    .unwrap_or(false);
                if !matches {
                    miss.task.expected_hash = Some(ExpectedHash::Sha1(miss.expected_sha1));
                    pending_tasks.push((miss.package_id, miss.task));
                }
            }

            hash_cache.save().await?;
        }

        let pkg_map: HashMap<&str, &PackageDescriptor> = packages
            .iter()
            .filter(|p| !p.id.trim().is_empty() && !p.url.trim().is_empty())
            .map(|p| (p.id.as_str(), p))
            .collect();

        let mut direct_downloads = Vec::new();
        let mut dirty_pkg_ids = BTreeSet::new();
        let mut fallback_by_pkg: HashMap<String, Vec<DownloadTask>> = HashMap::new();

        for (pkg_id_opt, file_task) in pending_tasks {
            if let Some(pkg_id) = pkg_id_opt
                .as_deref()
                .filter(|id| pkg_map.contains_key(*id))
            {
                dirty_pkg_ids.insert(pkg_id.to_string());
                fallback_by_pkg
                    .entry(pkg_id.to_string())
                    .or_default()
                    .push(file_task);
            } else {
                direct_downloads.push(file_task);
            }
        }

        // Preserve manifest ordering for dirty packages
        let mut package_downloads = Vec::with_capacity(dirty_pkg_ids.len());
        for pkg in packages {
            if !dirty_pkg_ids.contains(&pkg.id) {
                continue;
            }
            let archive_filename = format!("pkg-{}.zip", sanitize_archive_id(&pkg.id));
            let archive_dest = pkg_cache_dir.join(archive_filename);
            let category = if pkg.folder.trim().is_empty() {
                "Package".to_string()
            } else {
                pkg.folder.trim().to_string()
            };
            let archive_task = DownloadTask {
                url: pkg.url.clone(),
                destination: archive_dest,
                size: pkg.size,
                expected_hash: pkg.sha1.clone().map(ExpectedHash::Sha1),
                category,
                executable: false,
            };
            let fallback_file_tasks = fallback_by_pkg.remove(&pkg.id).unwrap_or_default();

            package_downloads.push(PlannedPackageDownload {
                descriptor: pkg.clone(),
                archive_task,
                fallback_file_tasks,
            });
        }

        Ok(BundleDownloadPlan {
            direct_downloads,
            package_downloads,
        })
    }

    /// Verifies all `items`, selectively downloads only direct files and dirty folder ZIP packages,
    /// pipelines package downloads with bounded concurrent `spawn_blocking` ZIP extraction into
    /// `game_dir` (preserving `.jar-disable` optional mods and existing `ignored` files while
    /// verifying per-file SHA-1 hashes and updating [`FileHashCache`]), and cleans up temporary
    /// archives. Uses **per-package** resilient fallback so if 1 package fails, ONLY that failed
    /// package falls back to per-file downloads without re-downloading succeeded sibling packages.
    pub async fn sync_bundle(
        &self,
        downloader: &Downloader,
        items: &[BundleItem],
        packages: &[PackageDescriptor],
        on_event: &EventCallback,
    ) -> CoreResult<()> {
        let hash_cache = match &self.hash_cache {
            Some(c) => c.clone(),
            None => match downloader.hash_cache() {
                Some(c) => c.clone(),
                None => FileHashCache::load(&self.root_dir).await,
            },
        };

        let mgr_with_cache = self.clone().with_hash_cache(hash_cache.clone());
        let downloader = downloader.clone().with_hash_cache(hash_cache.clone());

        let scope_name = self
            .game_dir
            .file_name()
            .and_then(|s| s.to_str())
            .map(sanitize_archive_id)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "root".to_string());
        let pkg_cache_dir = self
            .root_dir
            .join("loader")
            .join(".pkg_cache")
            .join(scope_name);

        let plan = mgr_with_cache
            .plan_bundle_downloads(items, packages, &pkg_cache_dir, on_event)
            .await?;

        if plan.direct_downloads.is_empty() && plan.package_downloads.is_empty() {
            hash_cache.save().await?;
            return Ok(());
        }

        if !plan.direct_downloads.is_empty() {
            downloader
                .download_many(plan.direct_downloads, on_event)
                .await?;
        }

        if plan.package_downloads.is_empty() {
            hash_cache.save().await?;
            return Ok(());
        }

        fs::create_dir_all(&pkg_cache_dir).await?;

        // Build expected per-file SHA-1 and executable lookup maps for post-extract verification
        let mut expected_hashes = HashMap::with_capacity(items.len());
        let mut executable_paths = HashSet::new();
        for item in items {
            let full = if item.path.is_absolute() {
                item.path.clone()
            } else {
                self.root_dir.join(&item.path)
            };
            let key = normalize_full_path_key(&full);
            if let Some(ref h) = item.sha1 {
                let trimmed = h.trim();
                if !trimmed.is_empty() {
                    expected_hashes.insert(key.clone(), trimmed.to_string());
                }
            }
            if item.executable {
                executable_paths.insert(key);
            }
        }

        let expected_arc = Arc::new(expected_hashes);
        let exec_arc = Arc::new(executable_paths);

        let known_total: u64 = plan
            .package_downloads
            .iter()
            .filter_map(|p| p.archive_task.size)
            .sum();
        let dl_semaphore = Arc::new(Semaphore::new(downloader.concurrency()));
        let extract_semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_PACKAGE_EXTRACTS));
        let downloaded_total = Arc::new(AtomicU64::new(0));
        let total_bytes = Arc::new(AtomicU64::new(known_total));
        let last_progress_emit = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(1)));

        let speed_handle = spawn_speed_reporter(
            Arc::clone(&downloaded_total),
            Arc::clone(&total_bytes),
            Arc::clone(on_event),
        );

        let mut join_set = tokio::task::JoinSet::new();

        for planned in plan.package_downloads {
            let dl = downloader.clone();
            let dl_sem = Arc::clone(&dl_semaphore);
            let ext_sem = Arc::clone(&extract_semaphore);
            let dl_bytes = Arc::clone(&downloaded_total);
            let tot_bytes = Arc::clone(&total_bytes);
            let last_emit = Arc::clone(&last_progress_emit);
            let cb = Arc::clone(on_event);
            let mgr = mgr_with_cache.clone();
            let expected_ref = Arc::clone(&expected_arc);
            let exec_ref = Arc::clone(&exec_arc);
            let hc = hash_cache.clone();

            join_set.spawn(async move {
                let dl_permit = dl_sem
                    .acquire_owned()
                    .await
                    .map_err(|e| CoreError::Other(format!("Package download semaphore closed: {e}")))?;

                let dl_res = dl
                    .download_task_shared(
                        &planned.archive_task,
                        &dl_bytes,
                        &tot_bytes,
                        &last_emit,
                        true,
                        &cb,
                    )
                    .await;
                drop(dl_permit);

                if let Err(dl_err) = dl_res {
                    let _ = fs::remove_file(&planned.archive_task.destination).await;
                    if !planned.fallback_file_tasks.is_empty() {
                        return Ok(Some(planned.fallback_file_tasks));
                    }
                    return Err(dl_err);
                }

                let _ext_permit = ext_sem
                    .acquire_owned()
                    .await
                    .map_err(|e| CoreError::Other(format!("Package extract semaphore closed: {e}")))?;

                cb(LaunchEvent::Extract {
                    message: format!("Extracting package {}...", planned.descriptor.id),
                });

                let zip_path = planned.archive_task.destination.clone();
                let extract_res = tokio::task::spawn_blocking(move || {
                    extract_package_zip(&mgr, &zip_path, &expected_ref, &exec_ref, Some(&hc))
                })
                .await
                .map_err(|e| CoreError::Other(format!("Package extract task panicked: {e}")))?;

                let _ = fs::remove_file(&planned.archive_task.destination).await;

                match extract_res {
                    Ok(()) => Ok(None),
                    Err(ext_err) => {
                        if !planned.fallback_file_tasks.is_empty() {
                            Ok(Some(planned.fallback_file_tasks))
                        } else {
                            Err(ext_err)
                        }
                    }
                }
            });
        }

        let mut fatal_err: Option<CoreError> = None;
        let mut fallback_tasks: Vec<DownloadTask> = Vec::new();

        while let Some(joined) = join_set.join_next().await {
            match joined {
                Ok(Ok(None)) => {}
                Ok(Ok(Some(per_pkg_fallbacks))) => {
                    fallback_tasks.extend(per_pkg_fallbacks);
                }
                Ok(Err(err)) => {
                    if fatal_err.is_none() {
                        on_event(LaunchEvent::Error {
                            message: err.to_string(),
                        });
                        fatal_err = Some(err);
                    }
                    join_set.abort_all();
                }
                Err(join_err) if !join_err.is_cancelled() => {
                    if fatal_err.is_none() {
                        fatal_err = Some(CoreError::Other(format!(
                            "Package pipeline task panicked: {join_err}"
                        )));
                    }
                }
                Err(_) => {}
            }
        }

        speed_handle.abort();
        let _ = fs::remove_dir_all(&pkg_cache_dir).await;

        if let Some(err) = fatal_err {
            let _ = hash_cache.save().await;
            return Err(err);
        }

        if fallback_tasks.is_empty() {
            let final_dl = downloaded_total.load(Ordering::Relaxed);
            let final_tot = total_bytes.load(Ordering::Relaxed).max(final_dl);
            on_event(LaunchEvent::Progress {
                downloaded: final_dl,
                total: final_tot,
                element: "complete".to_string(),
            });
        } else {
            // Per-package fallback: download ONLY files belonging to the specific package(s) that failed!
            downloader.download_many(fallback_tasks, on_event).await?;
        }

        hash_cache.save().await?;
        Ok(())
    }

    /// Safely removes unlisted extraneous files inside `game_dir` when `verify: true`:
    /// - Runs inside `tokio::task::spawn_blocking` and **short-circuits directory recursion
    ///   immediately** whenever a directory matches `protected_prefixes` or `ignored_rules`!
    /// - Preserves all files in `valid_items` and all `.jar-disable` optional mods.
    /// - Protects user data (`saves`, `screenshots`, `logs`, `crash-reports`, `launcher_config`,
    ///   `natives`, plus `resourcepacks`, `shaderpacks`, `options.txt`, `servers.dat`, and shared
    ///   root `assets`/`libraries`/`versions`/`runtime`/`loader`/`instances`/`resources` when
    ///   cleaning root without an instance!).
    /// - Uses `Path::parent()` bounded strictly by `game_dir` (eliminating the legacy Windows `C:\` infinite loop!).
    pub async fn check_files(&self, valid_items: &[BundleItem]) -> CoreResult<Vec<PathBuf>> {
        let mgr = self.clone();
        let items_vec = valid_items.to_vec();

        tokio::task::spawn_blocking(move || mgr.check_files_sync(&items_vec))
            .await
            .map_err(|e| CoreError::Other(format!("check_files task panicked: {e}")))?
    }

    fn check_files_sync(&self, valid_items: &[BundleItem]) -> CoreResult<Vec<PathBuf>> {
        if !self.game_dir.exists() {
            return Ok(Vec::new());
        }

        let mut allowed_files = HashSet::with_capacity(valid_items.len() * 2);
        for item in valid_items {
            let full = if item.path.is_absolute() {
                item.path.clone()
            } else {
                self.root_dir.join(&item.path)
            };
            allowed_files.insert(normalize_full_path_key(&full));
            let full_str = full.to_string_lossy();
            if full_str.ends_with(".jar") {
                allowed_files.insert(normalize_full_path_key(&PathBuf::from(format!(
                    "{full_str}-disable"
                ))));
            }
        }

        let protected_prefixes: &[&str] = if self.has_instance {
            INSTANCE_ALWAYS_PROTECTED_ENTRIES
        } else {
            ROOT_PROTECTED_ENTRIES
        };

        let mut existing_entries = Vec::new();
        collect_entries_short_circuit(
            &self.game_dir,
            &self.game_dir,
            protected_prefixes,
            &self.ignored_rules,
            &mut existing_entries,
        )?;

        let mut removed = Vec::new();

        for entry_path in existing_entries {
            let Ok(rel) = entry_path.strip_prefix(&self.game_dir) else {
                continue;
            };
            let rel_norm = normalize_rel_path(&rel.to_string_lossy());
            if rel_norm.is_empty() {
                continue;
            }

            if matches_any_prefix(&rel_norm, protected_prefixes.iter().copied())
                || matches_any_prefix(&rel_norm, self.ignored_rules.iter().map(String::as_str))
            {
                continue;
            }

            if rel_norm.ends_with(".jar-disable") {
                continue;
            }

            let key = normalize_full_path_key(&entry_path);
            if allowed_files.contains(&key) {
                continue;
            }

            if entry_path.is_dir() {
                if is_dir_empty(&entry_path) && std::fs::remove_dir(&entry_path).is_ok() {
                    removed.push(entry_path.clone());
                    prune_empty_ancestors_sync(&entry_path, &self.game_dir);
                }
            } else if entry_path.exists() {
                clear_readonly_if_exists(&entry_path);
                if std::fs::remove_file(&entry_path).is_ok() {
                    removed.push(entry_path.clone());
                    prune_empty_ancestors_sync(&entry_path, &self.game_dir);
                }
            }
        }

        Ok(removed)
    }
}

struct CacheMissCandidate {
    package_id: Option<String>,
    expected_sha1: String,
    task: DownloadTask,
}

/// Returns `true` when `path` is a Mojang content-addressed asset object
/// (`.../assets/objects/<xx>/<40-hex-sha1>`) whose filename matches `expected_sha1`.
fn is_mojang_content_addressed_asset(path: &Path, expected_sha1: Option<&str>) -> bool {
    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if file_name.len() != 40 || !file_name.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    if let Some(exp) = expected_sha1.map(str::trim).filter(|s| !s.is_empty()) {
        if !file_name.eq_ignore_ascii_case(exp) {
            return false;
        }
    }
    let Some(prefix_dir) = path.parent() else {
        return false;
    };
    let Some(prefix_name) = prefix_dir.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if prefix_name.len() != 2 || !prefix_name.eq_ignore_ascii_case(&file_name[..2]) {
        return false;
    }
    let Some(objects_dir) = prefix_dir.parent() else {
        return false;
    };
    if objects_dir.file_name().and_then(|n| n.to_str()) != Some("objects") {
        return false;
    }
    let Some(assets_dir) = objects_dir.parent() else {
        return false;
    };
    assets_dir.file_name().and_then(|n| n.to_str()) == Some("assets")
}

/// Walks `current_dir` while immediately skipping (never recursing into!) any entry whose
/// `game_dir`-relative path matches `protected_prefixes` or `ignored_rules`.
fn collect_entries_short_circuit(
    game_dir: &Path,
    current_dir: &Path,
    protected_prefixes: &[&str],
    ignored_rules: &[String],
    out: &mut Vec<PathBuf>,
) -> CoreResult<()> {
    if !current_dir.exists() {
        return Ok(());
    }
    let read_dir = std::fs::read_dir(current_dir)?;
    let mut count = 0usize;
    for entry_res in read_dir {
        let entry = entry_res?;
        count += 1;
        let path = entry.path();

        if let Ok(rel) = path.strip_prefix(game_dir) {
            let rel_norm = normalize_rel_path(&rel.to_string_lossy());
            if !rel_norm.is_empty()
                && (matches_any_prefix(&rel_norm, protected_prefixes.iter().copied())
                    || matches_any_prefix(&rel_norm, ignored_rules.iter().map(String::as_str)))
            {
                // Short-circuit immediately: never traverse protected or ignored directory trees!
                continue;
            }
        }

        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_entries_short_circuit(game_dir, &path, protected_prefixes, ignored_rules, out)?;
        } else {
            out.push(path);
        }
    }
    if count == 0 && current_dir != game_dir {
        out.push(current_dir.to_path_buf());
    }
    Ok(())
}

fn is_dir_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut it| it.next().is_none())
        .unwrap_or(false)
}

/// Safely walks upward from `deleted_path`'s parent directory, removing newly-emptied folders
/// while strictly stopping BEFORE `stop_at_root` (cross-platform `Path::parent()` — never loops!).
fn prune_empty_ancestors_sync(deleted_path: &Path, stop_at_root: &Path) {
    let mut cur_opt = deleted_path.parent();
    while let Some(cur) = cur_opt {
        if cur == stop_at_root || !cur.starts_with(stop_at_root) {
            break;
        }
        if !is_dir_empty(cur) {
            break;
        }
        if std::fs::remove_dir(cur).is_err() {
            break;
        }
        cur_opt = cur.parent();
    }
}

fn sanitize_relative_bundle_path(raw: &str) -> Option<PathBuf> {
    let normalized = raw.replace('\\', "/");
    let trimmed = normalized.trim_start_matches('/').trim_start_matches("./");
    if trimmed.is_empty() || trimmed.contains('\0') {
        return None;
    }

    let mut out = PathBuf::new();
    for comp in Path::new(trimmed).components() {
        match comp {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn normalize_rel_path(raw: &str) -> String {
    raw.replace('\\', "/")
        .trim()
        .trim_start_matches("./")
        .trim_matches('/')
        .to_string()
}

fn normalize_full_path_key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s
    }
}

fn matches_any_prefix<'a>(rel_norm: &str, prefixes: impl Iterator<Item = &'a str>) -> bool {
    for prefix in prefixes {
        let p = prefix
            .trim()
            .trim_start_matches("./")
            .trim_matches('/');
        if p.is_empty() {
            continue;
        }
        if if cfg!(windows) {
            rel_norm.eq_ignore_ascii_case(p)
                || (rel_norm.len() > p.len()
                    && rel_norm[..p.len()].eq_ignore_ascii_case(p)
                    && rel_norm.as_bytes()[p.len()] == b'/')
        } else {
            rel_norm == p
                || (rel_norm.starts_with(p) && rel_norm.as_bytes().get(p.len()) == Some(&b'/'))
        } {
            return true;
        }
    }
    false
}

fn sanitize_archive_id(raw: &str) -> String {
    raw.trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Extracts a downloaded folder-partitioned ZIP package into `manager.game_dir`:
/// - Uses `64 KiB` `BufReader` / `BufWriter` and deduplicated parent `create_dir_all`.
/// - Enforces strict ZipSlip path sanitization (`sanitize_relative_bundle_path`).
/// - Skips overwriting existing files matching `manager.is_ignored_path(...)` (`options.txt`, etc.).
/// - Preserves disabled optional mods (`foo.jar-disable`): if `foo.jar-disable` exists on disk and
///   `foo.jar` does not, updates `foo.jar-disable` in place instead of re-enabling `foo.jar`!
/// - Streams through SHA-1, verifies against `expected_hashes`, cleans up `.pkgpart` on any error,
///   clears read-only destination attributes before atomic rename, and updates [`FileHashCache`].
fn extract_package_zip(
    manager: &BundleManager,
    zip_path: &Path,
    expected_hashes: &HashMap<String, String>,
    executable_paths: &HashSet<String>,
    hash_cache: Option<&FileHashCache>,
) -> CoreResult<()> {
    let reader = BufReader::with_capacity(IO_BUF_CAPACITY, File::open(zip_path)?);
    let mut archive = zip::ZipArchive::new(reader)?;

    let mut copy_buf = vec![0_u8; IO_BUF_CAPACITY];
    let mut created_dirs: HashSet<PathBuf> = HashSet::new();

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let unix_exec = entry.unix_mode().map(|m| m & 0o111 != 0).unwrap_or(false);

        let raw_name = entry.name().to_string();
        if entry.is_dir() || raw_name.ends_with('/') || raw_name.ends_with('\\') {
            continue;
        }

        let Some(safe_rel) = sanitize_relative_bundle_path(&raw_name) else {
            continue;
        };

        let canonical_dest = manager.game_dir.join(&safe_rel);

        // 1. Respect existing `ignored` files/folders (`options.txt`, `shaderpacks/`, `launcher_config/`)
        if canonical_dest.exists() && manager.is_ignored_path(&canonical_dest) {
            continue;
        }

        // 2. Preserve disabled optional mods (`*.jar-disable` stays disabled while receiving updates!)
        let canon_str = canonical_dest.to_string_lossy();
        let actual_dest = if canon_str.ends_with(".jar") {
            let disabled_sibling = PathBuf::from(format!("{canon_str}-disable"));
            if !canonical_dest.exists() && disabled_sibling.exists() {
                disabled_sibling
            } else {
                canonical_dest.clone()
            }
        } else {
            canonical_dest.clone()
        };

        if let Some(parent) = actual_dest.parent() {
            if !parent.as_os_str().is_empty() && created_dirs.insert(parent.to_path_buf()) {
                std::fs::create_dir_all(parent)?;
            }
        }

        let file_name = actual_dest
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("entry");
        let part_path = actual_dest.with_file_name(format!(
            ".{file_name}.{}.pkgpart",
            std::process::id()
        ));

        let actual_sha1 = match stream_entry_to_part(&mut entry, &part_path, &mut copy_buf) {
            Ok(sha) => sha,
            Err(err) => {
                let _ = std::fs::remove_file(&part_path);
                return Err(CoreError::Io(err));
            }
        };

        let canonical_key = normalize_full_path_key(&canonical_dest);
        if let Some(expected_sha1) = expected_hashes.get(&canonical_key) {
            if !actual_sha1.eq_ignore_ascii_case(expected_sha1) {
                let _ = std::fs::remove_file(&part_path);
                return Err(CoreError::HashMismatch {
                    path: actual_dest.display().to_string(),
                    expected: expected_sha1.clone(),
                    actual: actual_sha1,
                });
            }
        }

        clear_readonly_if_exists(&actual_dest);
        #[cfg(windows)]
        if actual_dest.exists() {
            let _ = std::fs::remove_file(&actual_dest);
        }

        if let Err(rename_err) = std::fs::rename(&part_path, &actual_dest) {
            let _ = std::fs::remove_file(&part_path);
            return Err(CoreError::Io(rename_err));
        }

        if unix_exec || executable_paths.contains(&canonical_key) {
            let _ = ensure_executable(&actual_dest);
        }

        if let Some(hc) = hash_cache {
            hc.record_path(&actual_dest, actual_sha1);
        }
    }

    Ok(())
}

fn stream_entry_to_part<R: Read>(
    entry: &mut R,
    part_path: &Path,
    copy_buf: &mut [u8],
) -> io::Result<String> {
    let out_file = File::create(part_path)?;
    let mut writer = BufWriter::with_capacity(IO_BUF_CAPACITY, out_file);
    let mut hasher = Sha1::new();

    loop {
        let n = entry.read(copy_buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&copy_buf[..n]);
        writer.write_all(&copy_buf[..n])?;
    }
    writer.flush()?;
    Ok(hex::encode(hasher.finalize()))
}
