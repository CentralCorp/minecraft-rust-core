use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::config::LaunchOptions;
use crate::downloader::{DownloadTask, ExpectedHash};
use crate::error::CoreResult;
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
        }
    }
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
}

/// Manages custom instance file manifests, integrity verification (`check_bundle`), and safe
/// extraneous file cleanup (`check_files`).
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

    /// Fetches the custom file manifest from `manifest_url` (if provided) and maps every entry
    /// into the isolated `game_dir` (`<path>/instances/<slug>` or `<path>`).
    pub async fn fetch_custom_distribution(
        &self,
        http: &reqwest::Client,
        manifest_url: Option<&str>,
    ) -> CoreResult<Vec<BundleItem>> {
        let Some(url) = manifest_url.map(str::trim).filter(|u| !u.is_empty()) else {
            return Ok(Vec::new());
        };

        let resp = http.get(url).send().await?;
        if !resp.status().is_success() {
            return Err(crate::error::CoreError::DownloadFailed {
                url: url.to_string(),
                status: Some(resp.status().as_u16()),
                message: "Failed to fetch instance files manifest".to_string(),
            });
        }

        let entries: Vec<RemoteFileEntry> = resp.json().await?;
        let mut out = Vec::with_capacity(entries.len());

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

            out.push(BundleItem {
                path: self.game_dir.join(&safe_rel),
                url: Some(entry.url),
                sha1: entry.hash.filter(|h| !h.trim().is_empty()),
                size: entry.size,
                category,
                inline_content: None,
                executable: false,
            });
        }

        Ok(out)
    }

    /// Checks whether a path inside `game_dir` matches any `ignored` file or directory prefix
    /// recursively (used identically by BOTH [`Self::check_bundle`] and [`Self::check_files`]!).
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
    /// verifies SHA-1 hashes, and returns the list of [`DownloadTask`]s needed.
    pub async fn check_bundle(
        &self,
        items: &[BundleItem],
        on_event: &EventCallback,
    ) -> CoreResult<Vec<DownloadTask>> {
        let mut to_download = Vec::new();
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
            // do NOT re-download `foo.jar`!
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
                        to_download.push(DownloadTask {
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
                to_download.push(DownloadTask {
                    url: url.clone(),
                    destination: full_path,
                    size: item.size,
                    expected_hash: item.sha1.clone().map(ExpectedHash::Sha1),
                    category: item.category.clone(),
                    executable: item.executable,
                });
            }

            on_event(LaunchEvent::Check {
                current: idx + 1,
                total,
                category: item.category.clone(),
            });
        }

        Ok(to_download)
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
