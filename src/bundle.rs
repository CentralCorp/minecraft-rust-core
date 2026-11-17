use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use tokio::fs;

use crate::config::LaunchOptions;
use crate::downloader::{DownloadTask, Downloader, ExpectedHash};
use crate::error::{CoreError, CoreResult};
use crate::events::{EventCallback, LaunchEvent};
use crate::utils::hash::sha1_file;
use crate::utils::zip_extract::ensure_executable;

/// Protected root directories and files when `verify` cleaner runs at the root `<path>` without
/// an isolated `instances/<slug>` subfolder, preventing accidental deletion of worlds, screenshots,
/// logs, resourcepacks, shaderpacks, options, or shared caches.
const ROOT_PROTECTED_ENTRIES: &[&str] = &[
    "assets",
    "libraries",
    "versions",
    "runtime",
    "loader",
    "instances",
    "resources",
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

/// Protected player-generated directories inside an isolated `<path>/instances/<slug>` folder
/// so `verify: true` removes unauthorized mods/configs without ever wiping worlds, logs, or
/// `launcher_config/mods_config.json`.
const INSTANCE_ALWAYS_PROTECTED_ENTRIES: &[&str] = &[
    "saves",
    "screenshots",
    "logs",
    "crash-reports",
    "launcher_config",
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
        }
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

    /// Inspects `items`, writes inline `CFILE` entries immediately, skips disabled optional mods
    /// (`.jar-disable`), respects recursive `ignored` paths when a local copy already exists,
    /// verifies SHA-1 hashes, and returns the list of direct [`DownloadTask`]s needed.
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
    /// - Every local file's SHA-1 hash is verified individually (`check_bundle` parity).
    /// - Disabled optional mods (`*.jar-disable`) and existing `ignored` files are respected and
    ///   NEVER mark a ZIP package as dirty.
    /// - If one or more files belonging to a ZIP package are missing or modified, **only that
    ///   specific ZIP package** is scheduled for download (not unaffected packages).
    pub async fn plan_bundle_downloads(
        &self,
        items: &[BundleItem],
        packages: &[PackageDescriptor],
        pkg_cache_dir: &Path,
        on_event: &EventCallback,
    ) -> CoreResult<BundleDownloadPlan> {
        let pkg_map: HashMap<&str, &PackageDescriptor> = packages
            .iter()
            .filter(|p| !p.id.trim().is_empty() && !p.url.trim().is_empty())
            .map(|p| (p.id.as_str(), p))
            .collect();

        let mut direct_downloads = Vec::new();
        let mut dirty_pkg_ids = BTreeSet::new();
        let mut fallback_by_pkg: HashMap<String, Vec<DownloadTask>> = HashMap::new();
        let total = items.len();

        for (idx, item) in items.iter().enumerate() {
            let full_path = if item.path.is_absolute() {
                item.path.clone()
            } else {
                self.root_dir.join(&item.path)
            };

            let path_str = full_path.to_string_lossy();
            if path_str.ends_with(".jar-disable") {
                on_event(LaunchEvent::Check {
                    current: idx + 1,
                    total,
                    category: item.category.clone(),
                });
                continue;
            }

            // If an optional mod has been disabled by the player (`foo.jar-disable`),
            // do NOT re-download `foo.jar` or mark its ZIP package dirty!
            if path_str.ends_with(".jar") {
                let disabled_sibling = PathBuf::from(format!("{path_str}-disable"));
                if disabled_sibling.exists() {
                    on_event(LaunchEvent::Check {
                        current: idx + 1,
                        total,
                        category: item.category.clone(),
                    });
                    continue;
                }
            }

            // Direct inline content file (e.g. `assets/indexes/<id>.json`, `versions/<id>/<id>.json`)
            if let Some(ref content) = item.inline_content {
                if let Some(parent) = full_path.parent() {
                    fs::create_dir_all(parent).await?;
                }
                fs::write(&full_path, content.as_bytes()).await?;
                if item.executable {
                    ensure_executable(&full_path)?;
                }
                on_event(LaunchEvent::Check {
                    current: idx + 1,
                    total,
                    category: item.category.clone(),
                });
                continue;
            }

            let Some(ref url) = item.url else {
                continue;
            };
            if url.trim().is_empty() {
                continue;
            }

            let mut maybe_task: Option<DownloadTask> = None;

            if full_path.exists() {
                if item.executable {
                    let _ = ensure_executable(&full_path);
                }

                // Respect recursive `ignored` files & folders so player customizations in `options.txt`,
                // `config/`, `shaderpacks/`, etc. are never overwritten once created!
                if self.is_ignored_path(&full_path) {
                    on_event(LaunchEvent::Check {
                        current: idx + 1,
                        total,
                        category: item.category.clone(),
                    });
                    continue;
                }

                if let Some(ref expected_sha1) = item.sha1 {
                    let actual_sha1 = sha1_file(&full_path).await?;
                    if !actual_sha1.eq_ignore_ascii_case(expected_sha1.trim()) {
                        maybe_task = Some(DownloadTask {
                            url: url.clone(),
                            destination: full_path,
                            size: item.size,
                            expected_hash: Some(ExpectedHash::Sha1(expected_sha1.clone())),
                            category: item.category.clone(),
                            executable: item.executable,
                        });
                    }
                }
            } else {
                maybe_task = Some(DownloadTask {
                    url: url.clone(),
                    destination: full_path,
                    size: item.size,
                    expected_hash: item.sha1.clone().map(ExpectedHash::Sha1),
                    category: item.category.clone(),
                    executable: item.executable,
                });
            }

            if let Some(file_task) = maybe_task {
                if let Some(pkg_id) = item
                    .package_id
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

            on_event(LaunchEvent::Check {
                current: idx + 1,
                total,
                category: item.category.clone(),
            });
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
    /// safely extracts downloaded ZIP packages into `game_dir` (preserving `.jar-disable` optional
    /// mods and existing `ignored` files while verifying per-file SHA-1 hashes), and cleans up
    /// temporary archives. Automatically falls back to per-file downloads if a package download fails.
    pub async fn sync_bundle(
        &self,
        downloader: &Downloader,
        items: &[BundleItem],
        packages: &[PackageDescriptor],
        on_event: &EventCallback,
    ) -> CoreResult<()> {
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

        let plan = self
            .plan_bundle_downloads(items, packages, &pkg_cache_dir, on_event)
            .await?;

        if plan.direct_downloads.is_empty() && plan.package_downloads.is_empty() {
            return Ok(());
        }

        if plan.package_downloads.is_empty() {
            downloader
                .download_many(plan.direct_downloads, on_event)
                .await?;
            return Ok(());
        }

        // Download unpackaged files (JRE/Vanilla) first if any
        if !plan.direct_downloads.is_empty() {
            downloader
                .download_many(plan.direct_downloads, on_event)
                .await?;
        }

        fs::create_dir_all(&pkg_cache_dir).await?;

        let pkg_tasks: Vec<DownloadTask> = plan
            .package_downloads
            .iter()
            .map(|p| p.archive_task.clone())
            .collect();

        let has_fallbacks = plan
            .package_downloads
            .iter()
            .all(|p| !p.fallback_file_tasks.is_empty());

        // Suppress transient `LaunchEvent::Error` during package download if per-file fallback is available
        let pkg_event_cb: EventCallback = if has_fallbacks {
            let inner = on_event.clone();
            std::sync::Arc::new(move |ev| {
                if !matches!(ev, LaunchEvent::Error { .. }) {
                    inner(ev);
                }
            })
        } else {
            on_event.clone()
        };

        match downloader.download_many(pkg_tasks, &pkg_event_cb).await {
            Ok(()) => {
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

                let expected_arc = std::sync::Arc::new(expected_hashes);
                let exec_arc = std::sync::Arc::new(executable_paths);

                for planned in &plan.package_downloads {
                    let zip_path = planned.archive_task.destination.clone();
                    on_event(LaunchEvent::Extract {
                        message: format!("Extracting package {}...", planned.descriptor.id),
                    });

                    let mgr = self.clone();
                    let expected_ref = std::sync::Arc::clone(&expected_arc);
                    let exec_ref = std::sync::Arc::clone(&exec_arc);

                    let extract_res = tokio::task::spawn_blocking(move || {
                        extract_package_zip(&mgr, &zip_path, &expected_ref, &exec_ref)
                    })
                    .await
                    .map_err(|e| CoreError::Other(format!("Package extract task failed: {e}")))?;

                    let _ = fs::remove_file(&planned.archive_task.destination).await;

                    if let Err(err) = extract_res {
                        if !planned.fallback_file_tasks.is_empty() {
                            downloader
                                .download_many(planned.fallback_file_tasks.clone(), on_event)
                                .await?;
                        } else {
                            let _ = fs::remove_dir_all(&pkg_cache_dir).await;
                            return Err(err);
                        }
                    }
                }

                let _ = fs::remove_dir_all(&pkg_cache_dir).await;
                Ok(())
            }
            Err(pkg_err) => {
                let _ = fs::remove_dir_all(&pkg_cache_dir).await;
                if !has_fallbacks {
                    return Err(pkg_err);
                }

                // Resilient fallback: download only the missing/modified files individually
                let fallback_tasks: Vec<DownloadTask> = plan
                    .package_downloads
                    .into_iter()
                    .flat_map(|p| p.fallback_file_tasks)
                    .collect();
                downloader.download_many(fallback_tasks, on_event).await
            }
        }
    }

    /// Safely removes unlisted extraneous files inside `game_dir` when `verify: true`:
    /// - Preserves all files in `valid_items`
    /// - Preserves all `.jar-disable` optional mods
    /// - Recursively preserves all `ignored` files and directories
    /// - Protects user data (`saves`, `screenshots`, `logs`, `crash-reports`, `launcher_config`,
    ///   plus `resourcepacks`, `shaderpacks`, `options.txt`, `servers.dat`, and shared root
    ///   `assets`/`libraries`/`versions`/`runtime`/`loader`/`instances`/`resources` when cleaning
    ///   root without an instance!)
    /// - Uses `Path::parent()` bounded strictly by `game_dir` (eliminating the legacy Windows `C:\` infinite loop!).
    pub async fn check_files(&self, valid_items: &[BundleItem]) -> CoreResult<Vec<PathBuf>> {
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

        let existing_entries = collect_entries_under(&self.game_dir)?;
        let mut removed = Vec::new();

        for entry_path in existing_entries {
            let Ok(rel) = entry_path.strip_prefix(&self.game_dir) else {
                continue;
            };
            let rel_norm = normalize_rel_path(&rel.to_string_lossy());
            if rel_norm.is_empty() {
                continue;
            }

            // Never delete protected user directories/files or recursively ignored paths
            if matches_any_prefix(&rel_norm, protected_prefixes.iter().copied())
                || matches_any_prefix(&rel_norm, self.ignored_rules.iter().map(String::as_str))
            {
                continue;
            }

            // Never delete disabled optional mods
            if rel_norm.ends_with(".jar-disable") {
                continue;
            }

            let key = normalize_full_path_key(&entry_path);
            if allowed_files.contains(&key) {
                continue;
            }

            if entry_path.is_dir() {
                // Only remove an empty directory if no valid/protected files live inside it
                if is_dir_empty(&entry_path) {
                    let _ = fs::remove_dir(&entry_path).await;
                    removed.push(entry_path.clone());
                    prune_empty_ancestors(&entry_path, &self.game_dir).await;
                }
            } else if entry_path.exists() && fs::remove_file(&entry_path).await.is_ok() {
                removed.push(entry_path.clone());
                prune_empty_ancestors(&entry_path, &self.game_dir).await;
            }
        }

        Ok(removed)
    }
}

fn collect_entries_under(dir: &Path) -> CoreResult<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_entries_recursive(dir, &mut out)?;
    Ok(out)
}

fn collect_entries_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> CoreResult<()> {
    if !dir.exists() {
        return Ok(());
    }
    let read_dir = std::fs::read_dir(dir)?;
    let mut count = 0usize;
    for entry_res in read_dir {
        let entry = entry_res?;
        count += 1;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_entries_recursive(&path, out)?;
        } else {
            out.push(path);
        }
    }
    if count == 0 {
        out.push(dir.to_path_buf());
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
async fn prune_empty_ancestors(deleted_path: &Path, stop_at_root: &Path) {
    let mut cur_opt = deleted_path.parent();
    while let Some(cur) = cur_opt {
        if cur == stop_at_root || !cur.starts_with(stop_at_root) {
            break;
        }
        if !is_dir_empty(cur) {
            break;
        }
        if fs::remove_dir(cur).await.is_err() {
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
/// - Enforces strict ZipSlip path sanitization (`sanitize_relative_bundle_path`).
/// - Skips overwriting existing files matching `manager.is_ignored_path(...)` (`options.txt`, etc.).
/// - Preserves disabled optional mods (`foo.jar-disable`): if `foo.jar-disable` exists on disk and
///   `foo.jar` does not, updates `foo.jar-disable` in place instead of re-enabling `foo.jar`!
/// - Streams through SHA-1 and verifies against `expected_hashes` before atomically renaming `.pkgpart`.
fn extract_package_zip(
    manager: &BundleManager,
    zip_path: &Path,
    expected_hashes: &HashMap<String, String>,
    executable_paths: &HashSet<String>,
) -> CoreResult<()> {
    let file = File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    let mut copy_buf = [0_u8; 32 * 1024];

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;

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
            std::fs::create_dir_all(parent)?;
        }

        let file_name = actual_dest
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("entry");
        let part_path = actual_dest.with_file_name(format!(".{file_name}.pkgpart"));

        let mut out_file = File::create(&part_path)?;
        let mut hasher = Sha1::new();

        loop {
            let n = io::Read::read(&mut entry, &mut copy_buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&copy_buf[..n]);
            io::Write::write_all(&mut out_file, &copy_buf[..n])?;
        }
        io::Write::flush(&mut out_file)?;
        drop(out_file);

        let canonical_key = normalize_full_path_key(&canonical_dest);
        if let Some(expected_sha1) = expected_hashes.get(&canonical_key) {
            let actual_sha1 = hex::encode(hasher.finalize());
            if !actual_sha1.eq_ignore_ascii_case(expected_sha1) {
                let _ = std::fs::remove_file(&part_path);
                return Err(CoreError::HashMismatch {
                    path: actual_dest.display().to_string(),
                    expected: expected_sha1.clone(),
                    actual: actual_sha1,
                });
            }
        }

        std::fs::rename(&part_path, &actual_dest)?;

        let unix_exec = entry.unix_mode().map(|m| m & 0o111 != 0).unwrap_or(false);
        if unix_exec || executable_paths.contains(&canonical_key) {
            let _ = ensure_executable(&actual_dest);
        }
    }

    Ok(())
}
