use std::path::Path;

use serde::Deserialize;
use tokio::fs;

use crate::bundle::{BundleItem, BundleManager};
use crate::config::{LaunchOptions, LoaderType};
use crate::downloader::Downloader;
use crate::error::{CoreError, CoreResult};
use crate::events::EventCallback;
use crate::loader::forge::{is_pinned_loader_build, write_json_if_changed};
use crate::minecraft::arguments::{LoaderProfileData, RawArgumentsSection};
use crate::utils::maven::{MavenArtifact, DEFAULT_MAVEN_MIRRORS};
use crate::utils::os_rules::{evaluate_rules, MojangRule, RuleContext};
use crate::utils::percent_encode;

const FABRIC_META_V2_URL: &str = "https://meta.fabricmc.net/v2/versions";
const FABRIC_MAVEN_URL: &str = "https://maven.fabricmc.net";

const LEGACY_FABRIC_META_V2_URL: &str = "https://meta.legacyfabric.net/v2/versions";
const LEGACY_FABRIC_MAVEN_URL: &str = "https://repo.legacyfabric.net/repository/legacyfabric";

const QUILT_META_V3_URL: &str = "https://meta.quiltmc.org/v3/versions";
const QUILT_MAVEN_URL: &str = "https://maven.quiltmc.org/repository/release";

#[derive(Debug, Clone, Copy)]
struct MetaEndpoints {
    name: &'static str,
    meta_base: &'static str,
    default_maven: &'static str,
}

impl MetaEndpoints {
    fn for_loader(loader_type: LoaderType) -> CoreResult<Self> {
        match loader_type {
            LoaderType::Fabric => Ok(Self {
                name: "Fabric",
                meta_base: FABRIC_META_V2_URL,
                default_maven: FABRIC_MAVEN_URL,
            }),
            LoaderType::LegacyFabric => Ok(Self {
                name: "LegacyFabric",
                meta_base: LEGACY_FABRIC_META_V2_URL,
                default_maven: LEGACY_FABRIC_MAVEN_URL,
            }),
            LoaderType::Quilt => Ok(Self {
                name: "Quilt",
                meta_base: QUILT_META_V3_URL,
                default_maven: QUILT_MAVEN_URL,
            }),
            other => Err(CoreError::InvalidConfig(format!(
                "Loader '{other}' is not a Meta v2/v3 loader"
            ))),
        }
    }
}

#[derive(Debug, Deserialize)]
struct MetaLoaderResponseEntry {
    loader: MetaLoaderVersionInfo,
}

#[derive(Debug, Deserialize)]
struct MetaLoaderVersionInfo {
    version: String,
    #[serde(default)]
    stable: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct MetaProfileJson {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "mainClass")]
    main_class: Option<String>,
    #[serde(default, rename = "minecraftArguments")]
    minecraft_arguments: Option<String>,
    #[serde(default)]
    arguments: Option<RawArgumentsSection>,
    #[serde(default)]
    libraries: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct MetaLibraryEntry {
    name: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    sha1: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    rules: Option<Vec<MojangRule>>,
}

/// Installs Fabric (Meta v2), LegacyFabric (Meta v2), or Quilt (Meta v3), caches the profile
/// inside `<loader_dir>/loader_profile.json` for zero-HTTP pinned warm launches and offline reuse,
/// downloads/verifies all required Maven libraries into `<root>/libraries/`, and returns [`LoaderProfileData`].
pub async fn install_meta_loader(
    loader_type: LoaderType,
    options: &LaunchOptions,
    mc_version_id: &str,
    downloader: &Downloader,
    bundle_manager: &BundleManager,
    on_event: &EventCallback,
) -> CoreResult<LoaderProfileData> {
    let endpoints = MetaEndpoints::for_loader(loader_type)?;
    let loader_dir = options.loader_dir(mc_version_id);
    fs::create_dir_all(&loader_dir).await?;
    let cached_profile_path = loader_dir.join("loader_profile.json");

    // Zero-HTTP warm fast-path: when `options.loader.build` is pinned to an explicit version
    // and `loader_profile.json` already exists locally, skip querying Meta API over HTTP!
    let warm_cached_profile: Option<LoaderProfileData> =
        if is_pinned_loader_build(&options.loader.build) && cached_profile_path.is_file() {
            fs::read_to_string(&cached_profile_path)
                .await
                .ok()
                .and_then(|raw| serde_json::from_str::<LoaderProfileData>(&raw).ok())
        } else {
            None
        };

    let profile_data = match warm_cached_profile {
        Some(cached) => cached,
        None => match fetch_remote_meta_profile(
            downloader.client(),
            endpoints,
            mc_version_id,
            &options.loader.build,
        )
        .await
        {
            Ok(profile) => {
                write_json_if_changed(
                    &cached_profile_path,
                    &serde_json::to_vec_pretty(&profile)?,
                )
                .await?;
                profile
            }
            Err(err) => {
                if cached_profile_path.exists() {
                    let raw = fs::read_to_string(&cached_profile_path).await?;
                    serde_json::from_str(&raw)?
                } else {
                    return Err(err);
                }
            }
        },
    };

    let libraries_dir = options.path.join("libraries");
    fs::create_dir_all(&libraries_dir).await?;
    let rule_ctx = RuleContext::current(options.intel_enabled_mac);

    download_meta_libraries(
        &profile_data.libraries,
        &libraries_dir,
        endpoints.default_maven,
        &rule_ctx,
        downloader,
        bundle_manager,
        on_event,
    )
    .await?;

    Ok(profile_data)
}

async fn fetch_remote_meta_profile(
    http: &reqwest::Client,
    endpoints: MetaEndpoints,
    mc_version: &str,
    requested_build: &str,
) -> CoreResult<LoaderProfileData> {
    let encoded_mc = percent_encode(mc_version.trim());
    let loader_version = resolve_meta_loader_version(
        http,
        endpoints,
        &encoded_mc,
        mc_version,
        requested_build,
    )
    .await?;

    let encoded_loader = percent_encode(&loader_version);
    let profile_url = format!(
        "{}/loader/{encoded_mc}/{encoded_loader}/profile/json",
        endpoints.meta_base
    );

    let resp = http.get(&profile_url).send().await?;
    if !resp.status().is_success() {
        return Err(CoreError::LoaderError(format!(
            "{} profile not found for Minecraft {mc_version} (loader {loader_version}, HTTP {})",
            endpoints.name,
            resp.status().as_u16()
        )));
    }

    let parsed: MetaProfileJson = resp.json().await?;
    Ok(LoaderProfileData {
        id: parsed.id,
        main_class: parsed.main_class,
        minecraft_arguments: parsed.minecraft_arguments,
        arguments: parsed.arguments,
        libraries: parsed.libraries,
        is_old_forge: false,
        custom_jar_path: None,
    })
}

async fn resolve_meta_loader_version(
    http: &reqwest::Client,
    endpoints: MetaEndpoints,
    encoded_mc: &str,
    raw_mc: &str,
    requested_build: &str,
) -> CoreResult<String> {
    let build = requested_build.trim();
    let is_latest = build.is_empty() || build.eq_ignore_ascii_case("latest");
    let is_recommended = build.eq_ignore_ascii_case("recommended");

    if !is_latest && !is_recommended {
        return Ok(build.to_string());
    }

    let list_url = format!("{}/loader/{encoded_mc}", endpoints.meta_base);
    let resp = http.get(&list_url).send().await?;
    if !resp.status().is_success() {
        return Err(CoreError::LoaderError(format!(
            "{} does not list any loader builds for Minecraft {raw_mc} (HTTP {})",
            endpoints.name,
            resp.status().as_u16()
        )));
    }

    let entries: Vec<MetaLoaderResponseEntry> = resp.json().await?;
    if entries.is_empty() {
        return Err(CoreError::LoaderError(format!(
            "No {} loader builds available for Minecraft {raw_mc}",
            endpoints.name
        )));
    }

    if is_recommended {
        if let Some(stable_entry) = entries.iter().find(|e| {
            if let Some(stable) = e.loader.stable {
                stable
            } else {
                let lower = e.loader.version.to_ascii_lowercase();
                !lower.contains("beta") && !lower.contains("alpha")
            }
        }) {
            return Ok(stable_entry.loader.version.clone());
        }
    }

    Ok(entries[0].loader.version.clone())
}

async fn download_meta_libraries(
    raw_libs: &[serde_json::Value],
    libraries_dir: &Path,
    default_maven: &str,
    rule_ctx: &RuleContext,
    downloader: &Downloader,
    bundle_manager: &BundleManager,
    on_event: &EventCallback,
) -> CoreResult<()> {
    let mut bundle_items = Vec::with_capacity(raw_libs.len());

    for val in raw_libs {
        let Ok(entry) = serde_json::from_value::<MetaLibraryEntry>(val.clone()) else {
            continue;
        };
        if let Some(ref rules) = entry.rules {
            if !evaluate_rules(rules, rule_ctx) {
                continue;
            }
        }

        let Ok(artifact) = MavenArtifact::parse(&entry.name) else {
            continue;
        };
        let rel_path = artifact.relative_path();
        let dest = libraries_dir.join(&rel_path);

        let base_repo = entry
            .url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .unwrap_or(default_maven);

        let direct_url = format!("{}/{}", base_repo.trim_end_matches('/'), rel_path);
        let final_url = if dest.exists()
            || entry.sha1.is_some()
            || downloader.probe_url(&direct_url).await.is_some()
        {
            direct_url
        } else if let Some((mirror_url, _)) = downloader
            .resolve_maven_mirror(&rel_path, DEFAULT_MAVEN_MIRRORS)
            .await
        {
            mirror_url
        } else {
            direct_url
        } ;

        bundle_items.push(BundleItem::download(
            dest,
            final_url,
            entry.sha1,
            entry.size,
            "Libraries",
        ));
    }

    if !bundle_items.is_empty() {
        let tasks = bundle_manager.check_bundle(&bundle_items, on_event).await?;
        if !tasks.is_empty() {
            downloader.download_many(tasks, on_event).await?;
        }
    }

    Ok(())
}
