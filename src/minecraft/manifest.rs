use std::cmp::Ordering;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::error::{CoreError, CoreResult};
use crate::utils::maven::compare_maven_versions;
use crate::utils::os_rules::{TargetArch, TargetOs};

const PISTON_VERSION_MANIFEST_V2: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const LAUNCHERMETA_VERSION_MANIFEST_V2: &str =
    "https://launchermeta.mojang.com/mc/game/version_manifest_v2.json";

// Compile-time embedded Linux ARM32 (`aarch`) & ARM64 (`aarch64`) LWJGL overrides for LWJGL < 3.3.3
const AARCH_2_9_4: &str = include_str!("../../assets/LWJGL/aarch/2.9.4.json");
const AARCH_3_1_2: &str = include_str!("../../assets/LWJGL/aarch/3.1.2.json");
const AARCH_3_1_6: &str = include_str!("../../assets/LWJGL/aarch/3.1.6.json");
const AARCH_3_2_1: &str = include_str!("../../assets/LWJGL/aarch/3.2.1.json");
const AARCH_3_2_2: &str = include_str!("../../assets/LWJGL/aarch/3.2.2.json");
const AARCH_3_3_1: &str = include_str!("../../assets/LWJGL/aarch/3.3.1.json");
const AARCH_3_3_2: &str = include_str!("../../assets/LWJGL/aarch/3.3.2.json");

const AARCH64_2_9_4: &str = include_str!("../../assets/LWJGL/aarch64/2.9.4.json");
const AARCH64_3_1_2: &str = include_str!("../../assets/LWJGL/aarch64/3.1.2.json");
const AARCH64_3_1_6: &str = include_str!("../../assets/LWJGL/aarch64/3.1.6.json");
const AARCH64_3_2_1: &str = include_str!("../../assets/LWJGL/aarch64/3.2.1.json");
const AARCH64_3_2_2: &str = include_str!("../../assets/LWJGL/aarch64/3.2.2.json");
const AARCH64_3_3_1: &str = include_str!("../../assets/LWJGL/aarch64/3.3.1.json");
const AARCH64_3_3_2: &str = include_str!("../../assets/LWJGL/aarch64/3.3.2.json");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionManifestV2 {
    pub latest: LatestVersions,
    pub versions: Vec<VersionManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatestVersions {
    pub release: String,
    pub snapshot: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionManifestEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub release_type: String,
    pub url: String,
    #[serde(default)]
    pub sha1: Option<String>,
}

/// Resolved Vanilla Minecraft version JSON and canonical version ID.
#[derive(Debug, Clone)]
pub struct ResolvedMinecraftVersion {
    pub version_id: String,
    pub version_json: serde_json::Value,
}

/// Fetches Mojang's `version_manifest_v2.json` and the target version's JSON metadata,
/// skipping `version_manifest_v2.json` HTTP calls completely on warm launches when an explicit
/// `versions/<id>/<id>.json` is already cached locally, seamlessly falling back to cached local
/// files under `<root>/versions/` when offline, and applying Linux ARM32/ARM64 LWJGL native
/// overrides when LWJGL `< 3.3.3`.
pub async fn resolve_vanilla_version(
    root_dir: &Path,
    requested_version: &str,
    http: &reqwest::Client,
) -> CoreResult<ResolvedMinecraftVersion> {
    let trimmed_req = requested_version.trim();

    // Zero-HTTP warm fast-path: if `requested_version` is an explicit version ID (not a dynamic
    // `latest_*` shorthand) and `versions/<id>/<id>.json` already exists and parses validly on disk,
    // skip fetching `version_manifest_v2.json` and `<id>.json` over the network!
    if !is_version_shorthand(trimmed_req) && !is_unsafe_version_segment(trimmed_req) {
        let cached_path = root_dir
            .join("versions")
            .join(trimmed_req)
            .join(format!("{trimmed_req}.json"));
        if cached_path.is_file() {
            if let Ok(cached_text) = fs::read_to_string(&cached_path).await {
                if let Ok(mut cached_json) = serde_json::from_str::<serde_json::Value>(&cached_text)
                {
                    if is_valid_cached_version_json(&cached_json) {
                        if TargetOs::current() == TargetOs::Linux {
                            apply_linux_arm_lwjgl_overrides(&mut cached_json, TargetArch::current())?;
                        }
                        return Ok(ResolvedMinecraftVersion {
                            version_id: trimmed_req.to_string(),
                            version_json: cached_json,
                        });
                    }
                }
            }
        }
    }

    let manifest_cache_path = root_dir.join("versions").join("version_manifest_v2.json");
    let manifest_opt = fetch_or_load_manifest(http, &manifest_cache_path).await;

    let version_id = resolve_version_shorthand(trimmed_req, manifest_opt.as_ref())?;
    let local_version_json_path = root_dir
        .join("versions")
        .join(&version_id)
        .join(format!("{version_id}.json"));

    let mut version_json = match fetch_version_json(
        http,
        &version_id,
        manifest_opt.as_ref(),
        &local_version_json_path,
    )
    .await
    {
        Ok(json) => json,
        Err(net_err) => {
            if local_version_json_path.exists() {
                let cached = fs::read_to_string(&local_version_json_path).await?;
                serde_json::from_str(&cached)?
            } else {
                return Err(net_err);
            }
        }
    };

    if TargetOs::current() == TargetOs::Linux {
        apply_linux_arm_lwjgl_overrides(&mut version_json, TargetArch::current())?;
    }

    Ok(ResolvedMinecraftVersion {
        version_id,
        version_json,
    })
}

fn is_version_shorthand(requested: &str) -> bool {
    matches!(
        requested,
        "" | "latest_release" | "r" | "lr" | "latest_snapshot" | "s" | "ls"
    )
}

fn is_unsafe_version_segment(requested: &str) -> bool {
    requested.contains('/')
        || requested.contains('\\')
        || requested == "."
        || requested == ".."
        || requested.contains('\0')
}

fn is_valid_cached_version_json(json: &serde_json::Value) -> bool {
    json.is_object()
        && (json.get("libraries").is_some()
            || json.get("mainClass").is_some()
            || json.get("downloads").is_some())
}

/// Applies embedded ARM32 (`aarch`) or ARM64 (`aarch64`) LWJGL overrides ONLY when:
/// 1. `arch` is `TargetArch::Arm32` or `TargetArch::Arm64`, AND
/// 2. The detected LWJGL version in `version_json` is strictly `< 3.3.3` (since Mojang natively
///    ships `natives-linux-arm64` starting with LWJGL `3.3.3` in Minecraft 1.20.5 / 1.21+!).
pub fn apply_linux_arm_lwjgl_overrides(
    version_json: &mut serde_json::Value,
    arch: TargetArch,
) -> CoreResult<bool> {
    if !matches!(arch, TargetArch::Arm32 | TargetArch::Arm64) {
        return Ok(false);
    }

    let Some(libs) = version_json
        .get("libraries")
        .and_then(|l| l.as_array())
        .cloned()
    else {
        return Ok(false);
    };

    let lwjgl_version = libs.iter().find_map(|lib| {
        let name = lib.get("name")?.as_str()?;
        if name.starts_with("org.lwjgl:lwjgl:") || name.starts_with("org.lwjgl.lwjgl:lwjgl:") {
            let parts: Vec<&str> = name.split(':').collect();
            parts.get(2).map(|v| (*v).to_string())
        } else {
            None
        }
    });

    let Some(ver) = lwjgl_version else {
        return Ok(false);
    };

    // Mojang provides official Linux ARM64 LWJGL natives starting with LWJGL 3.3.3
    if compare_maven_versions(&ver, "3.3.3") != Ordering::Less {
        return Ok(false);
    }

    let Some(embedded_json) = select_embedded_lwjgl_json(arch, &ver) else {
        return Ok(false);
    };

    let parsed_override: serde_json::Value = serde_json::from_str(embedded_json)?;
    let Some(override_libs) = parsed_override.get("libraries").and_then(|l| l.as_array()) else {
        return Ok(false);
    };

    let has_jinput = libs.iter().any(|lib| {
        lib.get("name")
            .and_then(|n| n.as_str())
            .map(|n| {
                n.starts_with("net.java.jinput:jinput-platform:")
                    || n.starts_with("net.java.jinput:jinput:")
            })
            .unwrap_or(false)
    });

    let mut filtered: Vec<serde_json::Value> = libs
        .into_iter()
        .filter(|lib| {
            let name = lib.get("name").and_then(|n| n.as_str()).unwrap_or_default();
            if has_jinput && name.contains("jinput") {
                return false;
            }
            !name.contains("lwjgl")
        })
        .collect();

    filtered.extend(override_libs.iter().cloned());
    version_json["libraries"] = serde_json::Value::Array(filtered);

    Ok(true)
}

/// Checks whether a Minecraft version JSON uses the pre-1.7.3 legacy / virtual assets layout.
pub fn is_legacy_assets(version_json: &serde_json::Value) -> bool {
    matches!(
        version_json.get("assets").and_then(|a| a.as_str()),
        Some("legacy") | Some("pre-1.6")
    )
}

fn select_embedded_lwjgl_json(arch: TargetArch, lwjgl_ver: &str) -> Option<&'static str> {
    let key = if lwjgl_ver.starts_with("2.9") {
        "2.9.4"
    } else if lwjgl_ver == "3.1.2" {
        "3.1.2"
    } else if lwjgl_ver.starts_with("3.1") {
        "3.1.6"
    } else if lwjgl_ver == "3.2.1" {
        "3.2.1"
    } else if lwjgl_ver.starts_with("3.2") {
        "3.2.2"
    } else if lwjgl_ver == "3.3.1" {
        "3.3.1"
    } else if lwjgl_ver.starts_with("3.3") {
        "3.3.2"
    } else {
        return None;
    };

    match (arch, key) {
        (TargetArch::Arm32, "2.9.4") => Some(AARCH_2_9_4),
        (TargetArch::Arm32, "3.1.2") => Some(AARCH_3_1_2),
        (TargetArch::Arm32, "3.1.6") => Some(AARCH_3_1_6),
        (TargetArch::Arm32, "3.2.1") => Some(AARCH_3_2_1),
        (TargetArch::Arm32, "3.2.2") => Some(AARCH_3_2_2),
        (TargetArch::Arm32, "3.3.1") => Some(AARCH_3_3_1),
        (TargetArch::Arm32, "3.3.2") => Some(AARCH_3_3_2),
        (TargetArch::Arm64, "2.9.4") => Some(AARCH64_2_9_4),
        (TargetArch::Arm64, "3.1.2") => Some(AARCH64_3_1_2),
        (TargetArch::Arm64, "3.1.6") => Some(AARCH64_3_1_6),
        (TargetArch::Arm64, "3.2.1") => Some(AARCH64_3_2_1),
        (TargetArch::Arm64, "3.2.2") => Some(AARCH64_3_2_2),
        (TargetArch::Arm64, "3.3.1") => Some(AARCH64_3_3_1),
        (TargetArch::Arm64, "3.3.2") => Some(AARCH64_3_3_2),
        _ => None,
    }
}

async fn fetch_or_load_manifest(
    http: &reqwest::Client,
    cache_path: &Path,
) -> Option<VersionManifestV2> {
    for url in [PISTON_VERSION_MANIFEST_V2, LAUNCHERMETA_VERSION_MANIFEST_V2] {
        if let Ok(resp) = http.get(url).send().await {
            if resp.status().is_success() {
                if let Ok(text) = resp.text().await {
                    if let Ok(parsed) = serde_json::from_str::<VersionManifestV2>(&text) {
                        if let Some(parent) = cache_path.parent() {
                            let _ = fs::create_dir_all(parent).await;
                        }
                        let _ = fs::write(cache_path, text.as_bytes()).await;
                        return Some(parsed);
                    }
                }
            }
        }
    }

    if cache_path.exists() {
        if let Ok(text) = fs::read_to_string(cache_path).await {
            if let Ok(parsed) = serde_json::from_str::<VersionManifestV2>(&text) {
                return Some(parsed);
            }
        }
    }

    None
}

fn resolve_version_shorthand(
    requested: &str,
    manifest: Option<&VersionManifestV2>,
) -> CoreResult<String> {
    match requested {
        "" | "latest_release" | "r" | "lr" => manifest
            .map(|m| m.latest.release.clone())
            .ok_or_else(|| {
                CoreError::VersionNotFound(
                    "Cannot resolve 'latest_release' while offline without cached version_manifest_v2.json"
                        .to_string(),
                )
            }),
        "latest_snapshot" | "s" | "ls" => manifest
            .map(|m| m.latest.snapshot.clone())
            .ok_or_else(|| {
                CoreError::VersionNotFound(
                    "Cannot resolve 'latest_snapshot' while offline without cached version_manifest_v2.json"
                        .to_string(),
                )
            }),
        explicit => Ok(explicit.to_string()),
    }
}

async fn fetch_version_json(
    http: &reqwest::Client,
    version_id: &str,
    manifest: Option<&VersionManifestV2>,
    local_json_path: &Path,
) -> CoreResult<serde_json::Value> {
    let Some(manifest) = manifest else {
        if local_json_path.exists() {
            let text = fs::read_to_string(local_json_path).await?;
            return Ok(serde_json::from_str(&text)?);
        }
        return Err(CoreError::VersionNotFound(format!(
            "Minecraft {version_id} is not cached locally and Mojang manifest is unreachable"
        )));
    };

    let Some(entry) = manifest.versions.iter().find(|v| v.id == version_id) else {
        if local_json_path.exists() {
            let text = fs::read_to_string(local_json_path).await?;
            return Ok(serde_json::from_str(&text)?);
        }
        return Err(CoreError::VersionNotFound(format!(
            "Minecraft version '{version_id}' was not found in Mojang's manifest"
        )));
    };

    // Fast path: if local JSON already matches Mojang's SHA-1, reuse it
    if local_json_path.exists() {
        if let Some(ref exp_sha1) = entry.sha1 {
            if let Ok(local_sha1) = crate::utils::hash::sha1_file(local_json_path).await {
                if local_sha1.eq_ignore_ascii_case(exp_sha1) {
                    if let Ok(text) = fs::read_to_string(local_json_path).await {
                        if let Ok(parsed) = serde_json::from_str(&text) {
                            return Ok(parsed);
                        }
                    }
                }
            }
        }
    }

    let resp = http.get(&entry.url).send().await?.error_for_status()?;
    let json: serde_json::Value = resp.json().await?;
    Ok(json)
}
