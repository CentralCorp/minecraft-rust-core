use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tokio::fs;

use crate::bundle::BundleItem;
use crate::error::{CoreError, CoreResult};
use crate::events::{EventCallback, LaunchEvent};
use crate::utils::maven::MavenArtifact;
use crate::utils::os_rules::{
    collect_arch_specific_native_families, evaluate_rules, is_library_allowed, MojangRule,
    RuleContext,
};
use crate::utils::zip_extract::extract_zip_archive;

#[derive(Debug, Clone, Deserialize)]
struct RawMinecraftLibrary {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub rules: Option<Vec<MojangRule>>,
    #[serde(default)]
    pub natives: Option<HashMap<String, String>>,
    #[serde(default)]
    pub downloads: Option<RawLibraryDownloads>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawLibraryDownloads {
    #[serde(default)]
    pub artifact: Option<RawArtifactInfo>,
    #[serde(default)]
    pub classifiers: Option<HashMap<String, RawArtifactInfo>>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawArtifactInfo {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub url: Option<String>,
}

/// Collected Vanilla libraries, client JAR, version JSON, and native JAR paths to extract.
#[derive(Debug, Clone)]
pub struct CollectedVanillaLibraries {
    pub bundle_items: Vec<BundleItem>,
    pub native_jars: Vec<PathBuf>,
}

/// Evaluates all libraries in `version_json` against `rule_ctx` (handling both legacy `natives`
/// classifiers AND MC 1.19+ `:natives-<os>[-<arch>]` libraries without cross-arch collisions!)
/// and returns [`CollectedVanillaLibraries`].
pub fn collect_vanilla_libraries(
    root_dir: &Path,
    version_id: &str,
    version_json: &serde_json::Value,
    rule_ctx: &RuleContext,
) -> CoreResult<CollectedVanillaLibraries> {
    let raw_libs: Vec<RawMinecraftLibrary> = version_json
        .get("libraries")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();

    let arch_specific_families = collect_arch_specific_native_families(
        raw_libs.iter().filter_map(|l| l.name.as_deref()),
        rule_ctx.os,
    );

    let mut bundle_items = Vec::with_capacity(raw_libs.len() + 2);
    let mut native_jars = Vec::new();

    for lib in &raw_libs {
        if !is_library_allowed(
            lib.rules.as_deref(),
            lib.name.as_deref(),
            rule_ctx,
            &arch_specific_families,
        ) {
            continue;
        }

        // 1. Standard artifact (may itself be a 1.19+ `:natives-*` JAR)
        if let Some(artifact) = lib
            .downloads
            .as_ref()
            .and_then(|d| d.artifact.as_ref())
        {
            if let Some(url) = artifact.url.as_deref().filter(|u| !u.trim().is_empty()) {
                let rel_path = artifact
                    .path
                    .clone()
                    .or_else(|| {
                        lib.name
                            .as_deref()
                            .and_then(|n| MavenArtifact::parse(n).ok().map(|a| a.relative_path()))
                    });

                if let Some(rel) = rel_path {
                    let dest = root_dir.join("libraries").join(&rel);
                    let is_modern_native = lib
                        .name
                        .as_deref()
                        .and_then(|n| MavenArtifact::parse(n).ok())
                        .and_then(|a| a.classifier)
                        .map(|c| c.to_ascii_lowercase().starts_with("natives-"))
                        .unwrap_or(false);

                    let category = if is_modern_native {
                        native_jars.push(dest.clone());
                        "Native"
                    } else {
                        "Libraries"
                    };

                    bundle_items.push(BundleItem::download(
                        dest,
                        url,
                        artifact.sha1.clone(),
                        artifact.size,
                        category,
                    ));
                }
            }
        } else if lib.natives.is_none() {
            // Very old or custom version JSON library without `downloads.artifact`
            if let Some(ref coord) = lib.name {
                if let Ok(maven) = MavenArtifact::parse(coord) {
                    let base_repo = lib
                        .url
                        .as_deref()
                        .unwrap_or("https://libraries.minecraft.net")
                        .trim_end_matches('/');
                    let rel = maven.relative_path();
                    let dest = root_dir.join("libraries").join(&rel);
                    let url = format!("{base_repo}/{rel}");
                    bundle_items.push(BundleItem::download(dest, url, None, None, "Libraries"));
                }
            }
        }

        // 2. Legacy `lib.natives` classifier map (MC 1.7.10 -> 1.18.2 & ARM LWJGL overrides)
        if let Some(ref natives_map) = lib.natives {
            if let Some(ref rules) = lib.rules {
                if !evaluate_rules(rules, rule_ctx) {
                    continue;
                }
            }

            let os_key = rule_ctx.os.mojang_name();
            let native_tpl = natives_map.get(os_key).or_else(|| match rule_ctx.os {
                crate::utils::os_rules::TargetOs::MacOs => natives_map.get("macos"),
                _ => None,
            });

            if let Some(tpl) = native_tpl {
                let classifier_key =
                    tpl.replace("${arch}", rule_ctx.effective_arch().bitness_str());
                if let Some(classifier_info) = lib
                    .downloads
                    .as_ref()
                    .and_then(|d| d.classifiers.as_ref())
                    .and_then(|c| c.get(&classifier_key))
                {
                    if let Some(url) = classifier_info
                        .url
                        .as_deref()
                        .filter(|u| !u.trim().is_empty())
                    {
                        let rel = classifier_info.path.clone().or_else(|| {
                            lib.name.as_deref().and_then(|n| {
                                MavenArtifact::parse(n).ok().map(|a| {
                                    a.with_classifier(Some(&classifier_key)).relative_path()
                                })
                            })
                        });

                        if let Some(rel) = rel {
                            let dest = root_dir.join("libraries").join(&rel);
                            native_jars.push(dest.clone());
                            bundle_items.push(BundleItem::download(
                                dest,
                                url,
                                classifier_info.sha1.clone(),
                                classifier_info.size,
                                "Native",
                            ));
                        }
                    }
                }
            }
        }
    }

    // Main Vanilla client JAR (`versions/<id>/<id>.jar`)
    if let Some(client_dl) = version_json
        .get("downloads")
        .and_then(|d| d.get("client"))
    {
        if let Some(url) = client_dl.get("url").and_then(|u| u.as_str()) {
            let sha1 = client_dl
                .get("sha1")
                .and_then(|s| s.as_str())
                .map(str::to_string);
            let size = client_dl.get("size").and_then(|s| s.as_u64());
            let client_jar_path = root_dir
                .join("versions")
                .join(version_id)
                .join(format!("{version_id}.jar"));
            bundle_items.push(BundleItem::download(
                client_jar_path,
                url,
                sha1,
                size,
                "Minecraft",
            ));
        }
    }

    // Persist `versions/<id>/<id>.json`
    let version_json_path = root_dir
        .join("versions")
        .join(version_id)
        .join(format!("{version_id}.json"));
    bundle_items.push(BundleItem::inline_file(
        version_json_path,
        serde_json::to_string_pretty(version_json)?,
    ));

    // Deduplicate repeated base artifacts (e.g. pre-1.19 Mojang manifests where both the base
    // entry and the `natives` classifier entry list `downloads.artifact`)
    let mut seen_bundle_paths = std::collections::HashSet::with_capacity(bundle_items.len());
    bundle_items.retain(|item| {
        let key = item.path.to_string_lossy().replace('\\', "/");
        seen_bundle_paths.insert(key)
    });

    let mut seen_native_paths = std::collections::HashSet::with_capacity(native_jars.len());
    native_jars.retain(|p| {
        let key = p.to_string_lossy().replace('\\', "/");
        seen_native_paths.insert(key)
    });

    Ok(CollectedVanillaLibraries {
        bundle_items,
        native_jars,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
struct NativeJarStampEntry {
    path: String,
    size: u64,
    mtime_secs: i64,
    mtime_nanos: u32,
}

#[derive(Debug, Clone, serde::Serialize, Deserialize)]
struct NativesStampFile {
    version: u32,
    jars: Vec<NativeJarStampEntry>,
    #[serde(default)]
    extracted_files: Vec<String>,
}

const NATIVES_STAMP_VERSION: u32 = 1;
const NATIVES_STAMP_FILENAME: &str = ".natives.json";

/// Ensures `<root>/versions/<id>/natives` exists (always required by `-Djna.tmpdir` / LWJGL extract
/// path) and extracts all native shared libraries from `native_jars` excluding `META-INF`.
pub async fn extract_natives(
    root_dir: &Path,
    version_id: &str,
    native_jars: &[PathBuf],
    on_event: &EventCallback,
) -> CoreResult<PathBuf> {
    let natives_dir = root_dir.join("versions").join(version_id).join("natives");
    extract_natives_to(&natives_dir, native_jars, on_event).await
}

/// Ensures `natives_dir` exists (e.g. `<game_dir>/natives` for isolated multi-instance safety or
/// `<root>/versions/<id>/natives` otherwise) and extracts native shared libraries from `native_jars`
/// excluding `META-INF`, using `<natives_dir>/.natives.json` (`path + size + mtime`) so warm
/// launches skip re-extracting unchanged `.dll`/`.so`/`.dylib` files.
pub async fn extract_natives_to(
    natives_dir: &Path,
    native_jars: &[PathBuf],
    on_event: &EventCallback,
) -> CoreResult<PathBuf> {
    fs::create_dir_all(natives_dir).await?;

    if native_jars.is_empty() {
        return Ok(natives_dir.to_path_buf());
    }

    let jars_vec: Vec<PathBuf> = native_jars
        .iter()
        .filter(|p| p.exists())
        .cloned()
        .collect();
    if jars_vec.is_empty() {
        return Ok(natives_dir.to_path_buf());
    }

    let target_dir = natives_dir.to_path_buf();
    let cb = std::sync::Arc::clone(on_event);

    tokio::task::spawn_blocking(move || -> CoreResult<()> {
        let mut current_jars = Vec::with_capacity(jars_vec.len());
        for jar in &jars_vec {
            let meta = std::fs::metadata(jar)?;
            let (size, mtime_secs, mtime_nanos) =
                crate::utils::hash_cache::metadata_fingerprint(&meta)
                    .unwrap_or((meta.len(), 0, 0));
            current_jars.push(NativeJarStampEntry {
                path: crate::utils::hash_cache::normalize_cache_path_key(jar),
                size,
                mtime_secs,
                mtime_nanos,
            });
        }
        current_jars.sort_by(|a, b| a.path.cmp(&b.path));

        let stamp_path = target_dir.join(NATIVES_STAMP_FILENAME);
        if is_natives_stamp_valid(&target_dir, &stamp_path, &current_jars) {
            return Ok(());
        }

        cb(LaunchEvent::Extract {
            message: format!("Extracting {} native library archives...", jars_vec.len()),
        });

        let mut all_extracted = Vec::new();
        for jar in &jars_vec {
            let extracted_paths = extract_zip_archive(jar, &target_dir, true)?;
            for p in extracted_paths {
                if let Ok(rel) = p.strip_prefix(&target_dir) {
                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                    if !rel_str.is_empty() && rel_str != NATIVES_STAMP_FILENAME {
                        all_extracted.push(rel_str);
                    }
                }
            }
        }
        all_extracted.sort();
        all_extracted.dedup();

        let stamp = NativesStampFile {
            version: NATIVES_STAMP_VERSION,
            jars: current_jars,
            extracted_files: all_extracted,
        };
        if let Ok(serialized) = serde_json::to_vec(&stamp) {
            let tmp_stamp = target_dir.join(format!(
                ".natives.json.{}.tmp",
                std::process::id()
            ));
            if std::fs::write(&tmp_stamp, &serialized).is_ok() {
                crate::utils::hash_cache::clear_readonly_if_exists(&stamp_path);
                let _ = std::fs::rename(&tmp_stamp, &stamp_path);
            } else {
                let _ = std::fs::remove_file(&tmp_stamp);
            }
        }

        Ok(())
    })
    .await
    .map_err(|e| CoreError::Other(format!("Native extraction task panicked: {e}")))??;

    Ok(natives_dir.to_path_buf())
}

fn is_natives_stamp_valid(
    natives_dir: &Path,
    stamp_path: &Path,
    current_jars: &[NativeJarStampEntry],
) -> bool {
    let Ok(bytes) = std::fs::read(stamp_path) else {
        return false;
    };
    let Ok(stamp) = serde_json::from_slice::<NativesStampFile>(&bytes) else {
        return false;
    };
    if stamp.version != NATIVES_STAMP_VERSION || stamp.jars != current_jars {
        return false;
    }
    stamp
        .extracted_files
        .iter()
        .all(|rel| natives_dir.join(rel).is_file())
}
