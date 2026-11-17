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

/// Ensures `<root>/versions/<id>/natives` exists (always required by `-Djna.tmpdir` / LWJGL extract
/// path) and extracts all native shared libraries from `native_jars` excluding `META-INF`.
pub async fn extract_natives(
    root_dir: &Path,
    version_id: &str,
    native_jars: &[PathBuf],
    on_event: &EventCallback,
) -> CoreResult<PathBuf> {
    let natives_dir = root_dir.join("versions").join(version_id).join("natives");
    fs::create_dir_all(&natives_dir).await?;

    if native_jars.is_empty() {
        return Ok(natives_dir);
    }

    let jars_vec: Vec<PathBuf> = native_jars
        .iter()
        .filter(|p| p.exists())
        .cloned()
        .collect();
    if jars_vec.is_empty() {
        return Ok(natives_dir);
    }

    on_event(LaunchEvent::Extract {
        message: format!("Extracting {} native library archives...", jars_vec.len()),
    });

    let target_dir = natives_dir.clone();
    tokio::task::spawn_blocking(move || -> CoreResult<()> {
        for jar in &jars_vec {
            extract_zip_archive(jar, &target_dir, true)?;
        }
        Ok(())
    })
    .await
    .map_err(|e| CoreError::Other(format!("Native extraction task panicked: {e}")))??;

    Ok(natives_dir)
}
