use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::bundle::BundleItem;
use crate::config::LaunchOptions;
use crate::downloader::{DownloadTask, Downloader, ExpectedHash};
use crate::error::{CoreError, CoreResult};
use crate::events::{EventCallback, LaunchEvent};
use crate::utils::os_rules::{TargetArch, TargetOs};
use crate::utils::zip_extract::{
    ensure_executable, extract_tar_gz_archive, extract_zip_archive, sanitize_archive_entry_path,
};

const MOJANG_JAVA_MANIFEST_URL: &str =
    "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";
const AZUL_ZULU_API_URL: &str = "https://api.azul.com/metadata/v1/zulu/packages/";
const CACHED_JRE_MANIFEST_FILENAME: &str = ".jre_manifest.json";

#[derive(Debug, Serialize, Deserialize)]
struct CachedMojangJreManifest {
    #[serde(default)]
    component: String,
    #[serde(default)]
    version_name: String,
    manifest: serde_json::Value,
}

/// Result of Java runtime resolution.
#[derive(Debug, Clone)]
pub struct ResolvedJava {
    /// Path to the `java` (or `java.exe` / `javaw.exe`) binary.
    pub binary_path: PathBuf,
    /// Individual Mojang runtime manifest files to verify/download via [`crate::bundle::BundleManager`],
    /// if resolved via Mojang's curated manifest.
    pub bundle_files: Vec<BundleItem>,
}

/// Parses a Java version string such as `"8"`, `"1.8"`, `"1.8.0_402"`, `"17.0.10"`, or
/// `"21.0.3+9-LTS"` into its canonical major version (`8`, `17`, `21`, ...).
pub fn parse_java_major_version(raw: &str) -> Option<u32> {
    let trimmed = raw.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    if trimmed.is_empty() {
        return None;
    }

    let cleaned = trimmed
        .strip_prefix("java-")
        .or_else(|| trimmed.strip_prefix("jdk-"))
        .or_else(|| trimmed.strip_prefix("jre-"))
        .unwrap_or(trimmed);

    if let Some(after_one) = cleaned.strip_prefix("1.") {
        let digits: String = after_one
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        let major = digits.parse::<u32>().ok()?;
        return (major >= 5).then_some(major);
    }

    let digits: String = cleaned.chars().take_while(|c| c.is_ascii_digit()).collect();
    let major = digits.parse::<u32>().ok()?;
    (major >= 5).then_some(major)
}

/// Resolves or downloads an appropriate Java runtime for the target Minecraft version:
/// 1. Uses explicit `options.java.path` if configured (validating system Java major version if
///    `options.java.version` is also set).
/// 2. Queries Mojang's official Java Runtime manifest (`windows-x64/x86/arm64`,
///    `mac-os/mac-os-arm64` respecting `intel_enabled_mac`, `linux/linux-i386`) when no custom
///    Java major version override is requested, caching `<runtime_base>/.jre_manifest.json` so
///    warm launches make 0 Mojang JRE HTTP requests.
/// 3. Falls back to Azul Zulu's Metadata API (`.zip` / `.tar.gz` extraction, automatic `jre` -> `jdk`
///    fallback, macOS `.jre/Contents/Home/bin/java` resolution, and Unix `0o755` executable bits).
pub async fn resolve_java_runtime(
    options: &LaunchOptions,
    mc_version_json: &serde_json::Value,
    downloader: &Downloader,
    on_event: &EventCallback,
) -> CoreResult<ResolvedJava> {
    let forced_major_str = options
        .java
        .version
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let forced_major = forced_major_str.and_then(parse_java_major_version);

    // 1. Explicit custom Java path (or `"system"` when `embedded_java == false`)
    if let Some(ref custom_path) = options.java.path {
        if custom_path == Path::new("system") {
            if let Some(sys_bin) = locate_system_java_binary(forced_major) {
                return Ok(ResolvedJava {
                    binary_path: sys_bin,
                    bundle_files: Vec::new(),
                });
            }
            // Host has no matching system Java on JAVA_HOME / PATH -> fall back seamlessly to embedded JRE
        } else {
            let resolved_bin = locate_java_binary_in_custom_path(custom_path)?;
            return Ok(ResolvedJava {
                binary_path: resolved_bin,
                bundle_files: Vec::new(),
            });
        }
    }

    let manifest_major = mc_version_json
        .get("javaVersion")
        .and_then(|jv| jv.get("majorVersion"))
        .and_then(|m| m.as_u64())
        .map(|m| m as u32);

    // 2. Mojang official curated Java Runtime Manifest (when no forced version is set)
    if forced_major_str.is_none() {
        let component = mc_version_json
            .get("javaVersion")
            .and_then(|jv| jv.get("component"))
            .and_then(|c| c.as_str())
            .unwrap_or("jre-legacy");

        let expected_major = manifest_major.or_else(|| infer_mojang_component_major(component));

        if let Some(arch_os_key) = mojang_runtime_platform_key(
            TargetOs::current(),
            TargetArch::current(),
            options.intel_enabled_mac,
        ) {
            match try_resolve_mojang_jre(
                &options.path,
                arch_os_key,
                component,
                downloader.client(),
            )
            .await
            {
                Ok(Some(resolved)) => return Ok(resolved),
                Ok(None) | Err(_) => {
                    // Offline check: reuse an already-installed Mojang JRE matching component / major version
                    if let Some(local_bin) = find_cached_mojang_jre(
                        &options.path,
                        arch_os_key,
                        Some(component),
                        expected_major,
                    ) {
                        return Ok(ResolvedJava {
                            binary_path: local_bin,
                            bundle_files: Vec::new(),
                        });
                    }
                }
            }
        }
    }

    // 3. Fallback to Azul Zulu API
    let major_version = forced_major.or(manifest_major).unwrap_or(8);

    resolve_azul_zulu_jre(options, major_version, downloader, on_event).await
}

/// Maps `(TargetOs, TargetArch, intel_enabled_mac)` to Mojang's Java Runtime manifest platform key.
pub fn mojang_runtime_platform_key(
    os: TargetOs,
    arch: TargetArch,
    intel_enabled_mac: bool,
) -> Option<&'static str> {
    match (os, arch) {
        (TargetOs::Windows, TargetArch::X86_64) => Some("windows-x64"),
        (TargetOs::Windows, TargetArch::X86) => Some("windows-x86"),
        (TargetOs::Windows, TargetArch::Arm64) => Some("windows-arm64"),
        (TargetOs::MacOs, TargetArch::X86_64) => Some("mac-os"),
        (TargetOs::MacOs, TargetArch::Arm64) => {
            if intel_enabled_mac {
                Some("mac-os")
            } else {
                Some("mac-os-arm64")
            }
        }
        (TargetOs::Linux, TargetArch::X86_64) => Some("linux"),
        (TargetOs::Linux, TargetArch::X86) => Some("linux-i386"),
        _ => None,
    }
}

fn infer_mojang_component_major(component: &str) -> Option<u32> {
    match component.trim() {
        "jre-legacy" => Some(8),
        "java-runtime-alpha" => Some(16),
        "java-runtime-beta" | "java-runtime-gamma" | "java-runtime-gamma-snapshot" => Some(17),
        "java-runtime-delta" => Some(21),
        _ => None,
    }
}

async fn try_resolve_mojang_jre(
    root_path: &Path,
    arch_os: &str,
    component: &str,
    http: &reqwest::Client,
) -> CoreResult<Option<ResolvedJava>> {
    let safe_component = component.replace(['/', '\\', ':', ' '], "_");
    let runtime_base = root_path
        .join("runtime")
        .join(format!("jre-{safe_component}-{arch_os}"));
    let cached_manifest_path = runtime_base.join(CACHED_JRE_MANIFEST_FILENAME);

    // Zero-HTTP warm fast-path: reconstruct `ResolvedJava` from `<runtime_base>/.jre_manifest.json`
    // when already cached on disk (BundleManager + FileHashCache still verify all JRE files!).
    if cached_manifest_path.is_file() {
        if let Ok(raw_bytes) = fs::read(&cached_manifest_path).await {
            let manifest_val_opt = serde_json::from_slice::<CachedMojangJreManifest>(&raw_bytes)
                .map(|w| w.manifest)
                .ok()
                .or_else(|| serde_json::from_slice::<serde_json::Value>(&raw_bytes).ok());

            if let Some(manifest_json) = manifest_val_opt {
                if let Some(resolved) =
                    build_resolved_java_from_mojang_manifest(&runtime_base, &manifest_json).await
                {
                    return Ok(Some(resolved));
                }
            }
        }
    }

    let all_json: serde_json::Value = http
        .get(MOJANG_JAVA_MANIFEST_URL)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let Some(entry) = all_json
        .get(arch_os)
        .and_then(|plat| plat.get(component))
        .and_then(|arr| arr.as_array())
        .and_then(|arr| arr.first())
    else {
        return Ok(None);
    };

    let version_name = entry
        .get("version")
        .and_then(|v| v.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or(component)
        .to_string();

    let Some(manifest_url) = entry
        .get("manifest")
        .and_then(|m| m.get("url"))
        .and_then(|u| u.as_str())
    else {
        return Ok(None);
    };

    let manifest_json: serde_json::Value = http
        .get(manifest_url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let Some(resolved) =
        build_resolved_java_from_mojang_manifest(&runtime_base, &manifest_json).await
    else {
        return Ok(None);
    };

    // Persist `.jre_manifest.json` atomically so subsequent warm launches skip both Mojang JRE HTTP requests
    let _ = fs::create_dir_all(&runtime_base).await;
    let cached_payload = CachedMojangJreManifest {
        component: component.to_string(),
        version_name,
        manifest: manifest_json,
    };
    if let Ok(serialized) = serde_json::to_vec(&cached_payload) {
        let tmp_path = runtime_base.join(format!(
            ".jre_manifest.json.{}.tmp",
            std::process::id()
        ));
        if fs::write(&tmp_path, &serialized).await.is_ok() {
            crate::utils::hash_cache::clear_readonly_if_exists(&cached_manifest_path);
            let _ = fs::rename(&tmp_path, &cached_manifest_path).await;
        } else {
            let _ = fs::remove_file(&tmp_path).await;
        }
    }

    Ok(Some(resolved))
}

async fn build_resolved_java_from_mojang_manifest(
    runtime_base: &Path,
    manifest_json: &serde_json::Value,
) -> Option<ResolvedJava> {
    let files_map = manifest_json.get("files").and_then(|f| f.as_object())?;

    // Identify prefix before `bin/java` or `bin/java.exe` / `bin/javaw.exe` (e.g. `jre.bundle/Contents/Home/` on macOS)
    let exe_candidates: &[&str] = if cfg!(windows) {
        &["bin/java.exe", "bin/javaw.exe"]
    } else {
        &["bin/java"]
    };

    let mut strip_prefix = String::new();
    let mut chosen_exe_rel: Option<String> = None;

    'outer: for exe_suffix in exe_candidates {
        for key in files_map.keys() {
            let norm = key.replace('\\', "/");
            if norm.ends_with(exe_suffix) {
                strip_prefix = norm[..norm.len() - exe_suffix.len()].to_string();
                chosen_exe_rel = Some((*exe_suffix).to_string());
                break 'outer;
            }
        }
    }

    let exe_rel = chosen_exe_rel?;

    let mut bundle_files = Vec::with_capacity(files_map.len());
    let mut dir_paths: Vec<PathBuf> = Vec::new();
    #[allow(unused_mut)]
    let mut pending_symlinks: Vec<(PathBuf, String)> = Vec::new();

    for (raw_rel, info) in files_map {
        let norm_rel = raw_rel.replace('\\', "/");
        let stripped = norm_rel
            .strip_prefix(&strip_prefix)
            .unwrap_or(&norm_rel);
        let Some(clean_rel) = sanitize_archive_entry_path(stripped) else {
            continue;
        };

        let entry_type = info.get("type").and_then(|t| t.as_str()).unwrap_or("file");
        let dest = runtime_base.join(&clean_rel);

        match entry_type {
            "directory" => {
                dir_paths.push(dest);
            }
            "link" => {
                if let Some(target) = info.get("target").and_then(|t| t.as_str()) {
                    pending_symlinks.push((dest, target.to_string()));
                }
            }
            _ => {
                let Some(raw_dl) = info.get("downloads").and_then(|d| d.get("raw")) else {
                    continue;
                };
                let Some(url) = raw_dl.get("url").and_then(|u| u.as_str()) else {
                    continue;
                };
                let sha1 = raw_dl
                    .get("sha1")
                    .and_then(|s| s.as_str())
                    .map(str::to_string);
                let size = raw_dl.get("size").and_then(|s| s.as_u64());
                let executable = info
                    .get("executable")
                    .and_then(|e| e.as_bool())
                    .unwrap_or(false)
                    || stripped.starts_with("bin/")
                    || stripped.ends_with("jspawnhelper");

                bundle_files.push(BundleItem {
                    path: dest,
                    url: Some(url.to_string()),
                    sha1,
                    size,
                    category: "Java".to_string(),
                    inline_content: None,
                    executable,
                    package_id: None,
                });
            }
        }
    }

    if !dir_paths.is_empty() || !pending_symlinks.is_empty() {
        let _ = tokio::task::spawn_blocking(move || {
            for d in dir_paths {
                if !d.exists() {
                    let _ = std::fs::create_dir_all(&d);
                }
            }
            #[cfg(unix)]
            for (link_path, target_str) in pending_symlinks {
                let target_pb = PathBuf::from(&target_str);
                if !target_pb.is_absolute() {
                    if let Ok(existing_target) = std::fs::read_link(&link_path) {
                        if existing_target == target_pb {
                            continue;
                        }
                    }
                    if let Some(parent) = link_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::remove_file(&link_path);
                    let _ = std::os::unix::fs::symlink(&target_pb, &link_path);
                }
            }
        })
        .await;
    }

    Some(ResolvedJava {
        binary_path: runtime_base.join(exe_rel),
        bundle_files,
    })
}

#[derive(Debug, Deserialize)]
struct ZuluPackage {
    name: String,
    download_url: String,
    #[serde(default)]
    sha256_hash: Option<String>,
}

async fn resolve_azul_zulu_jre(
    options: &LaunchOptions,
    major_version: u32,
    downloader: &Downloader,
    on_event: &EventCallback,
) -> CoreResult<ResolvedJava> {
    let os = TargetOs::current();
    let arch = if os == TargetOs::MacOs
        && TargetArch::current() == TargetArch::Arm64
        && options.intel_enabled_mac
    {
        TargetArch::X86_64
    } else {
        TargetArch::current()
    };

    let zulu_os = match os {
        TargetOs::Windows => "windows",
        TargetOs::MacOs => "macos",
        TargetOs::Linux => "linux",
    };
    let zulu_arch = match arch {
        TargetArch::X86_64 => "x64",
        TargetArch::X86 => "x86",
        TargetArch::Arm64 => "aarch64",
        TargetArch::Arm32 => "arm",
    };

    let runtime_dir = options
        .path
        .join("runtime")
        .join(format!("zulu-{major_version}-{zulu_os}-{zulu_arch}"));

    if let Some(existing_bin) = find_java_binary_in_dir(&runtime_dir) {
        ensure_executable(&existing_bin)?;
        return Ok(ResolvedJava {
            binary_path: existing_bin,
            bundle_files: Vec::new(),
        });
    }

    let archive_types: &[&str] = match os {
        TargetOs::Windows => &["zip"],
        TargetOs::MacOs => &["tar.gz", "zip"],
        TargetOs::Linux => &["tar.gz", "zip"],
    };

    let requested_pkg_type = if options.java.image_type.trim().is_empty() {
        "jre"
    } else {
        options.java.image_type.trim()
    };

    // If `jre` returns no builds on Azul's API (common on certain arch/version combinations),
    // automatically fall back to `jdk`!
    let pkg_types_to_try: Vec<&str> = if requested_pkg_type.eq_ignore_ascii_case("jre") {
        vec!["jre", "jdk"]
    } else {
        vec![requested_pkg_type]
    };

    let mut chosen_pkg: Option<(ZuluPackage, &'static str)> = None;
    'pkg_loop: for pkg_type in &pkg_types_to_try {
        for &archive_ext in archive_types {
            let query_url = format!(
                "{AZUL_ZULU_API_URL}?java_version={major_version}&os={zulu_os}&arch={zulu_arch}&archive_type={archive_ext}&java_package_type={pkg_type}&latest=true&release_status=ga&availability_types=CA"
            );
            let resp = downloader.client().get(&query_url).send().await?;
            if !resp.status().is_success() {
                continue;
            }
            let pkgs: Vec<ZuluPackage> = resp.json().await.unwrap_or_default();
            if let Some(first) = pkgs.into_iter().next() {
                chosen_pkg = Some((first, archive_ext));
                break 'pkg_loop;
            }
        }
    }

    let Some((pkg, archive_ext)) = chosen_pkg else {
        return Err(CoreError::JavaNotFound(format!(
            "No Azul Zulu {requested_pkg_type} (or jdk fallback) package found for Java {major_version} on {zulu_os}/{zulu_arch}"
        )));
    };

    fs::create_dir_all(&runtime_dir).await?;
    let archive_path = runtime_dir.join(&pkg.name);

    if !archive_path.exists() {
        let task = DownloadTask {
            url: pkg.download_url.clone(),
            destination: archive_path.clone(),
            size: None,
            expected_hash: None,
            category: pkg.name.clone(),
            executable: false,
        };
        downloader.download_single(&task, on_event).await?;
    }

    if let Some(expected_sha256) = pkg.sha256_hash.as_ref().filter(|s| !s.trim().is_empty()) {
        let actual = crate::utils::hash::sha256_file(&archive_path).await?;
        if !actual.eq_ignore_ascii_case(expected_sha256.trim()) {
            let _ = fs::remove_file(&archive_path).await;
            return Err(CoreError::HashMismatch {
                path: archive_path.display().to_string(),
                expected: expected_sha256.clone(),
                actual,
            });
        }
    }

    on_event(LaunchEvent::Extract {
        message: format!("Extracting Java runtime {}...", pkg.name),
    });

    let archive_clone = archive_path.clone();
    let dest_clone = runtime_dir.clone();
    let is_targz = archive_ext == "tar.gz"
        || pkg.name.ends_with(".tar.gz")
        || pkg.name.ends_with(".tgz");

    tokio::task::spawn_blocking(move || {
        if is_targz {
            extract_tar_gz_archive(&archive_clone, &dest_clone)
        } else {
            extract_zip_archive(&archive_clone, &dest_clone, true)
        }
    })
    .await
    .map_err(|e| CoreError::JavaNotFound(format!("Java extraction task panicked: {e}")))??;

    let _ = fs::remove_file(&archive_path).await;

    // Preserve ExpectedHash import used in tests/callers
    let _ = ExpectedHash::Sha1;

    let java_bin = find_java_binary_in_dir(&runtime_dir).ok_or_else(|| {
        CoreError::JavaNotFound(format!(
            "Extracted Azul Zulu archive into '{}', but could not locate bin/java executable",
            runtime_dir.display()
        ))
    })?;

    ensure_executable(&java_bin)?;

    Ok(ResolvedJava {
        binary_path: java_bin,
        bundle_files: Vec::new(),
    })
}

fn locate_java_binary_in_custom_path(custom: &Path) -> CoreResult<PathBuf> {
    if custom.is_file() {
        ensure_executable(custom)?;
        return Ok(custom.to_path_buf());
    }

    let bin_names: &[&str] = if cfg!(windows) {
        &["java.exe", "javaw.exe"]
    } else {
        &["java"]
    };

    for name in bin_names {
        let direct = custom.join(name);
        if direct.is_file() {
            ensure_executable(&direct)?;
            return Ok(direct);
        }
        let in_bin = custom.join("bin").join(name);
        if in_bin.is_file() {
            ensure_executable(&in_bin)?;
            return Ok(in_bin);
        }
        let macos_home = custom.join("Contents").join("Home").join("bin").join(name);
        if macos_home.is_file() {
            ensure_executable(&macos_home)?;
            return Ok(macos_home);
        }
    }

    // If path hasn't been created yet or is a system command name like `"java"`, allow it through
    if custom.components().count() == 1 {
        return Ok(custom.to_path_buf());
    }

    Err(CoreError::JavaNotFound(format!(
        "Custom Java binary not found at '{}'",
        custom.display()
    )))
}

fn locate_system_java_binary(required_major: Option<u32>) -> Option<PathBuf> {
    let matches_major = |candidate: &Path| -> bool {
        match required_major {
            None => true,
            Some(req) => detect_java_binary_major_version(candidate) == Some(req),
        }
    };

    if let Some(java_home) = std::env::var_os("JAVA_HOME") {
        let home_path = PathBuf::from(java_home);
        if let Some(bin) = find_java_binary_in_dir(&home_path) {
            if matches_major(&bin) {
                let _ = ensure_executable(&bin);
                return Some(bin);
            }
        }
    }

    // Prefer `java.exe` consistently so child stdout/stderr log pipes are never suppressed on Windows!
    let bin_names: &[&str] = if cfg!(windows) {
        &["java.exe", "javaw.exe"]
    } else {
        &["java"]
    };

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for exe in bin_names {
                let candidate = dir.join(exe);
                if candidate.is_file() && matches_major(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }

    None
}

fn detect_java_binary_major_version(java_bin: &Path) -> Option<u32> {
    if let Some(home) = java_bin.parent().and_then(|p| p.parent()) {
        if let Some(maj) = read_release_file_major(home) {
            return Some(maj);
        }
    }
    if let Ok(canon) = std::fs::canonicalize(java_bin) {
        if let Some(home) = canon.parent().and_then(|p| p.parent()) {
            if let Some(maj) = read_release_file_major(home) {
                return Some(maj);
            }
        }
    }

    let mut cmd = Command::new(java_bin);
    cmd.arg("-version");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = cmd.output().ok()?;
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    for line in combined.lines() {
        if let Some((_, after_quote)) = line.split_once('"') {
            if let Some((ver_token, _)) = after_quote.split_once('"') {
                if let Some(maj) = parse_java_major_version(ver_token) {
                    return Some(maj);
                }
            }
        }
        for word in line.split_whitespace() {
            if word.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                if let Some(maj) = parse_java_major_version(word) {
                    return Some(maj);
                }
            }
        }
    }
    None
}

fn read_release_file_major(java_home: &Path) -> Option<u32> {
    for candidate in [
        java_home.join("release"),
        java_home.join("Contents").join("Home").join("release"),
    ] {
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            for line in text.lines() {
                if let Some(val) = line.trim().strip_prefix("JAVA_VERSION=") {
                    if let Some(maj) = parse_java_major_version(val) {
                        return Some(maj);
                    }
                }
            }
        }
    }
    None
}

/// Locates an already-installed Mojang JRE under `<root>/runtime/` when offline, prioritizing:
/// 1. Exact component directory (`runtime/jre-{component}-{arch_os}`),
/// 2. Any `runtime/jre-*-{arch_os}` matching `expected_major` (via `<dir>/release` `JAVA_VERSION`
///    or version directory name), so multi-JRE installs never pick the wrong Java major version!
pub fn find_cached_mojang_jre(
    root_path: &Path,
    arch_os: &str,
    component: Option<&str>,
    expected_major: Option<u32>,
) -> Option<PathBuf> {
    let runtime_root = root_path.join("runtime");
    if !runtime_root.is_dir() {
        return None;
    }

    if let Some(comp) = component.map(str::trim).filter(|s| !s.is_empty()) {
        let safe_comp = comp.replace(['/', '\\', ':', ' '], "_");
        let exact_dir = runtime_root.join(format!("jre-{safe_comp}-{arch_os}"));
        if let Some(bin) = find_java_binary_in_dir(&exact_dir) {
            return Some(bin);
        }
    }

    let entries = std::fs::read_dir(&runtime_root).ok()?;
    let suffix = format!("-{arch_os}");
    let mut fallback_without_major: Option<PathBuf> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("jre-") || !name.ends_with(&suffix) {
            continue;
        }
        let Some(bin) = find_java_binary_in_dir(&path) else {
            continue;
        };

        match expected_major {
            Some(exp_maj) => {
                let dir_major = read_release_file_major(&path)
                    .or_else(|| {
                        bin.parent()
                            .and_then(|p| p.parent())
                            .and_then(read_release_file_major)
                    })
                    .or_else(|| {
                        let inner = name
                            .strip_prefix("jre-")
                            .and_then(|s| s.strip_suffix(&suffix))?;
                        infer_mojang_component_major(inner).or_else(|| parse_java_major_version(inner))
                    });

                if dir_major == Some(exp_maj) {
                    return Some(bin);
                }
            }
            None => {
                if fallback_without_major.is_none() {
                    fallback_without_major = Some(bin);
                }
            }
        }
    }

    fallback_without_major
}

/// Searches up to 4 directory levels under `root` for `bin/java` (`bin/java.exe` / `bin/javaw.exe`
/// on Windows) or macOS `.jre/Contents/Home/bin/java` (including text-file pseudo-symlinks in `.zip` distributions).
pub fn find_java_binary_in_dir(root: &Path) -> Option<PathBuf> {
    if !root.exists() {
        return None;
    }

    let exe_names: &[&str] = if cfg!(windows) {
        &["java.exe", "javaw.exe"]
    } else {
        &["java"]
    };

    for exe in exe_names {
        let direct = root.join("bin").join(exe);
        if direct.is_file() {
            return Some(direct);
        }
        let contents_home = root.join("Contents").join("Home").join("bin").join(exe);
        if contents_home.is_file() {
            return Some(contents_home);
        }
    }

    let read_dir = std::fs::read_dir(root).ok()?;
    for entry in read_dir.flatten() {
        let sub = entry.path();
        if !sub.is_dir() {
            continue;
        }

        // Check macOS Zulu `.zip` text-file symlink `bin` -> `zulu-XX.jre/Contents/Home/bin`
        let bin_candidate = sub.join("bin");
        if bin_candidate.is_file() {
            if let Ok(link_rel) = std::fs::read_to_string(&bin_candidate) {
                let target_bin = sub.join(link_rel.trim()).join("java");
                if target_bin.is_file() {
                    return Some(target_bin);
                }
            }
        }

        for exe in exe_names {
            let p1 = sub.join("bin").join(exe);
            if p1.is_file() {
                return Some(p1);
            }
            let p2 = sub.join("Contents").join("Home").join("bin").join(exe);
            if p2.is_file() {
                return Some(p2);
            }

            if let Ok(sub_entries) = std::fs::read_dir(&sub) {
                for sub_entry in sub_entries.flatten() {
                    let p3 = sub_entry
                        .path()
                        .join("Contents")
                        .join("Home")
                        .join("bin")
                        .join(exe);
                    if p3.is_file() {
                        return Some(p3);
                    }
                }
            }
        }
    }

    None
}
