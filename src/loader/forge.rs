use std::collections::HashMap;
use std::fs as std_fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::bundle::{BundleItem, BundleManager};
use crate::config::LaunchOptions;
use crate::downloader::{DownloadTask, Downloader, ExpectedHash};
use crate::error::{CoreError, CoreResult};
use crate::events::{EventCallback, LaunchEvent};
use crate::loader::patcher::{
    run_processors, InstallDataEntry, PatchContext, ProcessorSpec,
};
use crate::minecraft::arguments::{LoaderProfileData, RawArgumentsSection};
use crate::utils::hash::md5_file;
use crate::utils::maven::{compare_maven_versions, MavenArtifact, DEFAULT_MAVEN_MIRRORS};
use crate::utils::os_rules::{evaluate_rules, MojangRule, RuleContext};
use crate::utils::zip_extract::{
    extract_zip_archive, extract_zip_prefix_to_dir, read_zip_entry_bytes,
};

const FORGE_PROMOTIONS_URL: &str =
    "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json";
const FORGE_MAVEN_METADATA_JSON_URL: &str =
    "https://files.minecraftforge.net/net/minecraftforge/forge/maven-metadata.json";
const FORGE_FILES_BASE_URL: &str = "https://files.minecraftforge.net/maven";
const FORGE_MAVEN_BASE_URL: &str = "https://maven.minecraftforge.net";

#[derive(Debug, Deserialize)]
struct PromotionsSlim {
    #[serde(default)]
    promos: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct ForgeBuildMeta {
    #[serde(default)]
    classifiers: HashMap<String, HashMap<String, String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct RawInstallerProfile {
    #[serde(default)]
    pub spec: Option<i32>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub json: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default, rename = "filePath")]
    pub file_path: Option<String>,
    #[serde(default)]
    pub libraries: Vec<serde_json::Value>,
    #[serde(default)]
    pub processors: Vec<ProcessorSpec>,
    #[serde(default)]
    pub data: HashMap<String, InstallDataEntry>,
    /// Present in Legacy Forge (1.7.10 – 1.12.2) `install_profile.json`.
    #[serde(default)]
    pub install: Option<LegacyInstallSection>,
    /// Present in Legacy Forge (1.7.10 – 1.12.2) `install_profile.json`.
    #[serde(default, rename = "versionInfo")]
    pub version_info: Option<EmbeddedVersionJson>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct LegacyInstallSection {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default, rename = "filePath")]
    pub file_path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct EmbeddedVersionJson {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, rename = "mainClass")]
    pub main_class: Option<String>,
    #[serde(default, rename = "minecraftArguments")]
    pub minecraft_arguments: Option<String>,
    #[serde(default)]
    pub arguments: Option<RawArgumentsSection>,
    #[serde(default)]
    pub libraries: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct InstallerLibraryEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    rules: Option<Vec<MojangRule>>,
    #[serde(default)]
    downloads: Option<InstallerLibDownloads>,
    #[serde(default)]
    clientreq: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct InstallerLibDownloads {
    #[serde(default)]
    artifact: Option<InstallerArtifact>,
}

#[derive(Debug, Clone, Deserialize)]
struct InstallerArtifact {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    sha1: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

/// Installs Minecraft Forge (`1.7.10` through `1.21+`) into the isolated loader directory,
/// extracting bundled `maven/**` artifacts, downloading required Maven libraries, running
/// `install_profile.json` processors via `java_bin`, and returning [`LoaderProfileData`].
pub async fn install_forge(
    options: &LaunchOptions,
    mc_version_id: &str,
    java_bin: &Path,
    downloader: &Downloader,
    bundle_manager: &BundleManager,
    on_event: &EventCallback,
) -> CoreResult<LoaderProfileData> {
    let loader_dir = options.loader_dir(mc_version_id);
    fs::create_dir_all(&loader_dir).await?;
    let cached_profile_path = loader_dir.join("loader_profile.json");

    let resolved_full_version = match resolve_forge_full_version(
        downloader.client(),
        mc_version_id,
        &options.loader.build,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            if cached_profile_path.exists() {
                let raw = fs::read_to_string(&cached_profile_path).await?;
                return Ok(serde_json::from_str(&raw)?);
            }
            return Err(err);
        }
    };

    let (classifier, ext, expected_md5) = fetch_forge_artifact_choice(
        downloader.client(),
        &resolved_full_version,
    )
    .await;

    let installers_dir = loader_dir.join("installers");
    fs::create_dir_all(&installers_dir).await?;
    let file_name = format!("forge-{resolved_full_version}-{classifier}.{ext}");
    let installer_path = installers_dir.join(&file_name);

    let needs_download = if !installer_path.exists() {
        true
    } else if let Some(ref md5_hex) = expected_md5 {
        match md5_file(&installer_path).await {
            Ok(actual) => !actual.eq_ignore_ascii_case(md5_hex),
            Err(_) => true,
        }
    } else {
        false
    };

    if needs_download {
        let primary_url = format!(
            "{FORGE_MAVEN_BASE_URL}/net/minecraftforge/forge/{resolved_full_version}/{file_name}"
        );
        let fallback_url = format!(
            "{FORGE_FILES_BASE_URL}/net/minecraftforge/forge/{resolved_full_version}/{file_name}"
        );

        let mut task = DownloadTask::new(primary_url, installer_path.clone(), "Loader");
        if let Some(ref md5_hex) = expected_md5 {
            task.expected_hash = Some(ExpectedHash::Md5(md5_hex.clone()));
        }

        if let Err(_first_err) = downloader.download_single(&task, on_event).await {
            task.url = fallback_url;
            downloader.download_single(&task, on_event).await?;
        }
    }

    let base_coord = MavenArtifact::parse(&format!(
        "net.minecraftforge:forge:{resolved_full_version}"
    ))
    .ok();

    let profile = process_forge_or_neoforge_installer(
        options,
        mc_version_id,
        &resolved_full_version,
        &loader_dir,
        &installer_path,
        base_coord.as_ref(),
        DEFAULT_MAVEN_MIRRORS,
        java_bin,
        downloader,
        bundle_manager,
        on_event,
    )
    .await?;

    fs::write(
        &cached_profile_path,
        serde_json::to_string_pretty(&profile)?,
    )
    .await?;

    Ok(profile)
}

/// Resolves `"latest"`, `"recommended"`, or an explicit Forge build into the canonical Maven
/// version string (e.g. `"1.20.1-47.3.0"` or `"1.7.10-10.13.4.1614-1.7.10"`).
pub async fn resolve_forge_full_version(
    http: &reqwest::Client,
    mc_version: &str,
    requested_build: &str,
) -> CoreResult<String> {
    let build = requested_build.trim();
    let is_latest = build.is_empty() || build.eq_ignore_ascii_case("latest");
    let is_recommended = build.eq_ignore_ascii_case("recommended");

    let metadata_map: Option<HashMap<String, Vec<String>>> = match http
        .get(FORGE_MAVEN_METADATA_JSON_URL)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp.json().await.ok(),
        _ => None,
    };

    let mc_builds = metadata_map
        .as_ref()
        .and_then(|m| m.get(mc_version))
        .cloned()
        .unwrap_or_default();

    if is_latest || is_recommended {
        if let Ok(resp) = http.get(FORGE_PROMOTIONS_URL).send().await {
            if resp.status().is_success() {
                if let Ok(promos) = resp.json::<PromotionsSlim>().await {
                    let target_key = if is_recommended {
                        format!("{mc_version}-recommended")
                    } else {
                        format!("{mc_version}-latest")
                    };
                    let fallback_key = format!("{mc_version}-latest");

                    if let Some(short_build) = promos
                        .promos
                        .get(&target_key)
                        .or_else(|| promos.promos.get(&fallback_key))
                    {
                        return Ok(match_full_forge_version(mc_version, short_build, &mc_builds));
                    }
                }
            }
        }

        if let Some(last) = mc_builds.last() {
            return Ok(last.clone());
        }

        return Err(CoreError::LoaderError(format!(
            "No Forge {requested_build} build found for Minecraft {mc_version}"
        )));
    }

    Ok(match_full_forge_version(mc_version, build, &mc_builds))
}

fn match_full_forge_version(mc_version: &str, build: &str, known_builds: &[String]) -> String {
    let candidates = [
        build.to_string(),
        format!("{mc_version}-{build}"),
        format!("{mc_version}-{build}-{mc_version}"),
    ];

    for cand in &candidates {
        if let Some(exact) = known_builds.iter().find(|b| b.eq_ignore_ascii_case(cand)) {
            return exact.clone();
        }
    }

    if let Some(found) = known_builds.iter().find(|b| b.contains(build)) {
        return found.clone();
    }

    if build.starts_with(&format!("{mc_version}-")) {
        build.to_string()
    } else {
        format!("{mc_version}-{build}")
    }
}

async fn fetch_forge_artifact_choice(
    http: &reqwest::Client,
    full_version: &str,
) -> (String, String, Option<String>) {
    let meta_url = format!(
        "https://files.minecraftforge.net/net/minecraftforge/forge/{full_version}/meta.json"
    );

    if let Ok(resp) = http.get(&meta_url).send().await {
        if resp.status().is_success() {
            if let Ok(meta) = resp.json::<ForgeBuildMeta>().await {
                for preferred in ["installer", "client", "universal"] {
                    if let Some(ext_map) = meta.classifiers.get(preferred) {
                        if let Some((ext, md5)) = ext_map.iter().next() {
                            return (
                                preferred.to_string(),
                                ext.clone(),
                                Some(md5.trim().to_string()),
                            );
                        }
                    }
                }
            }
        }
    }

    ("installer".to_string(), "jar".to_string(), None)
}

/// Shared installer archive processor used by both Forge (`1.7.10`–`1.21+`) and NeoForge (`1.20.1`–`1.21+`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_forge_or_neoforge_installer(
    options: &LaunchOptions,
    mc_version_id: &str,
    full_loader_version: &str,
    loader_dir: &Path,
    installer_path: &Path,
    default_base_artifact: Option<&MavenArtifact>,
    maven_mirrors: &[&str],
    java_bin: &Path,
    downloader: &Downloader,
    bundle_manager: &BundleManager,
    on_event: &EventCallback,
) -> CoreResult<LoaderProfileData> {
    let libraries_dir = options.path.join("libraries");
    fs::create_dir_all(&libraries_dir).await?;

    let install_profile_bytes = {
        let p = installer_path.to_path_buf();
        tokio::task::spawn_blocking(move || read_zip_entry_bytes(&p, "install_profile.json"))
            .await
            .map_err(|e| CoreError::LoaderError(format!("Installer read task failed: {e}")))??
    };

    // Case 1: Very old Forge zip/jar mod without `install_profile.json`
    let Some(profile_bytes) = install_profile_bytes else {
        let patched_jar = build_legacy_jar_mod_overlay(
            &options.path,
            mc_version_id,
            full_loader_version,
            loader_dir,
            installer_path,
            on_event,
        )
        .await?;
        return Ok(LoaderProfileData {
            id: Some(format!("{mc_version_id}-forge-{full_loader_version}")),
            is_old_forge: true,
            custom_jar_path: Some(patched_jar),
            ..Default::default()
        });
    };

    let raw_profile: RawInstallerProfile = serde_json::from_slice(&profile_bytes)?;
    fs::write(loader_dir.join("install_profile.json"), &profile_bytes).await?;

    // Extract ALL `maven/**` entries from the installer JAR into `<root>/libraries/` FIRST!
    {
        let inst = installer_path.to_path_buf();
        let libs_out = libraries_dir.clone();
        tokio::task::spawn_blocking(move || -> CoreResult<()> {
            let _ = extract_zip_prefix_to_dir(&inst, "maven/", &libs_out)?;
            Ok(())
        })
        .await
        .map_err(|e| CoreError::LoaderError(format!("Maven extraction task failed: {e}")))??;
    }

    let rule_ctx = RuleContext::current(options.intel_enabled_mac);

    // Case 2: Legacy Forge (1.7.10 – 1.12.2) `{ install, versionInfo }` format
    if let Some(version_info) = raw_profile.version_info {
        if let Some(ref install_sec) = raw_profile.install {
            if let (Some(ref coord), Some(ref entry_file)) =
                (&install_sec.path, &install_sec.file_path)
            {
                let art = MavenArtifact::parse(coord)?;
                let dest = art.to_library_path(&libraries_dir);
                if let Some(bytes) = read_zip_entry_bytes(installer_path, entry_file)? {
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent).await?;
                    }
                    fs::write(&dest, bytes).await?;
                }
            }
        }

        let client_libs = filter_legacy_client_libraries(&version_info.libraries);
        download_installer_libraries(
            &client_libs,
            &libraries_dir,
            &rule_ctx,
            maven_mirrors,
            downloader,
            bundle_manager,
            on_event,
        )
        .await?;

        return Ok(LoaderProfileData {
            id: version_info.id,
            main_class: version_info.main_class,
            minecraft_arguments: version_info.minecraft_arguments,
            arguments: version_info.arguments,
            libraries: client_libs,
            is_old_forge: true,
            custom_jar_path: None,
        });
    }

    // Case 3: Modern Forge (`1.12.2.2851+` / `1.13` – `1.21+`) & NeoForge (`1.20.1` – `1.21+`)
    if let (Some(ref coord), Some(ref file_entry)) = (&raw_profile.path, &raw_profile.file_path) {
        if let Ok(art) = MavenArtifact::parse(coord) {
            let dest = art.to_library_path(&libraries_dir);
            if !dest.exists() {
                if let Some(bytes) = read_zip_entry_bytes(installer_path, file_entry)? {
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent).await?;
                    }
                    fs::write(&dest, bytes).await?;
                }
            }
        }
    }

    let json_entry_name = raw_profile
        .json
        .as_deref()
        .unwrap_or("/version.json")
        .trim_start_matches('/');

    let embedded_version_bytes = read_zip_entry_bytes(installer_path, json_entry_name)?
        .ok_or_else(|| {
            CoreError::LoaderError(format!(
                "Installer '{}' is missing embedded '{json_entry_name}'",
                installer_path.display()
            ))
        })?;
    let embedded_version: EmbeddedVersionJson = serde_json::from_slice(&embedded_version_bytes)?;

    // Combine both `embedded_version.libraries` (runtime libraries) and `raw_profile.libraries`
    // (processor tool libraries), skipping explicit empty URLs (`""`) which are generated by processors
    // or already extracted from `maven/**`.
    let mut combined_libs =
        Vec::with_capacity(embedded_version.libraries.len() + raw_profile.libraries.len());
    combined_libs.extend(embedded_version.libraries.iter().cloned());
    combined_libs.extend(raw_profile.libraries.iter().cloned());

    download_installer_libraries(
        &combined_libs,
        &libraries_dir,
        &rule_ctx,
        maven_mirrors,
        downloader,
        bundle_manager,
        on_event,
    )
    .await?;

    if !raw_profile.processors.is_empty() {
        let profile_art = raw_profile
            .path
            .as_deref()
            .and_then(|p| MavenArtifact::parse(p).ok());
        let base_art_ref = profile_art.as_ref().or(default_base_artifact);

        let mc_jar = options
            .path
            .join("versions")
            .join(mc_version_id)
            .join(format!("{mc_version_id}.jar"));
        let mc_json = options
            .path
            .join("versions")
            .join(mc_version_id)
            .join(format!("{mc_version_id}.json"));

        let patch_ctx = PatchContext {
            root_dir: &options.path,
            libraries_dir: &libraries_dir,
            loader_dir,
            installer_jar: installer_path,
            mc_jar: &mc_jar,
            mc_json: &mc_json,
            java_bin,
            base_loader_artifact: base_art_ref,
        };

        run_processors(
            &raw_profile.processors,
            &raw_profile.data,
            &patch_ctx,
            on_event,
        )
        .await?;
    }

    // Legacy 1.12.2.2851+ transitional Forge profiles still use `minecraftArguments` + LaunchWrapper
    let is_old = embedded_version.minecraft_arguments.is_some()
        && embedded_version.arguments.is_none()
        && compare_maven_versions(mc_version_id, "1.13") == std::cmp::Ordering::Less;

    Ok(LoaderProfileData {
        id: embedded_version.id.or(raw_profile.version),
        main_class: embedded_version.main_class,
        minecraft_arguments: embedded_version.minecraft_arguments,
        arguments: embedded_version.arguments,
        libraries: embedded_version.libraries,
        is_old_forge: is_old,
        custom_jar_path: None,
    })
}

fn filter_legacy_client_libraries(libs: &[serde_json::Value]) -> Vec<serde_json::Value> {
    libs.iter()
        .filter(|val| {
            val.get("clientreq")
                .and_then(|c| c.as_bool())
                .unwrap_or(true)
        })
        .cloned()
        .collect()
}

pub(crate) async fn download_installer_libraries(
    raw_libs: &[serde_json::Value],
    libraries_dir: &Path,
    rule_ctx: &RuleContext,
    maven_mirrors: &[&str],
    downloader: &Downloader,
    bundle_manager: &BundleManager,
    on_event: &EventCallback,
) -> CoreResult<()> {
    let mut bundle_items = Vec::new();

    for val in raw_libs {
        let Ok(entry) = serde_json::from_value::<InstallerLibraryEntry>(val.clone()) else {
            continue;
        };

        if entry.clientreq == Some(false) {
            continue;
        }
        if let Some(ref rules) = entry.rules {
            if !evaluate_rules(rules, rule_ctx) {
                continue;
            }
        }

        let Some(ref raw_name) = entry.name else {
            continue;
        };
        let Ok(maven_art) = MavenArtifact::parse(raw_name) else {
            continue;
        };

        // Case A: Modern `downloads.artifact` entry
        if let Some(artifact) = entry.downloads.as_ref().and_then(|d| d.artifact.as_ref()) {
            let rel_path = artifact
                .path
                .clone()
                .unwrap_or_else(|| maven_art.relative_path());
            let dest = libraries_dir.join(&rel_path);

            let raw_url = artifact.url.as_deref().unwrap_or("").trim();
            if raw_url.is_empty() {
                // CRITICAL FIX: Explicit `"url": ""` means either extracted from `maven/` inside the
                // installer JAR or generated by `processors` — NEVER append `""` to a Maven mirror!
                continue;
            }

            bundle_items.push(BundleItem::download(
                dest,
                raw_url.to_string(),
                artifact.sha1.clone(),
                artifact.size,
                "Libraries",
            ));
            continue;
        }

        // Case B: Coordinate + optional Maven repository base URL (`entry.url`)
        let rel_path = maven_art.relative_path();
        let dest = libraries_dir.join(&rel_path);

        if let Some(ref custom_base) = entry.url {
            let trimmed_base = custom_base.trim();
            if trimmed_base.is_empty() {
                // Empty `"url": ""` — local / bundled artifact
                continue;
            }
            if dest.exists() {
                continue;
            }
            let candidate_url = format!("{}/{}", trimmed_base.trim_end_matches('/'), rel_path);
            let resolved_url = if downloader.probe_url(&candidate_url).await.is_some() {
                candidate_url
            } else if let Some((mirror_url, _)) =
                downloader.resolve_maven_mirror(&rel_path, maven_mirrors).await
            {
                mirror_url
            } else {
                candidate_url
            };

            bundle_items.push(BundleItem::download(
                dest,
                resolved_url,
                None,
                None,
                "Libraries",
            ));
        } else {
            if dest.exists() {
                continue;
            }
            let resolved_url = if let Some((mirror_url, _)) =
                downloader.resolve_maven_mirror(&rel_path, maven_mirrors).await
            {
                mirror_url
            } else {
                let first_mirror = maven_mirrors
                    .first()
                    .copied()
                    .unwrap_or(FORGE_MAVEN_BASE_URL);
                format!("{}/{}", first_mirror.trim_end_matches('/'), rel_path)
            };

            bundle_items.push(BundleItem::download(
                dest,
                resolved_url,
                None,
                None,
                "Libraries",
            ));
        }
    }

    if !bundle_items.is_empty() {
        let tasks = bundle_manager.check_bundle(&bundle_items, on_event).await?;
        if !tasks.is_empty() {
            downloader.download_many(tasks, on_event).await?;
        }
    }

    Ok(())
}

/// Overlays a very old Forge `client`/`universal` zip directly onto a copy of Vanilla `<mc>.jar`
/// (stripping Vanilla `META-INF`), used only for pre-`install_profile.json` legacy builds.
async fn build_legacy_jar_mod_overlay(
    root_dir: &Path,
    mc_version_id: &str,
    full_loader_version: &str,
    loader_dir: &Path,
    mod_archive_path: &Path,
    on_event: &EventCallback,
) -> CoreResult<PathBuf> {
    let out_jar = loader_dir.join(format!("minecraft-{full_loader_version}.jar"));
    if out_jar.exists() {
        return Ok(out_jar);
    }

    let vanilla_jar = root_dir
        .join("versions")
        .join(mc_version_id)
        .join(format!("{mc_version_id}.jar"));
    if !vanilla_jar.exists() {
        return Err(CoreError::LoaderError(format!(
            "Missing Vanilla Minecraft JAR at '{}' for legacy Forge overlay",
            vanilla_jar.display()
        )));
    }

    on_event(LaunchEvent::Extract {
        message: format!("Patching legacy Forge {full_loader_version} JAR..."),
    });

    let work_dir = loader_dir.join("jar_overlay_tmp");
    let mod_archive = mod_archive_path.to_path_buf();
    let out_jar_clone = out_jar.clone();

    tokio::task::spawn_blocking(move || -> CoreResult<()> {
        if work_dir.exists() {
            let _ = std_fs::remove_dir_all(&work_dir);
        }
        std_fs::create_dir_all(&work_dir)?;

        extract_zip_archive(&vanilla_jar, &work_dir, true)?;
        extract_zip_archive(&mod_archive, &work_dir, false)?;

        // Repack `work_dir` into `out_jar_clone`
        let jar_file = std_fs::File::create(&out_jar_clone)?;
        let mut zip_writer = ZipWriter::new(jar_file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        let mut stack = vec![work_dir.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std_fs::read_dir(&dir)? {
                let entry = entry?;
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(rel) = p.strip_prefix(&work_dir) {
                    let zip_name = rel.to_string_lossy().replace('\\', "/");
                    zip_writer.start_file(zip_name, options)?;
                    let bytes = std_fs::read(&p)?;
                    zip_writer.write_all(&bytes)?;
                }
            }
        }
        zip_writer.finish()?;

        let _ = std_fs::remove_dir_all(&work_dir);
        // Quick sanity check that output JAR opens cleanly
        let verify_f = std_fs::File::open(&out_jar_clone)?;
        let _ = ZipArchive::new(verify_f)?;
        Ok(())
    })
    .await
    .map_err(|e| CoreError::LoaderError(format!("Legacy Forge overlay panicked: {e}")))??;

    Ok(out_jar)
}
