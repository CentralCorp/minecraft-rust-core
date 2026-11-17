use std::path::Path;

use serde::Deserialize;
use tokio::fs;

use crate::bundle::BundleManager;
use crate::config::LaunchOptions;
use crate::downloader::{DownloadTask, Downloader, ExpectedHash};
use crate::error::{CoreError, CoreResult};
use crate::events::EventCallback;
use crate::loader::forge::process_forge_or_neoforge_installer;
use crate::minecraft::arguments::LoaderProfileData;
use crate::utils::hash::sha1_file;
use crate::utils::maven::{compare_maven_versions, MavenArtifact};

const NEOFORGE_LEGACY_API_URL: &str =
    "https://maven.neoforged.net/api/maven/versions/releases/net/neoforged/forge";
const NEOFORGE_MODERN_API_URL: &str =
    "https://maven.neoforged.net/api/maven/versions/releases/net/neoforged/neoforge";
const NEOFORGE_MAVEN_RELEASES_URL: &str = "https://maven.neoforged.net/releases";

const NEOFORGE_MIRRORS: &[&str] = &[
    "https://maven.neoforged.net/releases",
    "https://libraries.minecraft.net",
    "https://maven.minecraftforge.net",
    "https://repo1.maven.org/maven2",
];

#[derive(Debug, Deserialize)]
struct NeoForgeVersionsResponse {
    #[serde(default)]
    versions: Vec<String>,
}

/// Installs NeoForge for both Legacy `1.20.1` (`net.neoforged:forge`) and Modern `1.20.2` – `1.21+`
/// (`net.neoforged:neoforge`), running all installer processors and returning [`LoaderProfileData`].
pub async fn install_neoforge(
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

    let is_legacy_1_20_1 = mc_version_id.trim() == "1.20.1";
    let artifact_name = if is_legacy_1_20_1 {
        "forge"
    } else {
        "neoforge"
    };

    let resolved_version = match resolve_neoforge_version(
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

    let installers_dir = loader_dir.join("installers");
    fs::create_dir_all(&installers_dir).await?;
    let installer_filename = format!("{artifact_name}-{resolved_version}-installer.jar");
    let installer_path = installers_dir.join(&installer_filename);
    let installer_url = format!(
        "{NEOFORGE_MAVEN_RELEASES_URL}/net/neoforged/{artifact_name}/{resolved_version}/{installer_filename}"
    );

    let remote_sha1 = fetch_optional_sha1(downloader.client(), &format!("{installer_url}.sha1")).await;

    let needs_download = if !installer_path.exists() {
        true
    } else if let Some(ref exp_sha1) = remote_sha1 {
        match sha1_file(&installer_path).await {
            Ok(actual) => !actual.eq_ignore_ascii_case(exp_sha1),
            Err(_) => true,
        }
    } else {
        false
    };

    if needs_download {
        let mut task = DownloadTask::new(installer_url, installer_path.clone(), "Loader");
        if let Some(sha1_hex) = remote_sha1 {
            task.expected_hash = Some(ExpectedHash::Sha1(sha1_hex));
        }
        downloader.download_single(&task, on_event).await?;
    }

    let base_coord = MavenArtifact::parse(&format!(
        "net.neoforged:{artifact_name}:{resolved_version}"
    ))
    .ok();

    let profile = process_forge_or_neoforge_installer(
        options,
        mc_version_id,
        &resolved_version,
        &loader_dir,
        &installer_path,
        base_coord.as_ref(),
        NEOFORGE_MIRRORS,
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

/// Computes the NeoForge version prefix corresponding to a Minecraft release version:
/// - `"1.20.2"` -> `Some("20.2.")`
/// - `"1.20.4"` -> `Some("20.4.")`
/// - `"1.21"`   -> `Some("21.0.")`
/// - `"1.21.1"` -> `Some("21.1.")`
/// - `"1.21.4"` -> `Some("21.4.")`
pub fn neoforge_version_prefix(mc_version: &str) -> Option<String> {
    let trimmed = mc_version.trim();
    if trimmed == "1.20.1" {
        return Some("1.20.1-".to_string());
    }

    if let Some(rest) = trimmed.strip_prefix("1.") {
        let parts: Vec<&str> = rest.split('.').collect();
        let minor = parts.first()?.trim();
        if minor.is_empty() || !minor.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let patch = parts
            .get(1)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or("0");
        return Some(format!("{minor}.{patch}."));
    }

    None
}

/// Resolves `"latest"`, `"recommended"`, or an explicit NeoForge build version for `mc_version`.
pub async fn resolve_neoforge_version(
    http: &reqwest::Client,
    mc_version: &str,
    requested_build: &str,
) -> CoreResult<String> {
    let mc_trimmed = mc_version.trim();
    let build = requested_build.trim();
    let is_latest = build.is_empty() || build.eq_ignore_ascii_case("latest");
    let is_recommended = build.eq_ignore_ascii_case("recommended");

    if mc_trimmed == "1.20.1" {
        let all_versions = fetch_neoforge_version_list(
            http,
            NEOFORGE_LEGACY_API_URL,
            &format!("{NEOFORGE_MAVEN_RELEASES_URL}/net/neoforged/forge/maven-metadata.xml"),
        )
        .await
        .unwrap_or_default();

        return select_neoforge_1_20_1_build(&all_versions, build, is_latest, is_recommended);
    }

    let prefix = neoforge_version_prefix(mc_trimmed).ok_or_else(|| {
        CoreError::LoaderError(format!(
            "NeoForge does not support Minecraft version '{mc_trimmed}'"
        ))
    })?;

    if !is_latest && !is_recommended && build.starts_with(&prefix) {
        return Ok(build.to_string());
    }

    let all_versions = fetch_neoforge_version_list(
        http,
        NEOFORGE_MODERN_API_URL,
        &format!("{NEOFORGE_MAVEN_RELEASES_URL}/net/neoforged/neoforge/maven-metadata.xml"),
    )
    .await?;

    select_modern_neoforge_build(&all_versions, mc_trimmed, &prefix, build, is_latest, is_recommended)
}

/// Selects the matching NeoForge 1.20.1 (`net.neoforged:forge`) build version.
pub fn select_neoforge_1_20_1_build(
    all_versions: &[String],
    build: &str,
    is_latest: bool,
    is_recommended: bool,
) -> CoreResult<String> {
    if !is_latest && !is_recommended {
        let with_prefix = format!("1.20.1-{build}");
        if let Some(found) = all_versions
            .iter()
            .find(|v| v.eq_ignore_ascii_case(build) || v.eq_ignore_ascii_case(&with_prefix))
        {
            return Ok(found.clone());
        }
        if build.starts_with("1.20.1-") || build.starts_with("47.1.") {
            return Ok(build.to_string());
        }
        return Ok(with_prefix);
    }

    let mut matching: Vec<String> = all_versions
        .iter()
        .filter(|v| v.starts_with("1.20.1-") || v.starts_with("47.1."))
        .cloned()
        .collect();

    if matching.is_empty() {
        return Err(CoreError::LoaderError(
            "No NeoForge builds found for Minecraft 1.20.1".to_string(),
        ));
    }

    matching.sort_by(|a, b| {
        let norm_a = a.strip_prefix("1.20.1-").unwrap_or(a);
        let norm_b = b.strip_prefix("1.20.1-").unwrap_or(b);
        compare_maven_versions(norm_a, norm_b)
    });

    if is_recommended {
        if let Some(stable) = matching
            .iter()
            .rev()
            .find(|v| !v.to_ascii_lowercase().contains("beta"))
        {
            return Ok(stable.clone());
        }
    }

    Ok(matching.pop().unwrap())
}

/// Selects the matching modern NeoForge (`1.20.2` – `1.21+`, `net.neoforged:neoforge`) build version.
pub fn select_modern_neoforge_build(
    all_versions: &[String],
    mc_version: &str,
    prefix: &str,
    build: &str,
    is_latest: bool,
    is_recommended: bool,
) -> CoreResult<String> {
    let mut matching: Vec<String> = all_versions
        .iter()
        .filter(|v| v.starts_with(prefix))
        .cloned()
        .collect();

    if !is_latest && !is_recommended {
        if let Some(exact) = all_versions.iter().find(|v| v.eq_ignore_ascii_case(build)) {
            return Ok(exact.clone());
        }
        let prefixed = format!("{prefix}{build}");
        if let Some(found) = matching.iter().find(|v| v.eq_ignore_ascii_case(&prefixed)) {
            return Ok(found.clone());
        }
        return Ok(build.to_string());
    }

    if matching.is_empty() {
        return Err(CoreError::LoaderError(format!(
            "No NeoForge builds found for Minecraft {mc_version} (prefix '{prefix}*')"
        )));
    }

    matching.sort_by(|a, b| compare_maven_versions(a, b));

    if is_recommended {
        if let Some(stable) = matching.iter().rev().find(|v| {
            let lower = v.to_ascii_lowercase();
            !lower.contains("beta") && !lower.contains("alpha")
        }) {
            return Ok(stable.clone());
        }
    }

    Ok(matching.pop().unwrap())
}

async fn fetch_neoforge_version_list(
    http: &reqwest::Client,
    rest_api_url: &str,
    maven_xml_url: &str,
) -> CoreResult<Vec<String>> {
    if let Ok(resp) = http.get(rest_api_url).send().await {
        if resp.status().is_success() {
            if let Ok(parsed) = resp.json::<NeoForgeVersionsResponse>().await {
                if !parsed.versions.is_empty() {
                    return Ok(parsed.versions);
                }
            }
        }
    }

    let resp = http.get(maven_xml_url).send().await?;
    if !resp.status().is_success() {
        return Err(CoreError::DownloadFailed {
            url: maven_xml_url.to_string(),
            status: Some(resp.status().as_u16()),
            message: "Failed to fetch NeoForge Maven metadata".to_string(),
        });
    }

    let xml = resp.text().await?;
    let versions = parse_maven_metadata_xml_versions(&xml);
    if versions.is_empty() {
        return Err(CoreError::LoaderError(
            "NeoForge Maven metadata contained no versions".to_string(),
        ));
    }

    Ok(versions)
}

fn parse_maven_metadata_xml_versions(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<version>") {
        let after = &rest[start + "<version>".len()..];
        if let Some(end) = after.find("</version>") {
            let ver = after[..end].trim();
            if !ver.is_empty() {
                out.push(ver.to_string());
            }
            rest = &after[end + "</version>".len()..];
        } else {
            break;
        }
    }
    out
}

async fn fetch_optional_sha1(http: &reqwest::Client, url: &str) -> Option<String> {
    let resp = http.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    let token = body.split_whitespace().next()?.trim();
    if token.len() == 40 && token.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(token.to_string())
    } else {
        None
    }
}
