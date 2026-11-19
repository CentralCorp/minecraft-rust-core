use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::bundle::BundleItem;
use crate::error::{CoreError, CoreResult};
use crate::minecraft::manifest::is_legacy_assets;
use crate::utils::hash::sha1_bytes;
use crate::utils::zip_extract::sanitize_archive_entry_path;

const RESOURCES_BASE_URL: &str = "https://resources.download.minecraft.net";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AssetIndexFile {
    #[serde(default)]
    pub objects: BTreeMap<String, AssetObject>,
    #[serde(default, rename = "virtual")]
    pub is_virtual: bool,
    #[serde(default)]
    pub map_to_resources: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AssetObject {
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CollectedVanillaAssets {
    pub items: Vec<BundleItem>,
    pub parsed_index: Option<AssetIndexFile>,
}

/// Fetches or loads the Minecraft `assetIndex` JSON and returns deduplicated [`BundleItem`] entries
/// for `<root>/assets/indexes/<id>.json` and `<root>/assets/objects/<xx>/<hash>`.
pub async fn collect_vanilla_assets(
    root_dir: &Path,
    version_json: &serde_json::Value,
    http: &reqwest::Client,
) -> CoreResult<Vec<BundleItem>> {
    Ok(collect_vanilla_assets_parsed(root_dir, version_json, http)
        .await?
        .items)
}

/// Internal helper that parses the `assetIndex` JSON once in memory and returns both the
/// [`BundleItem`] list and the parsed [`AssetIndexFile`] for reuse by [`copy_legacy_assets_from_parsed`].
pub(crate) async fn collect_vanilla_assets_parsed(
    root_dir: &Path,
    version_json: &serde_json::Value,
    http: &reqwest::Client,
) -> CoreResult<CollectedVanillaAssets> {
    let Some(asset_index) = version_json.get("assetIndex") else {
        return Ok(CollectedVanillaAssets::default());
    };

    let Some(index_id) = asset_index.get("id").and_then(|i| i.as_str()) else {
        return Ok(CollectedVanillaAssets::default());
    };
    let index_url = asset_index.get("url").and_then(|u| u.as_str());
    let expected_sha1 = asset_index.get("sha1").and_then(|s| s.as_str());

    let local_index_path = root_dir
        .join("assets")
        .join("indexes")
        .join(format!("{index_id}.json"));

    let index_raw_text =
        load_or_fetch_asset_index(http, index_url, expected_sha1, &local_index_path).await?;

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

    Ok(CollectedVanillaAssets {
        items,
        parsed_index: Some(parsed),
    })
}

/// Copies hashed objects to `<root>/resources` (matching `${assets_root}` / `${game_assets}` passed
/// on `--assetsDir`!) for `legacy` / `pre-1.6` Minecraft versions, and also populates
/// `<game_dir>/resources` when `map_to_resources` is enabled on an isolated instance.
pub async fn copy_legacy_assets_if_needed(
    root_dir: &Path,
    game_dir: &Path,
    version_json: &serde_json::Value,
) -> CoreResult<()> {
    copy_legacy_assets_from_parsed(root_dir, game_dir, version_json, None).await
}

/// Reuses an already-parsed [`AssetIndexFile`] in memory without re-reading or re-parsing
/// `assets/indexes/<id>.json` from disk (loading from disk only if `parsed_opt` is `None`).
pub(crate) async fn copy_legacy_assets_from_parsed(
    root_dir: &Path,
    game_dir: &Path,
    version_json: &serde_json::Value,
    parsed_opt: Option<&AssetIndexFile>,
) -> CoreResult<()> {
    let owned_parsed: Option<AssetIndexFile>;
    let parsed: &AssetIndexFile = match parsed_opt {
        Some(p) => p,
        None => {
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
            owned_parsed = Some(serde_json::from_str(&index_text)?);
            owned_parsed.as_ref().expect("just assigned")
        }
    };

    let should_copy_legacy =
        is_legacy_assets(version_json) || parsed.is_virtual || parsed.map_to_resources;
    if !should_copy_legacy {
        return Ok(());
    }

    let root_buf = root_dir.to_path_buf();
    let game_buf = game_dir.to_path_buf();
    let index_clone = parsed.clone();

    tokio::task::spawn_blocking(move || -> CoreResult<()> {
        let shared_resources = root_buf.join("resources");
        populate_resources_dir_sync(&root_buf, &shared_resources, &index_clone)?;

        if index_clone.map_to_resources && game_buf != root_buf {
            let instance_resources = game_buf.join("resources");
            populate_resources_dir_sync(&root_buf, &instance_resources, &index_clone)?;
        }
        Ok(())
    })
    .await
    .map_err(|e| CoreError::Other(format!("Legacy asset copy task panicked: {e}")))?
}

fn populate_resources_dir_sync(
    root_dir: &Path,
    target_resources_dir: &Path,
    index: &AssetIndexFile,
) -> CoreResult<()> {
    let mut created_dirs: HashSet<PathBuf> = HashSet::new();

    for (rel_asset_key, obj) in &index.objects {
        if obj.hash.len() < 2 {
            continue;
        }
        let Some(safe_rel) = sanitize_archive_entry_path(rel_asset_key) else {
            continue;
        };

        let dst = target_resources_dir.join(&safe_rel);
        if dst.exists() {
            continue;
        }

        let src = root_dir
            .join("assets")
            .join("objects")
            .join(&obj.hash[..2])
            .join(&obj.hash);
        if !src.exists() {
            continue;
        }

        if let Some(parent) = dst.parent() {
            if !parent.as_os_str().is_empty() && created_dirs.insert(parent.to_path_buf()) {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::copy(&src, &dst)?;
    }
    Ok(())
}

async fn load_or_fetch_asset_index(
    http: &reqwest::Client,
    index_url: Option<&str>,
    expected_sha1: Option<&str>,
    local_path: &Path,
) -> CoreResult<String> {
    if local_path.is_file() {
        if let Ok(bytes) = fs::read(local_path).await {
            let hash_matches = match expected_sha1 {
                Some(exp) => sha1_bytes(&bytes).eq_ignore_ascii_case(exp.trim()),
                None => index_url.is_none(),
            };
            if hash_matches {
                if let Ok(text) = String::from_utf8(bytes) {
                    return Ok(text);
                }
            }
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
                return Err(CoreError::DownloadFailed {
                    url: url.to_string(),
                    status: Some(resp.status().as_u16()),
                    message: "Failed to download Minecraft assetIndex".to_string(),
                });
            }
        }
    }

    Ok(fs::read_to_string(local_path).await?)
}
