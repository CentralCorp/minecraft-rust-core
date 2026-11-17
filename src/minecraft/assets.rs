use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::bundle::BundleItem;
use crate::error::CoreResult;
use crate::minecraft::manifest::is_legacy_assets;
use crate::utils::hash::sha1_file;
use crate::utils::zip_extract::sanitize_archive_entry_path;

const RESOURCES_BASE_URL: &str = "https://resources.download.minecraft.net";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AssetIndexFile {
    #[serde(default)]
    pub objects: BTreeMap<String, AssetObject>,
    #[serde(default, rename = "virtual")]
    pub is_virtual: bool,
    #[serde(default)]
    pub map_to_resources: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AssetObject {
    pub hash: String,
    pub size: u64,
}

/// Fetches or loads the Minecraft `assetIndex` JSON and returns deduplicated [`BundleItem`] entries
/// for `<root>/assets/indexes/<id>.json` and `<root>/assets/objects/<xx>/<hash>`.
pub async fn collect_vanilla_assets(
    root_dir: &Path,
    version_json: &serde_json::Value,
    http: &reqwest::Client,
) -> CoreResult<Vec<BundleItem>> {
    let Some(asset_index) = version_json.get("assetIndex") else {
        return Ok(Vec::new());
    };

    let Some(index_id) = asset_index.get("id").and_then(|i| i.as_str()) else {
        return Ok(Vec::new());
    };
    let index_url = asset_index.get("url").and_then(|u| u.as_str());
    let expected_sha1 = asset_index.get("sha1").and_then(|s| s.as_str());

    let local_index_path = root_dir
        .join("assets")
        .join("indexes")
        .join(format!("{index_id}.json"));

    let index_raw_text = load_or_fetch_asset_index(
        http,
        index_url,
        expected_sha1,
        &local_index_path,
    )
    .await?;

    let parsed: AssetIndexFile = serde_json::from_str(&index_raw_text)?;
    let mut items = Vec::with_capacity(parsed.objects.len() + 1);

    items.push(BundleItem::inline_file(local_index_path, index_raw_text));

    // Deduplicate asset objects by SHA-1 hash so identical sound/lang blobs are never queued twice
    let mut seen_hashes = HashSet::with_capacity(parsed.objects.len());
    for obj in parsed.objects.values() {
        let hash = obj.hash.trim();
        if hash.len() < 2 || !seen_hashes.insert(hash.to_ascii_lowercase()) {
            continue;
        }
        let prefix = &hash[..2];
        let dest = root_dir
            .join("assets")
            .join("objects")
            .join(prefix)
            .join(hash);
        let url = format!("{RESOURCES_BASE_URL}/{prefix}/{hash}");

        items.push(BundleItem::download(
            dest,
            url,
            Some(hash.to_string()),
            Some(obj.size),
            "Assets",
        ));
    }

    Ok(items)
}

/// Copies hashed objects to `<root>/resources` (matching `${assets_root}` / `${game_assets}` passed
/// on `--assetsDir`!) for `legacy` / `pre-1.6` Minecraft versions, and also populates
/// `<game_dir>/resources` when `map_to_resources` is enabled on an isolated instance.
pub async fn copy_legacy_assets_if_needed(
    root_dir: &Path,
    game_dir: &Path,
    version_json: &serde_json::Value,
) -> CoreResult<()> {
    let assets_id = version_json
        .get("assetIndex")
        .and_then(|ai| ai.get("id"))
        .and_then(|id| id.as_str())
        .or_else(|| version_json.get("assets").and_then(|a| a.as_str()));

    let Some(index_id) = assets_id else {
        return Ok(());
    };

    let index_path = root_dir
        .join("assets")
        .join("indexes")
        .join(format!("{index_id}.json"));
    if !index_path.exists() {
        return Ok(());
    }

    let index_text = fs::read_to_string(&index_path).await?;
    let parsed: AssetIndexFile = serde_json::from_str(&index_text)?;

    let should_copy_legacy = is_legacy_assets(version_json) || parsed.is_virtual || parsed.map_to_resources;
    if !should_copy_legacy {
        return Ok(());
    }

    // Always populate `<root>/resources` to match `--assetsDir <root>/resources`
    let shared_resources = root_dir.join("resources");
    populate_resources_dir(root_dir, &shared_resources, &parsed).await?;

    // If `map_to_resources: true` and `game_dir != root_dir`, also mirror into `<game_dir>/resources`
    if parsed.map_to_resources && game_dir != root_dir {
        let instance_resources = game_dir.join("resources");
        populate_resources_dir(root_dir, &instance_resources, &parsed).await?;
    }

    Ok(())
}

async fn populate_resources_dir(
    root_dir: &Path,
    target_resources_dir: &Path,
    index: &AssetIndexFile,
) -> CoreResult<()> {
    for (rel_asset_key, obj) in &index.objects {
        if obj.hash.len() < 2 {
            continue;
        }
        let Some(safe_rel) = sanitize_archive_entry_path(rel_asset_key) else {
            continue;
        };

        let src = root_dir
            .join("assets")
            .join("objects")
            .join(&obj.hash[..2])
            .join(&obj.hash);
        if !src.exists() {
            continue;
        }

        let dst = target_resources_dir.join(&safe_rel);
        if dst.exists() {
            continue;
        }

        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::copy(&src, &dst).await?;
    }
    Ok(())
}

async fn load_or_fetch_asset_index(
    http: &reqwest::Client,
    index_url: Option<&str>,
    expected_sha1: Option<&str>,
    local_path: &Path,
) -> CoreResult<String> {
    if local_path.exists() {
        if let Some(exp) = expected_sha1 {
            if let Ok(actual) = sha1_file(local_path).await {
                if actual.eq_ignore_ascii_case(exp) {
                    return Ok(fs::read_to_string(local_path).await?);
                }
            }
        } else if index_url.is_none() {
            return Ok(fs::read_to_string(local_path).await?);
        }
    }

    if let Some(url) = index_url {
        match http.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(text) = resp.text().await {
                    return Ok(text);
                }
            }
            _ if local_path.exists() => {
                return Ok(fs::read_to_string(local_path).await?);
            }
            Err(e) => return Err(e.into()),
            Ok(resp) => {
                return Err(crate::error::CoreError::DownloadFailed {
                    url: url.to_string(),
                    status: Some(resp.status().as_u16()),
                    message: "Failed to download Minecraft assetIndex".to_string(),
                });
            }
        }
    }

    Ok(fs::read_to_string(local_path).await?)
}
